//! Windows trusted-host client for one owned resident worker. No listener,
//! implicit restart, model duplication or authorization from request IDs.
//! A local wait deadline is an uncertain outcome until cancel/status/termination;
//! it does not mean the worker was stopped. Management has two reserved slots.
use crate::{
    document::sha, parse_retrieval_request, EvidencePack, EvidenceView, Operation, PageRange,
    Provenance, SearchMode, SourceSpec, Unit, WorkerRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    io::{BufRead, BufReader, Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::System::JobObjects::*;

static CLIENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Debug)]
pub enum ClientError {
    Configuration(&'static str),
    ResourceLimit,
    Closed,
    Protocol(&'static str),
    Deadline { request_id: String },
    Worker { code: String, message: String },
}
impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(m) | Self::Protocol(m) => f.write_str(m),
            Self::ResourceLimit => f.write_str("client request budget exceeded"),
            Self::Closed => f.write_str("worker transport closed; no automatic replay"),
            Self::Deadline { request_id } => write!(
                f,
                "client deadline for {request_id}; worker outcome may be uncertain"
            ),
            Self::Worker { code, message } => write!(f, "{code}: {message}"),
        }
    }
}
impl std::error::Error for ClientError {}
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub program: PathBuf,
    /// Trusted host launcher arguments, never tool/LLM input.
    pub prefix_args: Vec<OsString>,
    pub local_roots: Vec<PathBuf>,
    pub model_dir: Option<PathBuf>,
    pub max_in_flight: usize,
    pub max_work_requests: usize,
    pub max_response_bytes: usize,
    pub worker_timeout: Duration,
}
impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            program: "retrieval-worker.exe".into(),
            prefix_args: Vec::new(),
            local_roots: Vec::new(),
            model_dir: None,
            max_in_flight: 8,
            max_work_requests: 4096,
            max_response_bytes: 4 * 1024 * 1024,
            worker_timeout: Duration::from_secs(30),
        }
    }
}

struct ProcessTree(OwnedHandle);
impl ProcessTree {
    fn new() -> Result<Self, ClientError> {
        // SAFETY: unnamed private job, with handle owned exactly once.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(ClientError::Configuration("client job creation failed"));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // No active-process limit: the worker owns nested extraction helpers.
        if unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(ClientError::Configuration(
                "client job configuration failed",
            ));
        }
        Ok(Self(handle))
    }
    fn attach(&self, child: &Child) -> Result<(), ClientError> {
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(ClientError::Configuration(
                "owned worker could not join client job",
            ));
        }
        Ok(())
    }
    fn stop(&self) {
        unsafe {
            TerminateJobObject(self.0.as_raw_handle(), 0xE0C1);
        }
    }
    fn active(&self) -> Option<u32> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            None
        } else {
            Some(info.ActiveProcesses)
        }
    }
    fn wait_empty(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.active() {
                Some(0) => return true,
                None => return false,
                _ => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ClientShutdown {
    pub transport_closed: bool,
    pub exit_observed: bool,
    pub exit_code: Option<i32>,
    pub tree_processes_before_stop: Option<u32>,
    pub tree_exit_observed: bool,
}

struct Pending {
    management: bool,
    send: SyncSender<Result<Value, ClientError>>,
}
struct State {
    closed: bool,
    sequence: u64,
    work_requests: usize,
    pending: BTreeMap<String, Pending>,
}
struct Shared {
    state: Mutex<State>,
    writer: Mutex<Option<SyncSender<Vec<u8>>>>,
    child: Mutex<Option<Child>>,
    scope: Mutex<Option<ProcessTree>>,
    shutdown: Mutex<ClientShutdown>,
}
impl Shared {
    fn deliver(&self, bytes: &[u8]) -> Result<(), ClientError> {
        let (id, result) = decode(bytes)?;
        let pending = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pending
            .remove(&id)
            .ok_or(ClientError::Protocol(
                "unknown or duplicate worker reply id",
            ))?;
        let _ = pending.send.try_send(result);
        Ok(())
    }
    fn closed(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closed
    }
    fn stop(&self, why: ClientError) -> ClientShutdown {
        let mut report = self.shutdown.lock().unwrap_or_else(|p| p.into_inner());
        let pending = {
            let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            s.closed = true;
            std::mem::take(&mut s.pending)
        };
        self.writer.lock().unwrap_or_else(|p| p.into_inner()).take();
        report.transport_closed = true;
        let scope = self.scope.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(scope) = &scope {
            report.tree_processes_before_stop = scope.active();
            scope.stop();
        }
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut process) = child.take() {
            if !matches!(process.try_wait(), Ok(Some(_))) {
                let _ = process.kill();
            }
            match process.wait() {
                Ok(status) => {
                    report.exit_observed = true;
                    report.exit_code = status.code();
                }
                Err(_) => {
                    *child = Some(process);
                }
            }
        }
        drop(child);
        if let Some(scope) = scope {
            report.tree_exit_observed = scope.wait_empty();
        }
        for (_, p) in pending {
            let _ = p.send.try_send(Err(why.clone()));
        }
        report.clone()
    }
}
struct Owner {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.shared.stop(ClientError::Closed);
        for handle in self
            .threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            let _ = handle.join();
        }
    }
}
#[derive(Clone)]
pub struct ProcessClient {
    owner: Arc<Owner>,
    config: Arc<ClientConfig>,
    prefix: String,
}
pub struct ClientRequest {
    id: String,
    deadline: Instant,
    receiver: Receiver<Result<Value, ClientError>>,
}
impl ClientRequest {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn wait(self) -> Result<Value, ClientError> {
        match self
            .receiver
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
        {
            Ok(reply) => reply,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ClientError::Deadline {
                request_id: self.id,
            }),
            Err(_) => Err(ClientError::Closed),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFault {
    code: String,
    message: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReply {
    schema_version: u32,
    id: String,
    ok: bool,
    result: Option<Value>,
    error: Option<WireFault>,
}
fn decode(bytes: &[u8]) -> Result<(String, Result<Value, ClientError>), ClientError> {
    let r: WireReply = serde_json::from_slice(bytes)
        .map_err(|_| ClientError::Protocol("invalid worker reply envelope"))?;
    if r.schema_version != 1 || r.id.is_empty() || r.id.len() > 128 {
        return Err(ClientError::Protocol(
            "invalid worker reply identity/version",
        ));
    }
    let result = match (r.ok, r.result, r.error) {
        (true, Some(value), None) if value.is_object() => Ok(value),
        (false, None, Some(f))
            if !f.code.is_empty() && f.code.len() <= 128 && f.message.len() <= 4096 =>
        {
            Err(ClientError::Worker {
                code: f.code,
                message: f.message,
            })
        }
        _ => {
            return Err(ClientError::Protocol(
                "inconsistent worker success/error envelope",
            ))
        }
    };
    Ok((r.id, result))
}
impl ProcessClient {
    pub fn spawn(config: ClientConfig) -> Result<Self, ClientError> {
        if !(1..=8).contains(&config.max_in_flight)
            || !(1..=65536).contains(&config.max_work_requests)
            || !(1024..=16 * 1024 * 1024).contains(&config.max_response_bytes)
            || config.worker_timeout < Duration::from_millis(1)
            || config.worker_timeout > Duration::from_secs(300)
            || config.local_roots.len() > 64
            || config.prefix_args.len() > 32
        {
            return Err(ClientError::Configuration(
                "invalid resident client budgets",
            ));
        }
        let program = config
            .program
            .canonicalize()
            .map_err(|_| ClientError::Configuration("worker executable unavailable"))?;
        if !program.is_file() {
            return Err(ClientError::Configuration("worker is not a file"));
        }
        let mut command = Command::new(program);
        command
            .args(&config.prefix_args)
            .arg("--request-timeout-ms")
            .arg(config.worker_timeout.as_millis().to_string())
            .arg("--max-request-ids")
            .arg(config.max_work_requests.to_string());
        for root in &config.local_roots {
            command.arg("--local-root").arg(root);
        }
        if let Some(root) = &config.model_dir {
            command.arg("--model-dir").arg(root);
        }
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "PATHEXT", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let tree = ProcessTree::new()?;
        let mut child = command
            .spawn()
            .map_err(|_| ClientError::Configuration("worker spawn failed"))?;
        if let Err(e) = tree.attach(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        let (mut stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(input), Some(output)) => (input, output),
            _ => {
                tree.stop();
                let _ = child.kill();
                let _ = child.wait();
                return Err(ClientError::Closed);
            }
        };
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(config.max_in_flight + 2);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                closed: false,
                sequence: 0,
                work_requests: 0,
                pending: BTreeMap::new(),
            }),
            writer: Mutex::new(Some(send)),
            child: Mutex::new(Some(child)),
            scope: Mutex::new(Some(tree)),
            shutdown: Mutex::new(ClientShutdown::default()),
        });
        let owner = Arc::new(Owner {
            shared: shared.clone(),
            threads: Mutex::new(Vec::new()),
        });
        let write_shared = shared.clone();
        let writer = thread::Builder::new()
            .name("retrieval-client-write".into())
            .spawn(move || {
                while let Ok(bytes) = receive.recv() {
                    if write_shared.closed() {
                        break;
                    }
                    if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
                        write_shared.stop(ClientError::Closed);
                        break;
                    }
                }
            })
            .map_err(|_| ClientError::Configuration("client writer thread unavailable"))?;
        owner
            .threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(writer);
        let read_shared = shared.clone();
        let max = config.max_response_bytes;
        let reader = thread::Builder::new()
            .name("retrieval-client-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut bytes = Vec::new();
                    let n = Read::take(&mut reader, max as u64 + 1).read_until(b'\n', &mut bytes);
                    let failure = match n {
                        Ok(0) | Err(_) => Some(ClientError::Closed),
                        Ok(_) if bytes.len() > max => Some(ClientError::Protocol(
                            "worker reply exceeds client frame budget",
                        )),
                        Ok(_) if bytes.last() != Some(&b'\n') => {
                            Some(ClientError::Protocol("unterminated worker reply"))
                        }
                        Ok(_) => read_shared.deliver(&bytes).err(),
                    };
                    if let Some(e) = failure {
                        read_shared.stop(e);
                        break;
                    }
                }
            })
            .map_err(|_| ClientError::Configuration("client reader thread unavailable"))?;
        owner
            .threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(reader);
        Ok(Self {
            owner,
            config: Arc::new(config),
            prefix: format!(
                "rc{}_{}",
                std::process::id(),
                CLIENT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
        })
    }
    pub fn is_closed(&self) -> bool {
        self.owner.shared.closed()
    }
    /// Terminates/waits only the owned worker tree. Pending calls fail; no replay.
    pub fn shutdown(&self) -> ClientShutdown {
        self.owner.shared.stop(ClientError::Closed)
    }
    pub fn submit(
        &self,
        operation: Operation,
        timeout: Duration,
    ) -> Result<ClientRequest, ClientError> {
        if timeout < Duration::from_millis(1) || timeout > Duration::from_secs(300) {
            return Err(ClientError::Configuration(
                "request timeout must be 1..300000ms",
            ));
        }
        let management = matches!(
            &operation,
            Operation::Cancel { .. }
                | Operation::RequestStatus { .. }
                | Operation::CancelOpen { .. }
                | Operation::TaskStatus { .. }
        );
        let deadline = Instant::now() + timeout;
        let shared = &self.owner.shared;
        let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return Err(ClientError::Closed);
        }
        let used = state
            .pending
            .values()
            .filter(|p| p.management == management)
            .count();
        if used
            >= if management {
                2
            } else {
                self.config.max_in_flight
            }
            || (!management && state.work_requests >= self.config.max_work_requests)
        {
            return Err(ClientError::ResourceLimit);
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or(ClientError::ResourceLimit)?;
        let id = format!("{}_{}", self.prefix, state.sequence);
        let request = WorkerRequest {
            schema_version: 1,
            id: id.clone(),
            timeout_ms: (!management).then_some(timeout.as_millis() as u64),
            operation,
        };
        let mut bytes = serde_json::to_vec(&request)
            .map_err(|_| ClientError::Protocol("request serialization failed"))?;
        parse_retrieval_request(&bytes)
            .map_err(|_| ClientError::Configuration("request violates worker contract"))?;
        bytes.push(b'\n');
        let (send, receiver) = mpsc::sync_channel(1);
        state
            .pending
            .insert(id.clone(), Pending { management, send });
        let result = shared
            .writer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .ok_or(ClientError::Closed)?
            .try_send(bytes);
        if result.is_err() {
            state.pending.remove(&id);
            return Err(ClientError::ResourceLimit);
        }
        if !management {
            state.work_requests += 1;
        }
        Ok(ClientRequest {
            id,
            deadline,
            receiver,
        })
    }
    fn request(&self, operation: Operation, timeout: Duration) -> Result<Value, ClientError> {
        self.submit(operation, timeout)?.wait()
    }
    fn typed<T: serde::de::DeserializeOwned>(&self, value: Value) -> Result<T, ClientError> {
        serde_json::from_value(value).map_err(|_| {
            let e = ClientError::Protocol("worker result violates typed consumer contract");
            self.owner.shared.stop(e.clone());
            e
        })
    }
    pub fn open(
        &self,
        source: SourceSpec,
        timeout: Duration,
    ) -> Result<ClientSession, ClientError> {
        let opened: Opened = self.typed(self.request(Operation::Open { source }, timeout)?)?;
        if opened.session_id.is_empty()
            || opened.session_id.len() > 128
            || !opened.snapshot_retained_in_memory
            || opened.index_built
            || opened.provenance.snapshot_id.len() != 64
            || !crate::evidence::source_bounds(&opened.provenance)
            || opened.pages.len() > 5000
            || crate::evidence::canonical_hash(&(
                opened.provenance.extractor.as_str(),
                opened.provenance.final_source.as_str(),
                opened.provenance.raw_sha256.as_str(),
                opened.provenance.text_sha256.as_str(),
            ))
            .ok()
            .as_deref()
                != Some(opened.provenance.snapshot_id.as_str())
        {
            self.owner
                .shared
                .stop(ClientError::Protocol("invalid opened snapshot"));
            return Err(ClientError::Protocol("invalid opened snapshot"));
        }
        Ok(ClientSession {
            client: self.clone(),
            opened,
        })
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Opened {
    session_id: String,
    provenance: Provenance,
    pages: Vec<PageRange>,
    warnings: Vec<String>,
    index_built: bool,
    snapshot_retained_in_memory: bool,
    isolation: Option<Value>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientSearch {
    pub session_id: String,
    pub query_sha256: String,
    pub entries: Vec<EvidenceView>,
    pub search: Value,
}
pub struct ClientSession {
    client: ProcessClient,
    opened: Opened,
}
impl ClientSession {
    pub fn id(&self) -> &str {
        &self.opened.session_id
    }
    pub fn provenance(&self) -> &Provenance {
        &self.opened.provenance
    }
    pub fn pages(&self) -> &[PageRange] {
        &self.opened.pages
    }
    pub fn warnings(&self) -> &[String] {
        &self.opened.warnings
    }
    pub fn isolation(&self) -> Option<&Value> {
        self.opened.isolation.as_ref()
    }
    fn verify_view(&self, v: &EvidenceView) -> Result<(), ClientError> {
        let expected_pages: Vec<_> = self
            .pages()
            .iter()
            .filter(|p| p.start < v.evidence.end && p.end > v.evidence.start)
            .map(|p| p.number)
            .collect();
        if v.record.schema_version != 1
            || v.record.document_index != 0
            || v.record.ordinal == 0
            || v.record.anchor_start >= v.record.anchor_end
            || v.record.start > v.record.anchor_start
            || v.record.anchor_end > v.record.end
            || v.evidence.pages != expected_pages
            || v.record.session_id != self.id()
            || v.record.snapshot_id != self.provenance().snapshot_id
            || v.evidence.snapshot_id != self.provenance().snapshot_id
            || v.evidence.source != self.provenance().final_source
            || v.record.start != v.evidence.start
            || v.record.end != v.evidence.end
            || v.record.end.checked_sub(v.record.start) != Some(v.evidence.text.chars().count())
            || v.record.quote_sha256 != sha(v.evidence.text.as_bytes())
            || v.record.reference_id().ok().as_deref() != Some(v.reference.as_str())
        {
            let e = ClientError::Protocol("evidence does not match opened snapshot/reference");
            self.client.owner.shared.stop(e.clone());
            return Err(e);
        }
        Ok(())
    }
    pub fn search(
        &self,
        query: &str,
        mode: SearchMode,
        top_k: usize,
        timeout: Duration,
    ) -> Result<ClientSearch, ClientError> {
        let result: ClientSearch = self.client.typed(self.client.request(
            Operation::Search {
                session_id: self.id().into(),
                query: query.into(),
                mode,
                top_k,
            },
            timeout,
        )?)?;
        if result.session_id != self.id()
            || result.query_sha256 != sha(query.as_bytes())
            || result.entries.len() > top_k
        {
            let e = ClientError::Protocol("search result identity differs");
            self.client.owner.shared.stop(e.clone());
            return Err(e);
        }
        for view in &result.entries {
            self.verify_view(view)?;
            if view.record.query_sha256 != result.query_sha256 {
                let error = ClientError::Protocol("query-bound evidence differs");
                self.client.owner.shared.stop(error.clone());
                return Err(error);
            }
        }
        Ok(result)
    }
    pub fn expand(
        &self,
        reference: &str,
        before: usize,
        after: usize,
        unit: Unit,
        timeout: Duration,
    ) -> Result<EvidenceView, ClientError> {
        let view: EvidenceView = self.client.typed(self.client.request(
            Operation::Expand {
                session_id: self.id().into(),
                reference: reference.into(),
                before,
                after,
                unit,
            },
            timeout,
        )?)?;
        self.verify_view(&view)?;
        if view.record.parent_reference.as_deref() != Some(reference) {
            let error = ClientError::Protocol("expansion parent differs");
            self.client.owner.shared.stop(error.clone());
            return Err(error);
        }
        Ok(view)
    }
    pub fn pack(
        &self,
        references: &[String],
        timeout: Duration,
    ) -> Result<EvidencePack, ClientError> {
        let pack: EvidencePack = self.client.typed(self.client.request(
            Operation::Pack {
                session_id: self.id().into(),
                references: references.to_vec(),
            },
            timeout,
        )?)?;
        let same = pack.payload.session_id == self.id()
            && pack.payload.documents.len() == 1
            && pack.payload.documents[0].snapshot_id == self.provenance().snapshot_id
            && pack.payload.documents[0].raw_sha256 == self.provenance().raw_sha256
            && pack.payload.documents[0].text_sha256 == self.provenance().text_sha256
            && pack.payload.entries.len() == references.len()
            && pack
                .payload
                .entries
                .iter()
                .all(|v| references.contains(&v.reference));
        if !same || pack.verify_integrity().is_err() {
            let error = ClientError::Protocol("exported pack identity/integrity differs");
            self.client.owner.shared.stop(error.clone());
            return Err(error);
        }
        for view in &pack.payload.entries {
            self.verify_view(view)?;
        }
        Ok(pack)
    }
    /// The trusted worker verified these quotes against its retained snapshot
    /// before export. This marker records that trust boundary, not a signature.
    pub fn verified_pack(
        &self,
        references: &[String],
        timeout: Duration,
    ) -> Result<crate::evidence_store::VerifiedEvidence, ClientError> {
        let pack = self.pack(references, timeout)?;
        crate::evidence_store::VerifiedEvidence::from_worker(pack)
            .map_err(|_| ClientError::Protocol("worker evidence integrity differs"))
    }
    pub fn close(self, timeout: Duration) -> Result<(), ClientError> {
        let value = self.client.request(
            Operation::Close {
                session_id: self.id().into(),
            },
            timeout,
        )?;
        if value["closed"] != true || value["session_id"] != self.id() {
            return Err(ClientError::Protocol("close acknowledgement differs"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_envelopes_reject_ambiguous_success_unknown_fields_and_duplicate_ids() {
        for wire in [
            r#"{"schema_version":1,"id":"x","id":"y","ok":true,"result":{}}"#,
            r#"{"schema_version":2,"id":"x","ok":true,"result":{}}"#,
            r#"{"schema_version":1,"id":"x","ok":true,"result":{},"error":{"code":"x","message":"y"}}"#,
            r#"{"schema_version":1,"id":"x","ok":true,"result":[],"extra":false}"#,
        ] {
            assert!(decode(wire.as_bytes()).is_err());
        }
    }
    #[test]
    fn out_of_order_replies_correlate_and_late_unknown_duplicates_are_errors() {
        let (a, ar) = mpsc::sync_channel(1);
        let (b, br) = mpsc::sync_channel(1);
        let shared = Shared {
            state: Mutex::new(State {
                closed: false,
                sequence: 2,
                work_requests: 2,
                pending: BTreeMap::from([
                    (
                        "a".into(),
                        Pending {
                            management: false,
                            send: a,
                        },
                    ),
                    (
                        "b".into(),
                        Pending {
                            management: false,
                            send: b,
                        },
                    ),
                ]),
            }),
            writer: Mutex::new(None),
            child: Mutex::new(None),
            scope: Mutex::new(None),
            shutdown: Mutex::new(ClientShutdown::default()),
        };
        shared
            .deliver(br#"{"schema_version":1,"id":"b","ok":true,"result":{"value":2}}"#)
            .unwrap();
        assert!(matches!(ar.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(br.recv().unwrap().unwrap()["value"], 2);
        assert!(shared
            .deliver(br#"{"schema_version":1,"id":"other","ok":true,"result":{}}"#)
            .is_err());
        shared
            .deliver(br#"{"schema_version":1,"id":"a","ok":true,"result":{"value":1}}"#)
            .unwrap();
        assert_eq!(ar.recv().unwrap().unwrap()["value"], 1);
        assert!(shared
            .deliver(br#"{"schema_version":1,"id":"a","ok":true,"result":{}}"#)
            .is_err());
        assert!(shared.state.lock().unwrap().pending.is_empty());
    }
}
