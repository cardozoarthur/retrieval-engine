//! Local trusted-host protocol. Session ids are identifiers, not authorization.
use crate::document::error;
use crate::{
    ChunkConfig, Document, Error, Evidence, EvidenceLimits, EvidenceSession, IndexLimits,
    LexicalIndex, Limits, RequestControl, Source, Unit,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_REQUEST_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Literal,
    Bm25,
    Hybrid,
    HybridPrefilter,
}

#[cfg(test)]
mod ttl_tests {
    use super::*;
    fn insert(
        worker: &mut RetrievalWorker,
        name: &str,
        at: Instant,
    ) -> (String, std::sync::Weak<Document>) {
        let doc = Arc::new(
            Document::from_bytes(
                name,
                b"texto fonte sem persistencia",
                "text/plain",
                &Limits::default(),
            )
            .unwrap(),
        );
        let weak = Arc::downgrade(&doc);
        let chars = doc.text().chars().count();
        let evidence = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
        let id = evidence.id().to_owned();
        let index =
            LexicalIndex::build(vec![doc], ChunkConfig::default(), IndexLimits::default()).unwrap();
        worker.sessions.insert(
            id.clone(),
            SessionState {
                evidence,
                index: Some(index),
                chars,
                last_activity: at,
            },
        );
        worker.total_chars += chars;
        (id, weak)
    }
    #[test]
    fn ttl_drops_source_and_index_at_boundary_and_preserves_other_session() {
        let mut worker = RetrievalWorker::new(WorkerConfig {
            session_ttl: Duration::from_secs(10),
            ..Default::default()
        })
        .unwrap();
        let now = Instant::now();
        let (expired, weak) = insert(&mut worker, "expired", now);
        let (live, live_weak) = insert(&mut worker, "live", now + Duration::from_secs(5));
        worker.expire_at(now + Duration::from_secs(10), true);
        assert!(
            weak.upgrade().is_none(),
            "both source and index must release their Arc"
        );
        assert!(live_weak.upgrade().is_some());
        assert!(!worker.sessions.contains_key(&expired));
        assert!(worker.sessions.contains_key(&live));
        assert_eq!(worker.total_chars, worker.sessions[&live].chars);
        assert_eq!(worker.idle_expired_sessions, 1);
        assert_eq!(worker.expired_sessions, 1);
        worker.expire_at(now + Duration::from_secs(15), false);
        assert_eq!(worker.total_chars, 0);
        assert!(live_weak.upgrade().is_none());
        assert_eq!(worker.expired_sessions, 2);
        assert_eq!(worker.idle_expired_sessions, 1);
    }
    #[test]
    fn admitted_activity_renews_but_malformed_request_does_not() {
        let mut worker = RetrievalWorker::new(WorkerConfig {
            session_ttl: Duration::from_secs(10),
            ..Default::default()
        })
        .unwrap();
        let old = Instant::now() - Duration::from_secs(9);
        let (id, _) = insert(&mut worker, "active", old);
        assert!(worker.handle_json(serde_json::to_string(&json!({"schema_version":1,"id":"q","op":"search","session_id":id,"query":"fonte","mode":"literal","top_k":1})).unwrap().as_bytes()).ok);
        let renewed = worker.sessions[&id].last_activity;
        assert!(renewed > old);
        assert!(!worker.handle_json(b"not-json").ok);
        assert_eq!(worker.sessions[&id].last_activity, renewed);
        worker.expire_at(renewed + Duration::from_secs(9), false);
        assert!(worker.sessions.contains_key(&id));
        worker.expire_at(renewed + Duration::from_secs(10), false);
        assert!(!worker.sessions.contains_key(&id));
        assert!(RetrievalWorker::new(WorkerConfig {
            session_ttl: Duration::ZERO,
            ..Default::default()
        })
        .is_err());
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSpec {
    Local { path: String },
    Remote { url: String },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Open {
        source: SourceSpec,
    },
    StartOpen {
        source: SourceSpec,
    },
    PollOpen {
        task_id: String,
        #[serde(default = "default_poll_events")]
        max_events: usize,
    },
    PeekOpen {
        task_id: String,
        query: String,
        #[serde(default = "default_top_k")]
        top_k: usize,
    },
    ExpandOpen {
        task_id: String,
        reference: String,
        before: usize,
        after: usize,
        unit: Unit,
    },
    FinishOpen {
        task_id: String,
        #[serde(default)]
        references: Vec<String>,
    },
    CancelOpen {
        task_id: String,
    },
    TaskStatus {
        task_id: String,
    },
    Search {
        session_id: String,
        query: String,
        mode: SearchMode,
        #[serde(default = "default_top_k")]
        top_k: usize,
    },
    Expand {
        session_id: String,
        reference: String,
        before: usize,
        after: usize,
        unit: Unit,
    },
    Pack {
        session_id: String,
        references: Vec<String>,
    },
    Close {
        session_id: String,
    },
    Cancel {
        request_id: String,
    },
    RequestStatus {
        request_id: String,
    },
    Capabilities,
}
fn default_top_k() -> usize {
    10
}
fn default_poll_events() -> usize {
    32
}

#[derive(Debug, Serialize)]
pub struct WorkerRequest {
    pub schema_version: u32,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(flatten)]
    pub operation: Operation,
}
#[derive(Debug, Serialize)]
pub struct WorkerFault {
    pub code: &'static str,
    pub message: &'static str,
    #[serde(skip)]
    pub(crate) correlation_id: Option<String>,
}
pub(crate) fn fault(code: &'static str, message: &'static str) -> WorkerFault {
    WorkerFault {
        code,
        message,
        correlation_id: None,
    }
}
impl std::fmt::Display for WorkerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for WorkerFault {}

#[derive(Debug, Serialize)]
pub struct WorkerResponse {
    pub schema_version: u32,
    pub id: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkerFault>,
}

pub fn parse_retrieval_request(bytes: &[u8]) -> Result<WorkerRequest, WorkerFault> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(fault("resource_limit", "Request exceeds 128KiB"));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| fault("invalid_request", "Invalid request JSON"))?;
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| fault("invalid_request", "Request must be an object"))?;
    let id = object
        .remove("id")
        .and_then(|v| v.as_str().map(String::from))
        .filter(|id| !id.is_empty() && id.len() <= 128);
    let timeout_value = object.remove("timeout_ms");
    let timeout_ms = timeout_value
        .as_ref()
        .and_then(Value::as_u64)
        .filter(|v| (1..=300_000).contains(v));
    let parse = (|| {
        if id.is_none() || object.remove("schema_version").and_then(|v| v.as_u64()) != Some(1) {
            return Err(fault(
                "invalid_request",
                "Expected schema_version 1 and bounded id",
            ));
        }
        if timeout_value.is_some() && timeout_ms.is_none() {
            return Err(fault("invalid_request", "Invalid request timeout"));
        }
        let allowed: &[&str] = match object.get("op").and_then(Value::as_str) {
            Some("open") => &["op", "source"],
            Some("start_open") => &["op", "source"],
            Some("poll_open") => &["op", "task_id", "max_events"],
            Some("peek_open") => &["op", "task_id", "query", "top_k"],
            Some("expand_open") => &["op", "task_id", "reference", "before", "after", "unit"],
            Some("finish_open") => &["op", "task_id", "references"],
            Some("cancel_open") | Some("task_status") => &["op", "task_id"],
            Some("search") => &["op", "session_id", "query", "mode", "top_k"],
            Some("expand") => &["op", "session_id", "reference", "before", "after", "unit"],
            Some("pack") => &["op", "session_id", "references"],
            Some("close") => &["op", "session_id"],
            Some("capabilities") => &["op"],
            Some("cancel") => &["op", "request_id"],
            Some("request_status") => &["op", "request_id"],
            _ => return Err(fault("invalid_request", "Unknown operation")),
        };
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(fault("invalid_request", "Unexpected request field"));
        }
        let operation: Operation = serde_json::from_value(Value::Object(object))
            .map_err(|_| fault("invalid_request", "Invalid operation fields"))?;
        if timeout_ms.is_some()
            && matches!(
                operation,
                Operation::Cancel { .. }
                    | Operation::RequestStatus { .. }
                    | Operation::CancelOpen { .. }
                    | Operation::TaskStatus { .. }
            )
        {
            return Err(fault(
                "invalid_request",
                "Request management cannot set a work timeout",
            ));
        }
        let bounded = |s: &str, max: usize| !s.is_empty() && s.len() <= max;
        let valid = match &operation {
            Operation::Open {
                source: SourceSpec::Local { path },
            }
            | Operation::StartOpen {
                source: SourceSpec::Local { path },
            } => bounded(path, 4096),
            Operation::Open {
                source: SourceSpec::Remote { url },
            }
            | Operation::StartOpen {
                source: SourceSpec::Remote { url },
            } => bounded(url, 4096) && url.starts_with("https://"),
            Operation::PollOpen {
                task_id,
                max_events,
            } => bounded(task_id, 128) && (1..=64).contains(max_events),
            Operation::PeekOpen {
                task_id,
                query,
                top_k,
            } => {
                bounded(task_id, 128)
                    && bounded(query, 4096)
                    && !query.trim().is_empty()
                    && (1..=32).contains(top_k)
            }
            Operation::ExpandOpen {
                task_id,
                reference,
                before,
                after,
                ..
            } => {
                bounded(task_id, 128)
                    && bounded(reference, 128)
                    && *before <= 65536
                    && *after <= 65536
            }
            Operation::FinishOpen {
                task_id,
                references,
            } => {
                bounded(task_id, 128)
                    && references.len() <= 32
                    && references.iter().all(|r| bounded(r, 128))
                    && references.iter().collect::<BTreeSet<_>>().len() == references.len()
            }
            Operation::CancelOpen { task_id } | Operation::TaskStatus { task_id } => {
                bounded(task_id, 128)
            }
            Operation::Search {
                session_id,
                query,
                top_k,
                ..
            } => {
                bounded(session_id, 128)
                    && bounded(query, 4096)
                    && !query.trim().is_empty()
                    && (1..=32).contains(top_k)
            }
            Operation::Expand {
                session_id,
                reference,
                before,
                after,
                ..
            } => {
                bounded(session_id, 128)
                    && bounded(reference, 128)
                    && *before <= 65_536
                    && *after <= 65_536
            }
            Operation::Pack {
                session_id,
                references,
            } => {
                bounded(session_id, 128)
                    && !references.is_empty()
                    && references.len() <= 32
                    && references.iter().all(|r| bounded(r, 128))
                    && references.iter().collect::<BTreeSet<_>>().len() == references.len()
            }
            Operation::Close { session_id } => bounded(session_id, 128),
            Operation::Capabilities => true,
            Operation::Cancel { request_id } | Operation::RequestStatus { request_id } => {
                bounded(request_id, 128)
            }
        };
        if !valid {
            return Err(fault(
                "invalid_request",
                "Request fields exceed protocol constraints",
            ));
        }
        Ok(operation)
    })();
    match parse {
        Ok(operation) => Ok(WorkerRequest {
            schema_version: 1,
            id: id.unwrap_or_default(),
            timeout_ms,
            operation,
        }),
        Err(mut error) => {
            error.correlation_id = id;
            Err(error)
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub local_roots: Vec<PathBuf>,
    pub model_dir: Option<PathBuf>,
    pub max_sessions: usize,
    pub max_total_source_chars: usize,
    pub extraction: Limits,
    pub evidence: EvidenceLimits,
    /// Idle lifetime, enforced at request boundaries and by host maintenance.
    pub session_ttl: Duration,
    pub open_tasks: crate::OpenTaskConfig,
    #[cfg(windows)]
    pub extraction_helper: Option<PathBuf>,
    #[cfg(windows)]
    pub isolation: crate::IsolationConfig,
}
impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            local_roots: Vec::new(),
            model_dir: None,
            max_sessions: 4,
            max_total_source_chars: 8_000_000,
            extraction: Limits::default(),
            evidence: EvidenceLimits::default(),
            session_ttl: Duration::from_secs(300),
            open_tasks: crate::OpenTaskConfig::default(),
            #[cfg(windows)]
            extraction_helper: None,
            #[cfg(windows)]
            isolation: crate::IsolationConfig::default(),
        }
    }
}
struct SessionState {
    evidence: EvidenceSession,
    index: Option<LexicalIndex>,
    chars: usize,
    last_activity: Instant,
}
pub struct RetrievalWorker {
    #[cfg(windows)]
    tasks: crate::incremental::OpenTasks,
    config: WorkerConfig,
    local_roots: Vec<PathBuf>,
    sessions: BTreeMap<String, SessionState>,
    total_chars: usize,
    expired_sessions: u64,
    idle_expired_sessions: u64,
    request_control: Option<RequestControl>,
    #[cfg(feature = "cpu-onnx")]
    model: Option<crate::E5Embedder>,
}
pub(crate) fn engine_fault(error: Error) -> WorkerFault {
    // Never echo a source body, URL query, file path or user text in protocol errors.
    let message = error.to_string();
    if message.starts_with("unknown task") {
        fault("unknown_task", "Extraction task is unavailable")
    } else if message.starts_with("task not ready") {
        fault(
            "task_not_ready",
            "Extraction task has not been verified completely",
        )
    } else if message.starts_with("task cancelled") {
        fault(
            "task_cancelled",
            "Extraction task was cancelled; provisional references invalidated",
        )
    } else if message.starts_with("task expired") {
        fault(
            "task_expired",
            "Extraction task expired; provisional references invalidated",
        )
    } else if message.starts_with("task failed") {
        fault(
            "task_failed",
            "Extraction failed; provisional references invalidated",
        )
    } else if message.starts_with("task already finalized") {
        fault("task_finalized", "Extraction task was already finalized")
    } else if message.starts_with("request cancelled") {
        fault(
            "cancelled",
            "Request cancelled; no partial evidence admitted",
        )
    } else if message.starts_with("request deadline") {
        fault(
            "deadline_exceeded",
            "Request deadline exceeded; no partial evidence admitted",
        )
    } else if message.contains("budget")
        || message.contains("limit")
        || message.contains("token window")
        || message.contains("deadline")
    {
        fault(
            "resource_limit",
            "Operation exceeds configured resource constraints",
        )
    } else if message.contains("unknown") {
        fault(
            "unknown_reference",
            "Reference is not available in this session",
        )
    } else {
        fault(
            "operation_failed",
            "Operation failed validation; no partial result",
        )
    }
}

fn admit<T>(
    control: Option<&RequestControl>,
    action: impl FnOnce() -> Result<T, Error>,
) -> Result<T, WorkerFault> {
    match control {
        Some(control) => control.commit(action),
        None => action(),
    }
    .map_err(engine_fault)
}

impl RetrievalWorker {
    pub fn new(config: WorkerConfig) -> Result<Self, Error> {
        config.open_tasks.validate()?;
        #[cfg(windows)]
        {
            config.isolation.validate()?;
            if config
                .extraction_helper
                .as_ref()
                .is_some_and(|p| !p.is_file())
            {
                return Err(error("extraction helper executable unavailable"));
            }
        }
        config.extraction.validate()?;
        config.evidence.validate()?;
        if !(1..=16).contains(&config.max_sessions)
            || !(1..=64_000_000).contains(&config.max_total_source_chars)
            || config.local_roots.len() > 16
            || config.session_ttl < Duration::from_millis(1)
            || config.session_ttl > Duration::from_secs(3600)
        {
            return Err(error("invalid worker limits"));
        }
        let local_roots = config
            .local_roots
            .iter()
            .map(|root| {
                let root = root
                    .canonicalize()
                    .map_err(|_| error("permitted local root unavailable"))?;
                if !root.is_dir() {
                    return Err(error("permitted local root is not a directory"));
                }
                Ok(root)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Self {
            #[cfg(windows)]
            tasks: crate::incremental::OpenTasks::new(config.open_tasks.clone()),
            config,
            local_roots,
            sessions: BTreeMap::new(),
            total_chars: 0,
            expired_sessions: 0,
            idle_expired_sessions: 0,
            request_control: None,
            #[cfg(feature = "cpu-onnx")]
            model: None,
        })
    }
    pub fn handle_json(&mut self, bytes: &[u8]) -> WorkerResponse {
        self.expire_at(Instant::now(), false);
        match parse_retrieval_request(bytes) {
            Ok(request) => {
                if let Some(timeout) = request
                    .timeout_ms
                    .filter(|_| self.request_control.is_none())
                {
                    return match RequestControl::new(Duration::from_millis(timeout)) {
                        Ok(control) => self.handle_json_with_control(bytes, &control),
                        Err(_) => WorkerResponse {
                            schema_version: 1,
                            id: Some(request.id),
                            ok: false,
                            result: None,
                            error: Some(fault("internal_error", "Request control unavailable")),
                        },
                    };
                }
                let session = match &request.operation {
                    Operation::Search { session_id, .. }
                    | Operation::Expand { session_id, .. }
                    | Operation::Pack { session_id, .. }
                    | Operation::Close { session_id } => Some(session_id.clone()),
                    _ => None,
                };
                if let Some(state) = session.as_ref().and_then(|id| self.sessions.get_mut(id)) {
                    state.last_activity = Instant::now();
                }
                let control = self.request_control.clone();
                let result = control
                    .as_ref()
                    .map_or(Ok(()), |c| {
                        if let Some(ms) = request.timeout_ms {
                            c.limit_deadline(Instant::now() + Duration::from_millis(ms))
                                .map_err(engine_fault)
                        } else {
                            c.checkpoint().map_err(engine_fault)
                        }
                    })
                    .and_then(|_| self.execute(request.operation));
                // A long admitted operation is not expired while it owns the worker.
                if let Some(state) = session.as_ref().and_then(|id| self.sessions.get_mut(id)) {
                    state.last_activity = Instant::now();
                }
                match result {
                    Ok(value) => WorkerResponse {
                        schema_version: 1,
                        id: Some(request.id),
                        ok: true,
                        result: Some(value),
                        error: None,
                    },
                    Err(error) => WorkerResponse {
                        schema_version: 1,
                        id: Some(request.id),
                        ok: false,
                        result: None,
                        error: Some(error),
                    },
                }
            }
            Err(error) => WorkerResponse {
                schema_version: 1,
                id: error.correlation_id.clone(),
                ok: false,
                result: None,
                error: Some(error),
            },
        }
    }
    /// A Rust host can cancel through a clone on another thread. This does not
    /// turn the JSONL binary into a cancellation dispatcher or isolate parsers.
    pub fn handle_json_with_control(
        &mut self,
        bytes: &[u8],
        control: &RequestControl,
    ) -> WorkerResponse {
        self.request_control = Some(control.clone());
        let response = self.handle_json(bytes);
        self.request_control = None;
        control.finish();
        response
    }
    /// Hosts embedding this library must call periodically when no requests arrive.
    /// The native JSONL worker calls this every 100ms while idle.
    pub fn maintenance(&mut self) {
        self.expire_at(Instant::now(), true);
        #[cfg(feature = "cpu-onnx")]
        if let Some(model) = self.model.as_mut() {
            model.maintenance();
        }
    }
    fn expire_at(&mut self, now: Instant, idle: bool) {
        #[cfg(windows)]
        self.tasks.maintenance(now);
        let mut expired = 0;
        let mut chars = 0;
        self.sessions.retain(|_, state| {
            let keep = now.saturating_duration_since(state.last_activity) < self.config.session_ttl;
            if !keep {
                expired += 1;
                chars += state.chars;
            }
            keep
        });
        self.total_chars -= chars;
        self.expired_sessions += expired;
        if idle {
            self.idle_expired_sessions += expired;
        }
    }
    fn execute(&mut self, operation: Operation) -> Result<Value, WorkerFault> {
        let control = self.request_control.clone();
        match operation {
            Operation::StartOpen { source } => self.start_open(source),
            Operation::PollOpen {
                task_id,
                max_events,
            } => {
                #[cfg(windows)]
                {
                    self.tasks
                        .poll(&task_id, max_events, control.as_ref())
                        .map_err(engine_fault)
                }
                #[cfg(not(windows))]
                {
                    let _ = (task_id, max_events);
                    Err(fault(
                        "feature_unavailable",
                        "Incremental extraction requires the Windows helper",
                    ))
                }
            }
            Operation::PeekOpen {
                task_id,
                query,
                top_k,
            } => {
                #[cfg(windows)]
                {
                    self.tasks
                        .peek(
                            &task_id,
                            &query,
                            top_k,
                            self.config.evidence,
                            control.as_ref(),
                        )
                        .map_err(engine_fault)
                }
                #[cfg(not(windows))]
                {
                    let _ = (task_id, query, top_k);
                    Err(fault(
                        "feature_unavailable",
                        "Incremental extraction requires the Windows helper",
                    ))
                }
            }
            Operation::ExpandOpen {
                task_id,
                reference,
                before,
                after,
                unit,
            } => {
                #[cfg(windows)]
                {
                    self.tasks
                        .expand(
                            &task_id,
                            &reference,
                            crate::Expansion {
                                before,
                                after,
                                unit,
                            },
                            self.config.evidence,
                            control.as_ref(),
                        )
                        .map_err(engine_fault)
                }
                #[cfg(not(windows))]
                {
                    let _ = (task_id, reference, before, after, unit);
                    Err(fault(
                        "feature_unavailable",
                        "Incremental extraction requires the Windows helper",
                    ))
                }
            }
            Operation::FinishOpen {
                task_id,
                references,
            } => self.finish_open(&task_id, &references),
            Operation::CancelOpen { task_id } => self
                .open_task_controller()
                .cancel(&task_id)
                .map_err(engine_fault),
            Operation::TaskStatus { task_id } => self
                .open_task_controller()
                .status(&task_id)
                .map_err(engine_fault),
            Operation::Cancel { .. } | Operation::RequestStatus { .. } => Err(fault(
                "feature_unavailable",
                "Request management requires the JSONL dispatcher or a Rust RequestControl clone",
            )),
            Operation::Capabilities => {
                let mut modes = vec!["literal", "bm25"];
                if cfg!(feature = "cpu-onnx") && self.config.model_dir.is_some() {
                    modes.extend(["hybrid", "hybrid_prefilter"]);
                }
                #[cfg(feature = "cpu-onnx")]
                let model_loaded = self.model.is_some();
                #[cfg(not(feature = "cpu-onnx"))]
                let model_loaded = false;
                #[cfg(windows)]
                let isolated_extraction = self.config.extraction_helper.is_some();
                #[cfg(not(windows))]
                let isolated_extraction = false;
                admit(control.as_ref(), || {
                    Ok(
                        json!({"protocol":"retrieval-worker/v1","modes":modes,"cpu_compiled":cfg!(feature="cpu-onnx"),
                    "model_configured":self.config.model_dir.is_some(),"model_loaded":model_loaded,
                    "active_sessions":self.sessions.len(),"local_roots_granted":self.local_roots.len(),
                    "units":{"characters":"Unicode scalar values","words":"Unicode whitespace separated"},
                    "evidence_pack":true,"ttl_enforced":true,"hard_native_cancellation":false,"jsonl_cancel":false,
                    "isolated_extraction":isolated_extraction,"incremental_extraction":isolated_extraction,
                    "provisional_modes":["literal"],"live_open_tasks":self.live_open_tasks(),"reserved_open_chars":self.reserved_open_chars(),
                    "max_open_tasks":self.config.open_tasks.max_live,"max_open_task_records":self.config.open_tasks.max_entries,
                    "max_provisional_references":self.config.open_tasks.max_previews,"open_task_ttl_ms":self.config.open_tasks.idle_ttl.as_millis(),
                    "session_ttl_ms":self.config.session_ttl.as_millis(),
                    "retained_source_chars":self.total_chars,"expired_sessions":self.expired_sessions,
                    "idle_expired_sessions":self.idle_expired_sessions}),
                    )
                })
            }
            Operation::Open { source } => self.open(source),
            Operation::Search {
                session_id,
                query,
                mode,
                top_k,
            } => self.search(&session_id, &query, mode, top_k),
            Operation::Expand {
                session_id,
                reference,
                before,
                after,
                unit,
            } => {
                let state = self
                    .sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| fault("unknown_session", "Session is not available"))?;
                let view = admit(control.as_ref(), || {
                    state.evidence.expand(&reference, before, after, unit)
                })?;
                serde_json::to_value(view)
                    .map_err(|_| fault("internal_error", "Response serialization failed"))
            }
            Operation::Pack {
                session_id,
                references,
            } => {
                let state = self
                    .sessions
                    .get(&session_id)
                    .ok_or_else(|| fault("unknown_session", "Session is not available"))?;
                let pack = state.evidence.export(&references).map_err(engine_fault)?;
                pack.verify_against(
                    &state
                        .evidence
                        .documents()
                        .iter()
                        .map(Arc::as_ref)
                        .collect::<Vec<_>>(),
                )
                .map_err(engine_fault)?;
                let value = serde_json::to_value(pack)
                    .map_err(|_| fault("internal_error", "Pack serialization failed"))?;
                admit(control.as_ref(), || Ok(value))
            }
            Operation::Close { session_id } => {
                if !self.sessions.contains_key(&session_id) {
                    return Err(fault("unknown_session", "Session is not available"));
                }
                admit(control.as_ref(), || {
                    let state = self
                        .sessions
                        .remove(&session_id)
                        .ok_or_else(|| error("unknown session"))?;
                    self.total_chars -= state.chars;
                    Ok(json!({"closed":true,"session_id":session_id}))
                })
            }
        }
    }
    fn resolve_source(&self, source: SourceSpec) -> Result<Source, WorkerFault> {
        Ok(match source {
            SourceSpec::Remote { url } => Source::Remote(url),
            SourceSpec::Local { path } => {
                if self.local_roots.is_empty() {
                    return Err(fault(
                        "forbidden_source",
                        "Host did not grant local source access",
                    ));
                }
                let canonical = PathBuf::from(path)
                    .canonicalize()
                    .map_err(|_| fault("source_unavailable", "Local source unavailable"))?;
                if !self
                    .local_roots
                    .iter()
                    .any(|root| canonical.starts_with(root))
                {
                    return Err(fault(
                        "forbidden_source",
                        "Local source is outside host-granted roots",
                    ));
                }
                Source::Local(canonical)
            }
        })
    }
    pub fn open_task_controller(&self) -> crate::OpenTaskController {
        #[cfg(windows)]
        {
            self.tasks.controls.clone()
        }
        #[cfg(not(windows))]
        {
            crate::OpenTaskController::default()
        }
    }
    fn live_open_tasks(&self) -> usize {
        #[cfg(windows)]
        {
            self.tasks.live()
        }
        #[cfg(not(windows))]
        {
            0
        }
    }
    fn reserved_open_chars(&self) -> usize {
        #[cfg(windows)]
        {
            self.tasks.reserved_chars()
        }
        #[cfg(not(windows))]
        {
            0
        }
    }
    fn extraction_limits(&self) -> Result<Limits, WorkerFault> {
        if self.sessions.len() + self.live_open_tasks() >= self.config.max_sessions {
            return Err(fault(
                "resource_limit",
                "Active and reserved session budget exceeded",
            ));
        }
        let remaining = self
            .config
            .max_total_source_chars
            .saturating_sub(self.total_chars + self.reserved_open_chars());
        if remaining == 0 {
            return Err(fault(
                "resource_limit",
                "Source text reservation budget exceeded",
            ));
        }
        let mut limits = self.config.extraction.clone();
        limits.max_text_chars = limits.max_text_chars.min(remaining);
        Ok(limits)
    }
    fn start_open(&mut self, source: SourceSpec) -> Result<Value, WorkerFault> {
        #[cfg(windows)]
        {
            let source = self.resolve_source(source)?;
            let limits = self.extraction_limits()?;
            let helper = self.config.extraction_helper.as_ref().ok_or_else(|| {
                fault(
                    "feature_unavailable",
                    "Host did not configure an isolated extraction helper",
                )
            })?;
            self.tasks
                .start(
                    helper,
                    source,
                    limits,
                    self.config.isolation.clone(),
                    self.request_control.as_ref(),
                )
                .map_err(engine_fault)
        }
        #[cfg(not(windows))]
        {
            let _ = source;
            Err(fault(
                "feature_unavailable",
                "Incremental extraction requires the Windows helper",
            ))
        }
    }
    fn finish_open(&mut self, task_id: &str, references: &[String]) -> Result<Value, WorkerFault> {
        #[cfg(windows)]
        {
            let prepared = self
                .tasks
                .prepare_finish(
                    task_id,
                    references,
                    self.config.evidence,
                    self.request_control.as_ref(),
                )
                .map_err(engine_fault)?;
            let id = prepared.session.id().to_string();
            let chars = prepared.document.text().chars().count();
            let response = json!({"operation":"finish_open","task_id":task_id,"provisional":false,"session_id":id,
                "provenance":prepared.document.provenance(),"pages":prepared.document.pages(),"warnings":prepared.document.warnings(),
                "isolation":prepared.isolation,"index_built":false,"snapshot_retained_in_memory":true,"bindings":prepared.bindings,"preview_lineage":prepared.lineage});
            let sessions = &mut self.sessions;
            let total = &mut self.total_chars;
            self.tasks
                .finalize(task_id, id.clone(), self.request_control.as_ref(), || {
                    sessions.insert(
                        id,
                        SessionState {
                            evidence: prepared.session,
                            index: None,
                            chars,
                            last_activity: Instant::now(),
                        },
                    );
                    *total += chars;
                    Ok(response)
                })
                .map_err(engine_fault)
        }
        #[cfg(not(windows))]
        {
            let _ = (task_id, references);
            Err(fault(
                "feature_unavailable",
                "Incremental extraction requires the Windows helper",
            ))
        }
    }
    fn open(&mut self, source: SourceSpec) -> Result<Value, WorkerFault> {
        let limits = self.extraction_limits()?;
        let source = self.resolve_source(source)?;
        #[cfg(windows)]
        let (loaded, isolation) = if let Some(program) = self.config.extraction_helper.as_ref() {
            let control = match &self.request_control {
                Some(control) => control.clone(),
                None => RequestControl::new(Duration::from_secs(30)).map_err(engine_fault)?,
            };
            match crate::ExtractionTask::start(
                program,
                source,
                limits.clone(),
                self.config.isolation.clone(),
                control,
            )
            .and_then(|task| task.finish())
            {
                Ok((document, report)) => (Ok(document), Some(report)),
                Err(failure) => (Err(failure), None),
            }
        } else {
            (Document::load(source, &limits), None)
        };
        #[cfg(not(windows))]
        let (loaded, isolation): (Result<Document, Error>, Option<Value>) =
            (Document::load(source, &limits), None);
        let doc = Arc::new(loaded.map_err(|failure| {
            if let Some(control) = &self.request_control {
                if let Err(cancelled) = control.checkpoint() {
                    return engine_fault(cancelled);
                }
            }
            let message = failure.to_string();
            if message.contains("OCR required") {
                fault(
                    "ocr_required",
                    "Source has no extractable PDF text; OCR required",
                )
            } else if message.contains("budget") {
                fault("resource_limit", "Extraction exceeds configured budgets")
            } else {
                fault(
                    "source_unavailable",
                    "Source acquisition or extraction failed; verify transport, format and limits",
                )
            }
        })?);
        let chars = doc.text().chars().count();
        if chars
            > self
                .config
                .max_total_source_chars
                .saturating_sub(self.total_chars + self.reserved_open_chars())
        {
            return Err(fault(
                "resource_limit",
                "Total retained source-text budget exceeded",
            ));
        }
        let session =
            EvidenceSession::new(vec![doc.clone()], self.config.evidence).map_err(engine_fault)?;
        let id = session.id().to_string();
        let response = json!({"session_id":id,"provenance":doc.provenance(),"pages":doc.pages(),"warnings":doc.warnings(),
            "index_built":false,"snapshot_retained_in_memory":true,"isolation":isolation});
        let control = self.request_control.clone();
        admit(control.as_ref(), || {
            self.sessions.insert(
                id,
                SessionState {
                    evidence: session,
                    index: None,
                    chars,
                    last_activity: Instant::now(),
                },
            );
            self.total_chars += chars;
            Ok(response)
        })
    }
    fn search(
        &mut self,
        session_id: &str,
        query: &str,
        mode: SearchMode,
        top_k: usize,
    ) -> Result<Value, WorkerFault> {
        let control = self.request_control.clone();
        if !self.sessions.contains_key(session_id) {
            return Err(fault("unknown_session", "Session is not available"));
        }
        if matches!(mode, SearchMode::Hybrid | SearchMode::HybridPrefilter) {
            #[cfg(not(feature = "cpu-onnx"))]
            return Err(fault(
                "feature_unavailable",
                "CPU semantic feature was not compiled",
            ));
            #[cfg(feature = "cpu-onnx")]
            if self.model.is_none() {
                let root = self.config.model_dir.as_ref().ok_or_else(|| {
                    fault(
                        "model_unavailable",
                        "Host did not configure CPU model resources",
                    )
                })?;
                self.model = Some(
                    crate::E5Embedder::load(root, crate::CpuConfig::default()).map_err(|_| {
                        fault(
                            "model_unavailable",
                            "CPU model resources unavailable or unverified",
                        )
                    })?,
                );
            }
        }
        let state = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| fault("unknown_session", "Session is not available"))?;
        if let Some(control) = &control {
            control.checkpoint().map_err(engine_fault)?;
        }
        let (evidence, info): (Vec<Evidence>, Value) = if mode == SearchMode::Literal {
            let hits = state.evidence.documents()[0]
                .find(query, top_k)
                .map_err(engine_fault)?;
            (
                hits,
                json!({"mode":mode,"coverage_scope":"extracted_text","max_hits":top_k,"case_sensitive":true}),
            )
        } else {
            if state.index.is_none() {
                state.index = Some(
                    LexicalIndex::build(
                        state.evidence.documents().to_vec(),
                        ChunkConfig::default(),
                        IndexLimits::default(),
                    )
                    .map_err(engine_fault)?,
                );
            }
            let index = state
                .index
                .as_ref()
                .ok_or_else(|| fault("internal_error", "Index unavailable"))?;
            if mode == SearchMode::Bm25 {
                let ranked = index.search(query, top_k).map_err(engine_fault)?;
                let metadata=ranked.iter().map(|h|json!({"chunk_index":h.chunk_index,"score":h.score,"matched_terms":h.matched_terms})).collect::<Vec<_>>();
                (
                    ranked.into_iter().map(|h| h.evidence).collect(),
                    json!({"mode":mode,"index_stats":index.stats(),"ranking":metadata,"semantic_inference":false}),
                )
            } else {
                #[cfg(feature = "cpu-onnx")]
                {
                    let mut options = crate::HybridOptions {
                        top_k,
                        ..crate::HybridOptions::default()
                    };
                    if mode == SearchMode::HybridPrefilter {
                        options.coverage = crate::SemanticCoverage::LexicalPrefilter;
                        options.lexical_candidates = 16;
                        options.semantic_candidates = 16;
                        options.budget.max_semantic_chunks = 16;
                    }
                    let model = self
                        .model
                        .as_mut()
                        .ok_or_else(|| fault("model_unavailable", "CPU model not loaded"))?;
                    let response = if let Some(control) = &control {
                        crate::hybrid_search_with_control(
                            index, model, query, &options, None, control,
                        )
                    } else {
                        crate::hybrid_search(index, model, query, &options, None)
                    }
                    .map_err(engine_fault)?;
                    let metadata=response.hits.iter().map(|h|json!({"chunk_index":h.chunk_index,"rrf_score":h.rrf_score,
                        "lexical_rank":h.lexical_rank,"semantic_rank":h.semantic_rank,"cosine":h.cosine,"matched_terms":h.matched_terms})).collect::<Vec<_>>();
                    (
                        response.hits.into_iter().map(|h| h.evidence).collect(),
                        json!({"mode":mode,"coverage":response.coverage,"cost":response.cost,
                        "model":response.model,"ranking":metadata,"rrf_k":response.rrf_k,"semantic_inference":true}),
                    )
                }
                #[cfg(not(feature = "cpu-onnx"))]
                {
                    return Err(fault(
                        "feature_unavailable",
                        "CPU semantic feature was not compiled",
                    ));
                }
            }
        };
        let entries = admit(control.as_ref(), || {
            if evidence.is_empty() {
                Ok(Vec::new())
            } else {
                state.evidence.register_many(query, &evidence)
            }
        })?;
        Ok(
            json!({"session_id":session_id,"query_sha256":crate::document::sha(query.as_bytes()),"entries":entries,"search":info}),
        )
    }
}
