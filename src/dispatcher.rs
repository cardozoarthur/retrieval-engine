//! Bounded native transport. ACKs are not terminal job results.
use crate::{
    document::error, parse_retrieval_request, worker::fault, CancelResult, Error, Operation,
    RequestControl, RetrievalWorker, WorkerFault, WorkerResponse, MAX_RETRIEVAL_REQUEST_BYTES,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    sync::{
        mpsc::{self, TrySendError},
        Arc, Mutex,
    },
    time::Duration,
};

const QUEUE: usize = 8;

#[derive(Clone, Debug)]
pub struct JsonlConfig {
    /// Includes queue time. Source parser/model initialization have boundary checks.
    pub request_timeout: Duration,
    /// Terminal IDs remain reserved; no unbounded ledger or unsafe ID recycling.
    pub max_request_ids: usize,
}
impl Default for JsonlConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            max_request_ids: 4096,
        }
    }
}
impl JsonlConfig {
    fn validate(&self) -> Result<(), Error> {
        if self.request_timeout < Duration::from_millis(1)
            || self.request_timeout > Duration::from_secs(300)
            || !(1..=65_536).contains(&self.max_request_ids)
        {
            return Err(error(
                "invalid JSONL limits: timeout 1..300000ms, IDs 1..65536",
            ));
        }
        Ok(())
    }
}
enum JobState {
    Pending(RequestControl, bool),
    Terminal,
}
struct Registry {
    jobs: BTreeMap<String, JobState>,
    pending: usize,
    running: bool,
    config: JsonlConfig,
}
impl Registry {
    #[cfg(test)]
    fn reserve(&mut self, id: &str) -> Result<RequestControl, WorkerFault> {
        self.reserve_timed(id, None)
    }
    fn reserve_timed(
        &mut self,
        id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<RequestControl, WorkerFault> {
        if self.jobs.contains_key(id) {
            return Err(fault(
                "duplicate_request_id",
                "Admitted request IDs cannot be reused in this process",
            ));
        }
        if self.jobs.len() >= self.config.max_request_ids {
            return Err(fault(
                "resource_limit",
                "Request ID ledger exhausted; close the pipe to release sessions and restart",
            ));
        }
        // Refuse saturation before allocating another timer. A receiver that has
        // popped a frame but not marked started may conservatively reject one.
        if self.pending >= QUEUE + usize::from(self.running) {
            return Err(fault(
                "resource_limit",
                "Pending request budget exceeded; request was not admitted",
            ));
        }
        let timeout = timeout_ms.map_or(self.config.request_timeout, |ms| {
            self.config.request_timeout.min(Duration::from_millis(ms))
        });
        let control = RequestControl::new(timeout)
            .map_err(|_| fault("internal_error", "Request control unavailable"))?;
        self.jobs
            .insert(id.to_owned(), JobState::Pending(control.clone(), false));
        self.pending += 1;
        Ok(control)
    }
    fn rollback(&mut self, id: &str) {
        if !matches!(self.jobs.get(id), Some(JobState::Pending(..))) {
            return;
        }
        if let Some(JobState::Pending(control, started)) = self.jobs.remove(id) {
            self.pending -= 1;
            if started {
                self.running = false;
            }
            control.finish();
        }
    }
    fn complete(&mut self, id: &str) {
        if let Some(state) = self.jobs.get_mut(id) {
            if let JobState::Pending(_, started) = state {
                self.pending -= 1;
                if *started {
                    self.running = false;
                }
            }
            *state = JobState::Terminal;
        }
    }
    fn started(&mut self, id: &str) {
        if let Some(JobState::Pending(_, started)) = self.jobs.get_mut(id) {
            *started = true;
            self.running = true;
        }
    }
    fn status(&self, ack_id: &str, target: &str) -> Result<serde_json::Value, WorkerFault> {
        if self.jobs.contains_key(ack_id) {
            return Err(fault(
                "duplicate_request_id",
                "Status ID collides with an admitted work ID",
            ));
        }
        match self.jobs.get(target) {
            Some(JobState::Pending(control, started)) => Ok(
                json!({"request_id":target,"state":if *started {"running"} else {"queued"},"native_interrupt_armed":control.interrupt_armed()}),
            ),
            Some(JobState::Terminal) => {
                Ok(json!({"request_id":target,"state":"finished","native_interrupt_armed":false}))
            }
            None => Err(fault(
                "unknown_request",
                "Target request was not admitted in this process",
            )),
        }
    }
    fn cancel(&self, ack_id: &str, target: &str) -> Result<serde_json::Value, WorkerFault> {
        if self.jobs.contains_key(ack_id) {
            return Err(fault(
                "duplicate_request_id",
                "Cancel ID collides with an admitted work ID",
            ));
        }
        match self.jobs.get(target) {
            Some(JobState::Pending(control, _)) => {
                Ok(json!({"request_id":target,"status":control.cancel(),"work_finished":false}))
            }
            Some(JobState::Terminal) => {
                Ok(json!({"request_id":target,"status":CancelResult::TooLate,"work_finished":true}))
            }
            None => Err(fault(
                "unknown_request",
                "Target request was not admitted in this process",
            )),
        }
    }
    fn cancel_all(&self) {
        for state in self.jobs.values() {
            if let JobState::Pending(control, _) = state {
                control.cancel();
            }
        }
    }
}
struct RegistryGuard(Arc<Mutex<Registry>>);
impl Drop for RegistryGuard {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cancel_all();
    }
}
struct Job {
    bytes: Vec<u8>,
    id: String,
    control: RequestControl,
    capabilities: bool,
}
fn response(id: Option<String>, value: Result<serde_json::Value, WorkerFault>) -> WorkerResponse {
    match value {
        Ok(result) => WorkerResponse {
            schema_version: 1,
            id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(failure) => WorkerResponse {
            schema_version: 1,
            id,
            ok: false,
            result: None,
            error: Some(failure),
        },
    }
}
fn write_reply<W: Write>(output: &Mutex<W>, reply: &WorkerResponse) -> Result<(), Error> {
    let mut output = output
        .lock()
        .map_err(|_| error("JSONL output unavailable"))?;
    serde_json::to_writer(&mut *output, reply).map_err(|_| error("JSONL serialization failed"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|_| error("JSONL output disconnected"))
}
fn read_requests<R: BufRead, W: Write>(
    mut input: R,
    output: &Mutex<W>,
    registry: &Mutex<Registry>,
    send: mpsc::SyncSender<Job>,
    task_controller: crate::OpenTaskController,
) -> Result<(), Error> {
    loop {
        let mut bytes = Vec::new();
        let read = Read::take(&mut input, MAX_RETRIEVAL_REQUEST_BYTES as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| error("JSONL input disconnected"))?;
        if read == 0 {
            return Ok(());
        }
        let oversize = bytes.len() > MAX_RETRIEVAL_REQUEST_BYTES;
        let request = match parse_retrieval_request(&bytes) {
            Ok(request) => request,
            Err(failure) => {
                let id = failure.correlation_id.clone();
                write_reply(output, &response(id, Err(failure)))?;
                if oversize {
                    return Err(error("Oversized request; worker closed"));
                }
                continue;
            }
        };
        if let Operation::Cancel { request_id } = &request.operation {
            let result = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cancel(&request.id, request_id);
            write_reply(output, &response(Some(request.id), result))?;
            continue;
        }
        if let Operation::RequestStatus { request_id } = &request.operation {
            let result = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .status(&request.id, request_id);
            write_reply(output, &response(Some(request.id), result))?;
            continue;
        }
        if matches!(
            request.operation,
            Operation::CancelOpen { .. } | Operation::TaskStatus { .. }
        ) {
            let collision = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .jobs
                .contains_key(&request.id);
            let result = if collision {
                Err(fault(
                    "duplicate_request_id",
                    "Management ID collides with an admitted work ID",
                ))
            } else {
                match &request.operation {
                    Operation::CancelOpen { task_id } => task_controller
                        .cancel(task_id)
                        .map_err(crate::worker::engine_fault),
                    Operation::TaskStatus { task_id } => task_controller
                        .status(task_id)
                        .map_err(crate::worker::engine_fault),
                    _ => unreachable!(),
                }
            };
            write_reply(output, &response(Some(request.id), result))?;
            continue;
        }
        let reservation = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve_timed(&request.id, request.timeout_ms);
        let control = match reservation {
            Ok(control) => control,
            Err(failure) => {
                write_reply(output, &response(Some(request.id), Err(failure)))?;
                continue;
            }
        };
        let job = Job {
            bytes,
            id: request.id,
            control,
            capabilities: matches!(request.operation, Operation::Capabilities),
        };
        match send.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .rollback(&job.id);
                write_reply(
                    output,
                    &response(
                        Some(job.id),
                        Err(fault(
                            "resource_limit",
                            "Request queue is full; request was not admitted",
                        )),
                    ),
                )?;
            }
            Err(TrySendError::Disconnected(job)) => {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .rollback(&job.id);
                return Err(error("JSONL worker disconnected"));
            }
        }
    }
}

/// Reads cancel commands independently of the serial engine. The consumer must
/// drain stdout: a blocked pipe cannot deliver either ACKs or terminal responses.
pub fn serve_jsonl(worker: RetrievalWorker, config: JsonlConfig) -> Result<(), Error> {
    serve_on(
        worker,
        config,
        BufReader::new(std::io::stdin()),
        std::io::stdout(),
    )
}
fn serve_on<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    mut worker: RetrievalWorker,
    config: JsonlConfig,
    input: R,
    output: W,
) -> Result<(), Error> {
    config.validate()?;
    let registry = Arc::new(Mutex::new(Registry {
        jobs: BTreeMap::new(),
        pending: 0,
        running: false,
        config: config.clone(),
    }));
    let _guard = RegistryGuard(registry.clone());
    let output = Arc::new(Mutex::new(output));
    let (send, receive) = mpsc::sync_channel(QUEUE);
    let reads = registry.clone();
    let writes = output.clone();
    let task_controller = worker.open_task_controller();
    let reader = std::thread::Builder::new()
        .name("retrieval-input".into())
        .spawn(move || {
            let result = read_requests(input, &writes, &reads, send, task_controller);
            if result.is_err() {
                reads.lock().unwrap_or_else(|p| p.into_inner()).cancel_all();
            }
            result
        })
        .map_err(|_| error("JSONL input thread unavailable"))?;
    loop {
        let job = match receive.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.maintenance();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .started(&job.id);
        let mut reply = worker.handle_json_with_control(&job.bytes, &job.control);
        let mut state = registry.lock().unwrap_or_else(|p| p.into_inner());
        state.complete(&job.id);
        if job.capabilities && reply.ok {
            if let Some(value) = reply.result.as_mut() {
                value["jsonl_cancel"] = json!(true);
                value["max_pending_requests"] = json!(QUEUE + 1);
                value["max_admitted_request_ids"] = json!(config.max_request_ids);
                value["admitted_request_ids"] = json!(state.jobs.len());
                value["request_timeout_ms"] = json!(config.request_timeout.as_millis());
            }
        }
        drop(state);
        write_reply(&output, &reply)?;
    }
    reader
        .join()
        .map_err(|_| error("JSONL input thread failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry(max: usize) -> Registry {
        Registry {
            jobs: BTreeMap::new(),
            pending: 0,
            running: false,
            config: JsonlConfig {
                max_request_ids: max,
                ..Default::default()
            },
        }
    }
    #[test]
    fn bounded_registry_cancel_targets_and_permanent_terminal_ids() {
        let mut registry = registry(2);
        let first = registry.reserve("first").unwrap();
        let other = registry.reserve("other").unwrap();
        assert_eq!(
            registry.reserve("first").unwrap_err().code,
            "duplicate_request_id"
        );
        assert_eq!(
            registry.reserve("excess").unwrap_err().code,
            "resource_limit"
        );
        assert_eq!(
            registry.cancel("cancel-first", "first").unwrap()["status"],
            "requested"
        );
        assert!(first.checkpoint().is_err());
        assert!(other.checkpoint().is_ok());
        assert_eq!(
            registry.cancel("again", "first").unwrap()["status"],
            "already_requested"
        );
        registry.complete("first");
        assert_eq!(
            registry.cancel("late", "first").unwrap()["work_finished"],
            true
        );
        assert_eq!(
            registry.reserve("first").unwrap_err().code,
            "duplicate_request_id"
        );
        assert_eq!(
            registry.cancel("unknown", "missing").unwrap_err().code,
            "unknown_request"
        );
        registry.rollback("other");
        assert!(
            registry.reserve("other").is_ok(),
            "only unadmitted work can roll back its ID"
        );
    }
    #[test]
    fn pending_controls_and_queue_are_bounded() {
        let mut registry = registry(32);
        registry.reserve("active").unwrap();
        registry.started("active");
        for id in 0..QUEUE {
            registry.reserve(&id.to_string()).unwrap();
        }
        assert_eq!(registry.reserve("over").unwrap_err().code, "resource_limit");
        registry.cancel_all();
        for job in registry.jobs.values() {
            if let JobState::Pending(control, _) = job {
                assert!(control.checkpoint().is_err());
            }
        }
    }
    #[test]
    fn configuration_and_status_observations_are_bounded() {
        for config in [
            JsonlConfig {
                request_timeout: Duration::ZERO,
                ..Default::default()
            },
            JsonlConfig {
                request_timeout: Duration::from_secs(301),
                ..Default::default()
            },
            JsonlConfig {
                max_request_ids: 0,
                ..Default::default()
            },
            JsonlConfig {
                max_request_ids: 65537,
                ..Default::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
        let mut registry = registry(4);
        registry.reserve("job").unwrap();
        assert_eq!(registry.status("s1", "job").unwrap()["state"], "queued");
        registry.started("job");
        assert_eq!(registry.status("s2", "job").unwrap()["state"], "running");
        assert_eq!(
            registry.status("s2", "job").unwrap()["native_interrupt_armed"],
            false
        );
        registry.complete("job");
        registry.rollback("job");
        assert_eq!(registry.status("s3", "job").unwrap()["state"], "finished");
        assert_eq!(
            registry.reserve("job").unwrap_err().code,
            "duplicate_request_id"
        );
        assert_eq!(registry.pending, 0);
    }
    #[test]
    fn request_timeout_can_reduce_but_never_raise_host_budget() {
        let mut host = registry(4);
        host.config.request_timeout = Duration::from_millis(5);
        let cannot_raise = host.reserve_timed("cannot-raise", Some(300000)).unwrap();
        let mut normal = registry(4);
        let reduced = normal.reserve_timed("reduced", Some(5)).unwrap();
        std::thread::sleep(Duration::from_millis(25));
        assert!(cannot_raise
            .checkpoint()
            .unwrap_err()
            .to_string()
            .contains("request deadline"));
        assert!(reduced
            .checkpoint()
            .unwrap_err()
            .to_string()
            .contains("request deadline"));
    }
}
