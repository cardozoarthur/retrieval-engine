//! Provisional anchors have their own namespace and can never masquerade as Evidence Pack references.
use crate::{document::error, Error, RequestControl};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct OpenTaskConfig {
    pub max_live: usize,
    pub max_entries: usize,
    pub max_previews: usize,
    pub idle_ttl: Duration,
    pub extraction_timeout: Duration,
}
impl Default for OpenTaskConfig {
    fn default() -> Self {
        Self {
            max_live: 2,
            max_entries: 16,
            max_previews: 64,
            idle_ttl: Duration::from_secs(300),
            extraction_timeout: Duration::from_secs(30),
        }
    }
}
impl OpenTaskConfig {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if !(1..=4).contains(&self.max_live)
            || !(self.max_live..=64).contains(&self.max_entries)
            || !(1..=256).contains(&self.max_previews)
            || self.idle_ttl < Duration::from_millis(1)
            || self.idle_ttl > Duration::from_secs(3600)
            || self.extraction_timeout < Duration::from_millis(1)
            || self.extraction_timeout > Duration::from_secs(300)
        {
            return Err(error("invalid open task budgets"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Extracting,
    Ready,
    CancelRequested,
    Cancelled,
    ExpireRequested,
    Expired,
    Failed,
    Finalized,
}
impl Phase {
    fn check(self, ready: bool) -> Result<(), Error> {
        match self {
            Self::Ready => Ok(()),
            Self::Extracting if !ready => Ok(()),
            Self::Extracting => Err(error("task not ready")),
            Self::CancelRequested | Self::Cancelled => Err(error("task cancelled")),
            Self::ExpireRequested | Self::Expired => Err(error("task expired")),
            Self::Failed => Err(error("task failed; provisional references invalidated")),
            Self::Finalized => Err(error("task already finalized")),
        }
    }
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Expired | Self::Failed | Self::Finalized
        )
    }
}
struct Gate {
    phase: Mutex<Phase>,
    control: RequestControl,
}
impl Gate {
    fn phase(&self) -> Phase {
        *self.phase.lock().unwrap_or_else(|p| p.into_inner())
    }
    #[cfg(windows)]
    fn admit<T>(
        &self,
        ready: bool,
        control: Option<&RequestControl>,
        action: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        let run = || {
            let phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            phase.check(ready)?;
            action()
        };
        match control {
            Some(control) => control.commit(run),
            None => run(),
        }
    }
    #[cfg(any(windows, test))]
    fn finish<T>(
        &self,
        control: Option<&RequestControl>,
        action: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        let run = || {
            let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            phase.check(true)?;
            let result = action()?;
            *phase = Phase::Finalized;
            Ok(result)
        };
        match control {
            Some(control) => control.commit(run),
            None => run(),
        }
    }
}
#[derive(Clone, Default)]
pub struct OpenTaskController(Arc<Mutex<BTreeMap<String, Arc<Gate>>>>);
impl OpenTaskController {
    fn get(&self, id: &str) -> Result<Arc<Gate>, Error> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| error("unknown task"))
    }
    pub fn cancel(&self, id: &str) -> Result<serde_json::Value, Error> {
        let gate = self.get(id)?;
        let mut phase = gate.phase.lock().unwrap_or_else(|p| p.into_inner());
        let status = if phase.terminal() {
            "too_late"
        } else if matches!(*phase, Phase::CancelRequested | Phase::ExpireRequested) {
            "already_requested"
        } else {
            *phase = Phase::CancelRequested;
            gate.control.cancel();
            "requested"
        };
        Ok(
            serde_json::json!({"operation":"cancel_open","task_id":id,"status":status,"work_finished":phase.terminal()}),
        )
    }
    pub fn status(&self, id: &str) -> Result<serde_json::Value, Error> {
        let gate = self.get(id)?;
        let phase = gate.phase();
        Ok(
            serde_json::json!({"operation":"task_status","task_id":id,"state":phase,"work_finished":phase.terminal()}),
        )
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::{
        document::sha, expand_text, AcquiredSource, Document, EvidenceLimits, EvidenceSession,
        EvidenceView, Expansion, ExtractionTask, IsolationConfig, IsolationReport, Limits,
        PageRange, Source, Unit,
    };
    use serde_json::{json, Value};
    use std::{
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static NEXT_TASK: AtomicU64 = AtomicU64::new(1);

    enum Data {
        Extracting(Box<ExtractionTask>),
        Ready(Arc<Document>, IsolationReport),
        Empty,
    }
    #[derive(Clone, Serialize)]
    pub(crate) struct PreviewRecord {
        task_id: String,
        ordinal: usize,
        raw_sha256: String,
        source: String,
        query_sha256: String,
        anchor_start: usize,
        anchor_end: usize,
        start: usize,
        end: usize,
        quote_sha256: String,
        pages: Vec<u32>,
        parent_reference: Option<String>,
        expansion: Option<Expansion>,
        available_chars: usize,
    }
    #[derive(Clone)]
    struct Preview {
        reference: String,
        record: PreviewRecord,
        query: Arc<str>,
    }
    struct Entry {
        gate: Arc<Gate>,
        data: Data,
        refs: BTreeMap<String, Preview>,
        reserved: usize,
        last: Instant,
        session: Option<String>,
        failure: Option<String>,
    }
    #[derive(Clone, Serialize)]
    struct Coverage {
        scope: &'static str,
        total_pages: Option<usize>,
        completed_pages: usize,
        pages_without_text: usize,
        available_chars: usize,
        end_of_source_known: bool,
        full_snapshot_verified: bool,
    }
    struct Available<'a> {
        text: &'a str,
        pages: &'a [PageRange],
        raw: &'a str,
        source: &'a str,
        coverage: Coverage,
    }
    impl Entry {
        fn available(&self) -> Option<Available<'_>> {
            match &self.data {
                Data::Extracting(task) => {
                    let source = task.source()?;
                    let pages = task.completed_pages();
                    let text = task.provisional_text();
                    let pdf = source
                        .media_type
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("application/pdf");
                    let chars = if pdf {
                        pages.last().map_or(0, |p| p.end)
                    } else {
                        text.chars().count()
                    };
                    let byte = text
                        .char_indices()
                        .nth(chars)
                        .map_or(text.len(), |(byte, _)| byte);
                    Some(Available {
                        text: &text[..byte],
                        pages,
                        raw: &source.raw_sha256,
                        source: &source.final_source,
                        coverage: Coverage {
                            scope: "extracted_text_prefix",
                            total_pages: task.total_pages(),
                            completed_pages: pages.len(),
                            pages_without_text: pages.iter().filter(|p| !p.has_text).count(),
                            available_chars: chars,
                            end_of_source_known: false,
                            full_snapshot_verified: false,
                        },
                    })
                }
                Data::Ready(doc, _) => Some(Available {
                    text: doc.text(),
                    pages: doc.pages(),
                    raw: &doc.provenance().raw_sha256,
                    source: &doc.provenance().final_source,
                    coverage: Coverage {
                        scope: "extracted_text_prefix",
                        total_pages: Some(doc.pages().len()),
                        completed_pages: doc.pages().len(),
                        pages_without_text: doc.pages().iter().filter(|p| !p.has_text).count(),
                        available_chars: doc.text().chars().count(),
                        end_of_source_known: true,
                        full_snapshot_verified: true,
                    },
                }),
                Data::Empty => None,
            }
        }
        fn coverage(&self) -> Coverage {
            self.available().map(|a| a.coverage).unwrap_or(Coverage {
                scope: "extracted_text_prefix",
                total_pages: None,
                completed_pages: 0,
                pages_without_text: 0,
                available_chars: 0,
                end_of_source_known: false,
                full_snapshot_verified: false,
            })
        }
        fn overview(&self, id: &str) -> Value {
            let phase = self.gate.phase();
            let source = match &self.data {
                Data::Extracting(task) => task.source().cloned(),
                Data::Ready(doc, _) => Some(AcquiredSource {
                    requested_source: doc.provenance().requested_source.clone(),
                    final_source: doc.provenance().final_source.clone(),
                    media_type: doc.provenance().media_type.clone(),
                    raw_bytes: doc.provenance().raw_bytes,
                    raw_sha256: doc.provenance().raw_sha256.clone(),
                }),
                Data::Empty => None,
            };
            json!({"operation":"open_task","task_id":id,"state":phase,"provisional":true,"coverage":self.coverage(),"source":source,
                "preview_references":self.refs.len(),"session_id":self.session,"failure":self.failure,"helper_exit_confirmed":!matches!(self.data,Data::Extracting(_))})
        }
        fn invalidate(&mut self, phase: Phase, failure: Option<String>) {
            // Keep the externally observed phase nonterminal until Drop waits for the child.
            let old = std::mem::replace(&mut self.data, Data::Empty);
            drop(old);
            self.gate.control.finish();
            self.refs.clear();
            self.reserved = 0;
            self.failure = failure;
            *self.gate.phase.lock().unwrap_or_else(|p| p.into_inner()) = phase;
            self.last = Instant::now();
        }
    }
    pub(crate) struct Prepared {
        pub document: Arc<Document>,
        pub session: EvidenceSession,
        pub bindings: Vec<Value>,
        pub lineage: Vec<Value>,
        pub isolation: Value,
    }
    pub(crate) struct OpenTasks {
        entries: BTreeMap<String, Entry>,
        pub controls: OpenTaskController,
        config: OpenTaskConfig,
        pub expired: u64,
        pub cancelled: u64,
    }
    impl OpenTasks {
        pub fn new(config: OpenTaskConfig) -> Self {
            Self {
                entries: BTreeMap::new(),
                controls: OpenTaskController::default(),
                config,
                expired: 0,
                cancelled: 0,
            }
        }
        pub fn reserved_chars(&self) -> usize {
            self.entries.values().map(|e| e.reserved).sum()
        }
        pub fn live(&self) -> usize {
            self.entries.values().filter(|e| e.reserved > 0).count()
        }
        pub fn maintenance(&mut self, now: Instant) {
            for entry in self.entries.values_mut() {
                let phase = entry.gate.phase();
                if phase == Phase::CancelRequested {
                    entry.invalidate(Phase::Cancelled, None);
                    self.cancelled += 1;
                    continue;
                }
                if !phase.terminal()
                    && now.saturating_duration_since(entry.last) >= self.config.idle_ttl
                {
                    *entry.gate.phase.lock().unwrap_or_else(|p| p.into_inner()) =
                        Phase::ExpireRequested;
                    entry.gate.control.cancel();
                    entry.invalidate(Phase::Expired, None);
                    self.expired += 1;
                    continue;
                }
                if phase == Phase::Extracting {
                    if let Err(failure) = entry.gate.control.checkpoint() {
                        entry.invalidate(Phase::Failed, Some(failure.to_string()));
                    }
                }
            }
            let expired: Vec<_> = self
                .entries
                .iter()
                .filter(|(_, e)| {
                    e.gate.phase().terminal()
                        && now.saturating_duration_since(e.last) >= self.config.idle_ttl
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in expired {
                self.entries.remove(&id);
                self.controls
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
            }
        }
        pub fn start(
            &mut self,
            program: &Path,
            source: Source,
            limits: Limits,
            isolation: IsolationConfig,
            control: Option<&RequestControl>,
        ) -> Result<Value, Error> {
            self.maintenance(Instant::now());
            if self.live() >= self.config.max_live {
                return Err(error("open task live budget exceeded"));
            }
            if self.entries.len() >= self.config.max_entries {
                let oldest = self
                    .entries
                    .iter()
                    .filter(|(_, e)| e.gate.phase().terminal())
                    .min_by_key(|(_, e)| e.last)
                    .map(|(id, _)| id.clone());
                if let Some(id) = oldest {
                    self.entries.remove(&id);
                    self.controls
                        .0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                } else {
                    return Err(error("open task record budget exceeded"));
                }
            }
            let source_control = RequestControl::new(self.config.extraction_timeout)?;
            let _handoff = if let Some(control) = control {
                let source = source_control.clone();
                Some(control.interrupt(Arc::new(move || {
                    source.cancel();
                }))?)
            } else {
                None
            };
            let reserved = limits.max_text_chars;
            let task =
                ExtractionTask::start(program, source, limits, isolation, source_control.clone())?;
            let nonce = NEXT_TASK.fetch_add(1, Ordering::Relaxed);
            let id = format!(
                "t_{}",
                sha(&serde_json::to_vec(&(
                    "open-task/1",
                    std::process::id(),
                    nonce,
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(error)?
                        .as_nanos()
                ))
                .map_err(error)?)
            );
            let gate = Arc::new(Gate {
                phase: Mutex::new(Phase::Extracting),
                control: source_control,
            });
            let entry = Entry {
                gate: gate.clone(),
                data: Data::Extracting(Box::new(task)),
                refs: BTreeMap::new(),
                reserved,
                last: Instant::now(),
                session: None,
                failure: None,
            };
            let response = entry.overview(&id);
            let action = || {
                self.entries.insert(id.clone(), entry);
                self.controls
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id, gate);
                Ok(response)
            };
            match control {
                Some(control) => control.commit(action),
                None => action(),
            }
        }
        fn entry(&mut self, id: &str) -> Result<&mut Entry, Error> {
            let entry = self
                .entries
                .get_mut(id)
                .ok_or_else(|| error("unknown task"))?;
            entry.last = Instant::now();
            Ok(entry)
        }
        pub fn poll(
            &mut self,
            id: &str,
            max_events: usize,
            control: Option<&RequestControl>,
        ) -> Result<Value, Error> {
            if !(1..=64).contains(&max_events) {
                return Err(error("open task poll budget exceeded"));
            }
            self.maintenance(Instant::now());
            let entry = self.entry(id)?;
            if entry.gate.phase() == Phase::Extracting {
                let mut failure = None;
                if let Data::Extracting(task) = &mut entry.data {
                    for _ in 0..max_events {
                        if let Some(control) = control {
                            control.checkpoint()?;
                        }
                        match task.next_event(Duration::ZERO) {
                            Ok(Some(_)) => {}
                            Ok(None) => break,
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                }
                if let Some(failure) = failure {
                    entry.invalidate(Phase::Failed, Some(failure.to_string()));
                } else {
                    let ready = if let Data::Extracting(task) = &mut entry.data {
                        task.terminal_received() && !task.helper_is_running()?
                    } else {
                        false
                    };
                    if ready {
                        let old = std::mem::replace(&mut entry.data, Data::Empty);
                        let Data::Extracting(task) = old else {
                            unreachable!()
                        };
                        match task.finish() {
                            Ok((document, report)) => {
                                let mut phase =
                                    entry.gate.phase.lock().unwrap_or_else(|p| p.into_inner());
                                if *phase == Phase::Extracting {
                                    entry.reserved = document.text().chars().count();
                                    entry.data = Data::Ready(Arc::new(document), report);
                                    *phase = Phase::Ready;
                                    entry.gate.control.finish();
                                }
                            }
                            Err(failure) => {
                                entry.invalidate(Phase::Failed, Some(failure.to_string()))
                            }
                        }
                    }
                }
            }
            let value = entry.overview(id);
            match control {
                Some(c) => c.commit(|| Ok(value)),
                None => Ok(value),
            }
        }
        pub fn peek(
            &mut self,
            id: &str,
            query: &str,
            top_k: usize,
            limits: EvidenceLimits,
            control: Option<&RequestControl>,
        ) -> Result<Value, Error> {
            self.maintenance(Instant::now());
            if query.trim().is_empty() || query.len() > 4096 || !(1..=32).contains(&top_k) {
                return Err(error("invalid provisional query"));
            }
            let max_refs = self.config.max_previews;
            let entry = self.entry(id)?;
            entry.gate.phase().check(false)?;
            let mut prepared = Vec::new();
            let coverage = entry.coverage();
            if let Some(available) = entry.available() {
                let mut scalar = 0;
                let mut previous = 0;
                let mut chars = 0;
                let query: Arc<str> = Arc::from(query);
                for (byte, text) in available.text.match_indices(query.as_ref()).take(top_k) {
                    if let Some(control) = control {
                        control.checkpoint()?;
                    }
                    scalar += available.text[previous..byte].chars().count();
                    previous = byte;
                    let end = scalar + text.chars().count();
                    chars += end - scalar;
                    if end - scalar > limits.max_context_chars || chars > limits.max_pack_chars {
                        return Err(error("provisional quote budget exceeded"));
                    }
                    let record = PreviewRecord {
                        task_id: id.into(),
                        ordinal: entry.refs.len() + prepared.len() + 1,
                        raw_sha256: available.raw.into(),
                        source: available.source.into(),
                        query_sha256: sha(query.as_bytes()),
                        anchor_start: scalar,
                        anchor_end: end,
                        start: scalar,
                        end,
                        quote_sha256: sha(text.as_bytes()),
                        pages: pages_for(available.pages, scalar, end),
                        parent_reference: None,
                        expansion: None,
                        available_chars: coverage.available_chars,
                    };
                    prepared.push(make_preview(record, query.clone())?);
                }
            }
            if entry.refs.len() + prepared.len() > max_refs {
                return Err(error("provisional reference budget exceeded"));
            }
            let mut views = Vec::new();
            for p in &prepared {
                if let Some(control) = control {
                    control.checkpoint()?;
                }
                views.push(view(entry, p)?);
            }
            let gate = entry.gate.clone();
            gate.admit(false,control,|| {for preview in prepared {entry.refs.insert(preview.reference.clone(),preview);}Ok(json!({"operation":"peek_open","task_id":id,"provisional":true,"mode":"literal","coverage":coverage,"entries":views}))})
        }
        pub fn expand(
            &mut self,
            id: &str,
            reference: &str,
            expansion: Expansion,
            limits: EvidenceLimits,
            control: Option<&RequestControl>,
        ) -> Result<Value, Error> {
            let Expansion {
                before,
                after,
                unit,
            } = expansion;
            self.maintenance(Instant::now());
            let max_refs = self.config.max_previews;
            let entry = self.entry(id)?;
            entry.gate.phase().check(false)?;
            if entry.refs.len() >= max_refs {
                return Err(error("provisional reference budget exceeded"));
            }
            let parent = entry
                .refs
                .get(reference)
                .ok_or_else(|| error("unknown provisional reference"))?
                .clone();
            let available = entry.available().ok_or_else(|| error("task not ready"))?;
            verify_preview(&available, &parent)?;
            let expanded = expand_text(
                available.source,
                available.text,
                (parent.record.start, parent.record.end),
                before,
                after,
                unit,
                limits.max_context_chars,
            )
            .map_err(error)?;
            let mut record = parent.record.clone();
            record.ordinal = entry.refs.len() + 1;
            record.start = expanded.start;
            record.end = expanded.end;
            record.quote_sha256 = sha(expanded.text.as_bytes());
            record.pages = pages_for(available.pages, expanded.start, expanded.end);
            record.parent_reference = Some(reference.into());
            record.expansion = Some(Expansion {
                before,
                after,
                unit,
            });
            record.available_chars = available.coverage.available_chars;
            let preview = make_preview(record, parent.query)?;
            let value = view(entry, &preview)?;
            let gate = entry.gate.clone();
            gate.admit(false, control, || {
                entry.refs.insert(preview.reference.clone(), preview);
                Ok(json!({"operation":"expand_open","task_id":id,"provisional":true,"entry":value}))
            })
        }
        pub fn prepare_finish(
            &mut self,
            id: &str,
            references: &[String],
            limits: EvidenceLimits,
            control: Option<&RequestControl>,
        ) -> Result<Prepared, Error> {
            self.maintenance(Instant::now());
            let entry = self.entry(id)?;
            entry.gate.phase().check(true)?;
            let Data::Ready(document, isolation) = &entry.data else {
                return Err(error("task not ready"));
            };
            let available = entry.available().ok_or_else(|| error("task not ready"))?;
            let mut session = EvidenceSession::new(vec![document.clone()], limits)?;
            let mut bindings = Vec::new();
            let mut audit = BTreeMap::new();
            let mut total = 0;
            for reference in references {
                if let Some(control) = control {
                    control.checkpoint()?;
                }
                let preview = entry
                    .refs
                    .get(reference)
                    .ok_or_else(|| error("unknown provisional reference"))?;
                verify_preview(&available, preview)?;
                total += preview.record.end - preview.record.start;
                if total > limits.max_pack_chars {
                    return Err(error("provisional binding budget exceeded"));
                }
                let anchor = document.read_span(
                    preview.record.anchor_start,
                    preview.record.anchor_end,
                    limits.max_context_chars,
                )?;
                let root = session.register(&preview.query, &anchor)?;
                let final_view: EvidenceView = if preview.record.start
                    == preview.record.anchor_start
                    && preview.record.end == preview.record.anchor_end
                {
                    root
                } else {
                    session.expand(
                        &root.reference,
                        preview.record.anchor_start - preview.record.start,
                        preview.record.end - preview.record.anchor_end,
                        Unit::Characters,
                    )?
                };
                if final_view.evidence.start != preview.record.start
                    || final_view.evidence.end != preview.record.end
                    || sha(final_view.evidence.text.as_bytes()) != preview.record.quote_sha256
                    || final_view.evidence.pages != preview.record.pages
                {
                    return Err(error("provisional binding mismatch"));
                }
                bindings.push(json!({"provisional_reference":reference,"preview":preview.record,"final":final_view}));
                let mut cursor = Some(reference.as_str());
                while let Some(id) = cursor {
                    let p = entry
                        .refs
                        .get(id)
                        .ok_or_else(|| error("unknown provisional ancestor"))?;
                    audit.insert(
                        p.record.ordinal,
                        json!({"reference":p.reference,"record":p.record}),
                    );
                    cursor = p.record.parent_reference.as_deref();
                }
            }
            Ok(Prepared {
                document: document.clone(),
                session,
                bindings,
                lineage: audit.into_values().collect(),
                isolation: serde_json::to_value(isolation).map_err(error)?,
            })
        }
        pub fn finalize<T>(
            &mut self,
            id: &str,
            session_id: String,
            control: Option<&RequestControl>,
            insert: impl FnOnce() -> Result<T, Error>,
        ) -> Result<T, Error> {
            let entry = self.entry(id)?;
            let gate = entry.gate.clone();
            gate.finish(control, || {
                let result = insert()?;
                entry.data = Data::Empty;
                entry.refs.clear();
                entry.reserved = 0;
                entry.session = Some(session_id);
                Ok(result)
            })
        }
    }
    impl Drop for OpenTasks {
        fn drop(&mut self) {
            for entry in self.entries.values_mut() {
                if !entry.gate.phase().terminal() {
                    *entry.gate.phase.lock().unwrap_or_else(|p| p.into_inner()) =
                        Phase::CancelRequested;
                    entry.gate.control.cancel();
                    entry.invalidate(Phase::Cancelled, None);
                }
            }
            self.controls
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        }
    }
    fn pages_for(pages: &[PageRange], start: usize, end: usize) -> Vec<u32> {
        pages
            .iter()
            .filter(|p| p.start < end && p.end > start)
            .map(|p| p.number)
            .collect()
    }
    fn make_preview(record: PreviewRecord, query: Arc<str>) -> Result<Preview, Error> {
        let reference = format!("p_{}", crate::evidence::canonical_hash(&record)?);
        Ok(Preview {
            reference,
            record,
            query,
        })
    }
    fn verify_preview(available: &Available<'_>, preview: &Preview) -> Result<(), Error> {
        let r = &preview.record;
        let text = quote(available, r)?;
        if r.raw_sha256 != available.raw
            || r.source != available.source
            || r.quote_sha256 != sha(text.as_bytes())
            || r.pages != pages_for(available.pages, r.start, r.end)
        {
            return Err(error("provisional version mismatch"));
        }
        Ok(())
    }
    fn view(entry: &Entry, preview: &Preview) -> Result<Value, Error> {
        let available = entry.available().ok_or_else(|| error("task not ready"))?;
        verify_preview(&available, preview)?;
        let text = quote(&available, &preview.record)?;
        Ok(
            json!({"reference":preview.reference,"record":preview.record,"text":text,"provisional":true,"coverage":available.coverage,
            "after_limited_by_progress":!available.coverage.end_of_source_known &&preview.record.end==available.coverage.available_chars}),
        )
    }
    fn quote<'a>(available: &Available<'a>, record: &PreviewRecord) -> Result<&'a str, Error> {
        if record.start >= record.end
            || record.end > available.coverage.available_chars
            || record.end - record.start > 65536
        {
            return Err(error("provisional span rejected"));
        }
        let start = available
            .text
            .char_indices()
            .nth(record.start)
            .map(|(byte, _)| byte)
            .ok_or_else(|| error("provisional span rejected"))?;
        let end = available.text[start..]
            .char_indices()
            .nth(record.end - record.start)
            .map_or(available.text.len(), |(byte, _)| start + byte);
        Ok(&available.text[start..end])
    }
}
#[cfg(windows)]
pub(crate) use windows::OpenTasks;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };
    #[test]
    fn finalize_and_cancel_serialize_before_canonical_admission() {
        for _ in 0..32 {
            let source = RequestControl::new(Duration::from_secs(5)).unwrap();
            source.finish();
            let gate = Arc::new(Gate {
                phase: Mutex::new(Phase::Ready),
                control: source,
            });
            let controller = OpenTaskController::default();
            controller
                .0
                .lock()
                .unwrap()
                .insert("task".into(), gate.clone());
            let barrier = Arc::new(Barrier::new(2));
            let other = barrier.clone();
            let cancel = std::thread::spawn(move || {
                other.wait();
                controller.cancel("task").unwrap()
            });
            let writes = AtomicUsize::new(0);
            barrier.wait();
            let result = gate.finish(None, || {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            let cancelled = cancel.join().unwrap();
            if result.is_ok() {
                assert_eq!(writes.load(Ordering::SeqCst), 1);
                assert_eq!(cancelled["status"], "too_late");
            } else {
                assert_eq!(writes.load(Ordering::SeqCst), 0);
                assert_eq!(cancelled["status"], "requested");
            }
        }
    }
}
