use retrieval_engine::{Document, EvidencePack, Limits, Source};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

struct WorkerProcess {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<String>,
}
impl WorkerProcess {
    fn start(root: Option<&std::path::Path>, cpu: bool) -> Self {
        Self::start_with_ttl(root, cpu, None)
    }
    fn start_with_ttl(root: Option<&std::path::Path>, cpu: bool, ttl_ms: Option<u64>) -> Self {
        Self::start_options(root, cpu, ttl_ms, None, None)
    }
    fn start_options(
        root: Option<&std::path::Path>,
        cpu: bool,
        ttl_ms: Option<u64>,
        timeout_ms: Option<u64>,
        max_ids: Option<usize>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_retrieval-worker"));
        command.current_dir(env!("CARGO_MANIFEST_DIR"));
        if let Some(root) = root {
            command.arg("--local-root").arg(root);
        }
        if cpu {
            command.args(["--model-dir", "assets/e5-small-int8"]);
        }
        if let Some(ttl_ms) = ttl_ms {
            command.arg("--session-ttl-ms").arg(ttl_ms.to_string());
        }
        if let Some(timeout) = timeout_ms {
            command.arg("--request-timeout-ms").arg(timeout.to_string());
        }
        if let Some(limit) = max_ids {
            command.arg("--max-request-ids").arg(limit.to_string());
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let input = child.stdin.take();
        let (send, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if send.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            input,
            replies,
        }
    }
    fn raw(&mut self, line: &str) -> Value {
        writeln!(self.input.as_mut().unwrap(), "{line}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
        self.receive()
    }
    fn call(&mut self, request: Value) -> Value {
        self.send(request);
        self.receive()
    }
    fn send(&mut self, request: Value) {
        writeln!(self.input.as_mut().unwrap(), "{request}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }
    fn receive(&self) -> Value {
        serde_json::from_str(
            &self
                .replies
                .recv_timeout(Duration::from_secs(45))
                .expect("Native worker reply timeout"),
        )
        .unwrap()
    }
    #[cfg(feature = "cpu-onnx")]
    fn until(&self, id: &str, pending: &mut Vec<Value>) -> Value {
        loop {
            let reply = self.receive();
            if reply["id"] == id {
                return reply;
            }
            pending.push(reply);
        }
    }
    fn finish(&mut self) {
        drop(self.input.take());
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "Worker did not exit on EOF"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn actual_native_bounded_backlog_preserves_admitted_fifo_and_replies_to_every_frame() {
    let mut worker = WorkerProcess::start(None, false);
    for id in 0..32 {
        writeln!(
            worker.input.as_mut().unwrap(),
            "{}",
            json!({"schema_version":1,"id":id.to_string(),"op":"capabilities"})
        )
        .unwrap();
    }
    worker.input.as_mut().unwrap().flush().unwrap();
    // Saturation now rejects immediately to keep the reader available for cancel.
    // Admitted work still preserves FIFO and drains before EOF.
    drop(worker.input.take());
    let mut seen = std::collections::BTreeSet::new();
    let mut admitted = Vec::new();
    for _ in 0..32 {
        let reply: Value = serde_json::from_str(
            &worker
                .replies
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
        )
        .unwrap();
        let id = reply["id"].as_str().unwrap().parse::<usize>().unwrap();
        assert!(id < 32 && seen.insert(id));
        if reply["ok"] == true {
            admitted.push(id);
        } else {
            assert_eq!(reply["error"]["code"], "resource_limit");
        }
    }
    assert!(!admitted.is_empty());
    assert!(admitted.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(seen.len(), 32);
    worker.finish();
}

#[test]
fn actual_native_oversize_frame_reports_limit_and_closes() {
    let mut worker = WorkerProcess::start(None, false);
    worker
        .input
        .as_mut()
        .unwrap()
        .write_all(&vec![
            b'x';
            retrieval_engine::MAX_RETRIEVAL_REQUEST_BYTES + 1
        ])
        .unwrap();
    worker.input.as_mut().unwrap().flush().unwrap();
    let reply = worker.receive();
    assert_eq!(reply["error"]["code"], "resource_limit");
    assert!(reply["id"].is_null());
    drop(worker.input.take());
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = worker.child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < until,
            "Oversized input did not close the owned worker"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn actual_native_request_ledger_limits_ids_and_management_remains_available() {
    let mut worker = WorkerProcess::start_options(None, false, None, None, Some(2));
    let first = worker.call(json!({"schema_version":1,"id":"a","op":"capabilities"}));
    assert_eq!(first["result"]["jsonl_cancel"], true);
    assert_eq!(first["result"]["max_admitted_request_ids"], 2);
    let status = worker
        .call(json!({"schema_version":1,"id":"status-a","op":"request_status","request_id":"a"}));
    assert_eq!(status["result"]["state"], "finished");
    let duplicate = worker.call(json!({"schema_version":1,"id":"a","op":"capabilities"}));
    assert_eq!(duplicate["error"]["code"], "duplicate_request_id");
    assert_eq!(
        worker.call(json!({"schema_version":1,"id":"b","op":"capabilities"}))["ok"],
        true
    );
    assert_eq!(
        worker.call(json!({"schema_version":1,"id":"c","op":"capabilities"}))["error"]["code"],
        "resource_limit"
    );
    let late =
        worker.call(json!({"schema_version":1,"id":"cancel-a","op":"cancel","request_id":"a"}));
    assert_eq!(late["result"]["status"], "too_late");
    assert_eq!(late["result"]["work_finished"], true);
    assert_eq!(
        worker.call(json!({"schema_version":1,"id":"missing","op":"cancel","request_id":"c"}))
            ["error"]["code"],
        "unknown_request"
    );
    worker.finish();
}

#[cfg(feature = "cpu-onnx")]
fn long_uncached_query() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tokenizer =
        tokenizers::Tokenizer::from_file(root.join("assets/e5-small-int8/tokenizer.json")).unwrap();
    for word in [
        "inconstitucionalissimamente",
        "hiperconstitucionalização",
        "desproporcionalizadamente",
    ] {
        let mut text = String::new();
        for _ in 0..128 {
            let next = format!("{text}{word} ");
            if tokenizer
                .encode(format!("query: {next}"), true)
                .unwrap()
                .len()
                > 512
            {
                break;
            }
            text = next;
        }
        if tokenizer
            .encode(format!("query: {text}"), true)
            .unwrap()
            .len()
            > 480
        {
            return text;
        }
    }
    panic!("No bounded lexical query filled the real E5 window");
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "actual source-task cancellation bypasses an unrelated E5 request in the JSONL worker"]
fn actual_cancel_open_during_inference_preserves_the_inference_session() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut worker = WorkerProcess::start(Some(&root), true);
    let open=worker.call(json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":root.join("two-pages.pdf")}}));
    let sid = open["result"]["session_id"].clone();
    let warm=worker.call(json!({"schema_version":1,"id":"warm","op":"search","session_id":sid,"mode":"hybrid","query":"prova","top_k":1}));
    assert_eq!(warm["ok"], true, "{warm}");
    let task=worker.call(json!({"schema_version":1,"id":"task","op":"start_open","source":{"kind":"local","path":root.join("progressive.pdf")}}));
    let task_id = task["result"]["task_id"].clone();
    assert!(task_id.is_string(), "{task}");
    worker.send(json!({"schema_version":1,"id":"active-model","op":"search","session_id":sid,"mode":"hybrid","query":long_uncached_query(),"top_k":1}));
    let mut pending = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    for number in 0.. {
        assert!(Instant::now() < end);
        let id = format!("model-status-{number}");
        worker.send(
            json!({"schema_version":1,"id":id,"op":"request_status","request_id":"active-model"}),
        );
        let status = worker.until(&id, &mut pending);
        assert_ne!(status["result"]["state"], "finished");
        if status["result"]["native_interrupt_armed"] == true {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    worker.send(
        json!({"schema_version":1,"id":"cancel-source","op":"cancel_open","task_id":task_id}),
    );
    let cancelled = worker.until("cancel-source", &mut pending);
    assert_eq!(cancelled["result"]["status"], "requested", "{cancelled}");
    assert!(
        pending.is_empty(),
        "source cancellation must bypass the busy model request"
    );
    let active = worker.until("active-model", &mut pending);
    assert_eq!(active["ok"], true, "{active}");
    assert_eq!(
        active["result"]["search"]["model"]["load_ms"],
        warm["result"]["search"]["model"]["load_ms"]
    );
    let state = worker
        .call(json!({"schema_version":1,"id":"poll-source","op":"poll_open","task_id":task_id}));
    assert_eq!(state["result"]["state"], "cancelled");
    assert_eq!(
        worker.call(json!({"schema_version":1,"id":"caps","op":"capabilities"}))["result"]
            ["active_sessions"],
        1
    );
    worker.finish();
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "real JSONL deadline during resident E5 work and non-preemptible model loading boundary"]
fn actual_jsonl_deadline_never_admits_partial_references() {
    let query = long_uncached_query();
    let fixture = Fixture::new("jsonl-deadline");
    let mut worker = WorkerProcess::start(Some(&fixture.root), true);
    let open=worker.call(json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":fixture.file}}));
    assert_eq!(open["ok"], true, "{open}");
    let sid = open["result"]["session_id"].clone();
    // Pinned model initialization is checked at its boundary, not hard-preempted.
    let warm=worker.call(json!({"schema_version":1,"id":"warm","op":"search","session_id":sid,"mode":"hybrid","query":"danos","top_k":1,"timeout_ms":30}));
    assert_eq!(warm["error"]["code"], "deadline_exceeded", "{warm}");
    let caps = worker.call(json!({"schema_version":1,"id":"caps","op":"capabilities"}));
    assert_eq!(caps["result"]["model_loaded"], true);
    assert_eq!(caps["result"]["request_timeout_ms"], 30000);
    let timed=worker.call(json!({"schema_version":1,"id":"timed","op":"search","session_id":sid,"mode":"hybrid","query":query,"top_k":1,"timeout_ms":30}));
    assert_eq!(timed["error"]["code"], "deadline_exceeded", "{timed}");
    let literal=worker.call(json!({"schema_version":1,"id":"after","op":"search","session_id":sid,"mode":"literal","query":"danos","top_k":1}));
    assert_eq!(literal["result"]["entries"][0]["record"]["ordinal"], 1);
    worker.finish();
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "real JSONL active/queued E5 cancellation, full queue and resident-model reuse"]
fn actual_jsonl_cancel_reaches_native_inference_and_queued_work() {
    let fixture = Fixture::new("jsonl-cancel");
    let mut worker = WorkerProcess::start(Some(&fixture.root), true);
    let open = |id: &str| json!({"schema_version":1,"id":id,"op":"open","source":{"kind":"local","path":fixture.file}});
    let first = worker.call(open("open-first"))["result"]["session_id"].clone();
    let second = worker.call(open("open-second"))["result"]["session_id"].clone();
    let other=worker.call(json!({"schema_version":1,"id":"other","op":"search","session_id":second,"mode":"literal","query":"danos","top_k":1}));
    let reference = other["result"]["entries"][0]["reference"].clone();
    let warm=worker.call(json!({"schema_version":1,"id":"warm","op":"search","session_id":first,"mode":"hybrid","query":"danos","top_k":1}));
    assert_eq!(warm["ok"], true, "{warm}");
    worker.send(json!({"schema_version":1,"id":"active","op":"search","session_id":first,"mode":"hybrid","query":long_uncached_query(),"top_k":1}));
    let mut pending = Vec::new();
    let until = Instant::now() + Duration::from_secs(10);
    for poll in 0.. {
        assert!(Instant::now() < until);
        let id = format!("poll-{poll}");
        worker
            .send(json!({"schema_version":1,"id":id,"op":"request_status","request_id":"active"}));
        let status = worker.until(&id, &mut pending);
        assert_eq!(status["ok"], true, "{status}");
        assert_ne!(status["result"]["state"], "finished");
        if status["result"]["native_interrupt_armed"] == true {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    for id in 0..8 {
        worker.send(json!({"schema_version":1,"id":format!("queue-{id}"),"op":"capabilities"}));
    }
    worker.send(json!({"schema_version":1,"id":"over","op":"capabilities"}));
    assert_eq!(
        worker.until("over", &mut pending)["error"]["code"],
        "resource_limit"
    );
    worker.send(json!({"schema_version":1,"id":"queued-status","op":"request_status","request_id":"queue-7"}));
    assert_eq!(
        worker.until("queued-status", &mut pending)["result"]["state"],
        "queued"
    );
    worker.send(
        json!({"schema_version":1,"id":"cancel-queued","op":"cancel","request_id":"queue-7"}),
    );
    let queued = worker.until("cancel-queued", &mut pending);
    assert_eq!(queued["result"]["status"], "requested");
    assert_eq!(queued["result"]["work_finished"], false);
    worker
        .send(json!({"schema_version":1,"id":"cancel-active","op":"cancel","request_id":"active"}));
    let ack = worker.until("cancel-active", &mut pending);
    assert_eq!(ack["result"]["status"], "requested");
    while pending.len() < 9 {
        pending.push(worker.receive());
    }
    let by_id: std::collections::BTreeMap<_, _> = pending
        .into_iter()
        .map(|r| (r["id"].as_str().unwrap().to_string(), r))
        .collect();
    assert_eq!(by_id.len(), 9);
    assert_eq!(by_id["active"]["error"]["code"], "cancelled");
    assert_eq!(by_id["queue-7"]["error"]["code"], "cancelled");
    for id in 0..7 {
        assert_eq!(by_id[&format!("queue-{id}")]["ok"], true);
    }
    let reused=worker.call(json!({"schema_version":1,"id":"reused","op":"search","session_id":first,"mode":"hybrid","query":"danos","top_k":1}));
    assert_eq!(reused["result"]["entries"][0]["record"]["ordinal"], 2);
    assert_eq!(
        reused["result"]["search"]["model"]["load_ms"],
        warm["result"]["search"]["model"]["load_ms"]
    );
    assert_eq!(reused["result"]["search"]["model"]["interrupted_runs"], 1);
    let other=worker.call(json!({"schema_version":1,"id":"other-expand","op":"expand","session_id":second,"reference":reference,"before":1,"after":1,"unit":"words"}));
    assert_eq!(other["ok"], true, "{other}");
    assert_eq!(other["result"]["record"]["ordinal"], 2);
    worker.finish();
}

#[test]
#[cfg(windows)]
fn actual_native_cancel_open_reaps_helper_and_preserves_existing_session() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut worker = WorkerProcess::start(Some(&root), false);
    let open=worker.call(json!({"schema_version":1,"id":"other-open","op":"open","source":{"kind":"local","path":root.join("two-pages.pdf")}}));
    assert_eq!(open["ok"], true, "{open}");
    assert_eq!(open["result"]["isolation"]["helper_exit_confirmed"], true);
    let sid = open["result"]["session_id"].clone();
    let found=worker.call(json!({"schema_version":1,"id":"other-search","op":"search","session_id":sid,"mode":"literal","query":"prova","top_k":1}));
    let reference = found["result"]["entries"][0]["reference"].clone();
    worker.send(json!({"schema_version":1,"id":"opening","op":"open","source":{"kind":"local","path":root.join("progressive.pdf")}}));
    let until = Instant::now() + Duration::from_secs(5);
    for poll in 0.. {
        assert!(Instant::now() < until);
        let status=worker.call(json!({"schema_version":1,"id":format!("poll-open-{poll}"),"op":"request_status","request_id":"opening"}));
        assert!(
            status["id"].as_str().unwrap().starts_with("poll-open-"),
            "open completed before cancellation could be observed: {status}"
        );
        if status["result"]["native_interrupt_armed"] == true {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    worker
        .send(json!({"schema_version":1,"id":"cancel-open","op":"cancel","request_id":"opening"}));
    let mut frames = Vec::new();
    while frames.len() < 2 {
        frames.push(worker.receive());
    }
    let by_id: std::collections::BTreeMap<_, _> = frames
        .into_iter()
        .map(|r| (r["id"].as_str().unwrap().to_string(), r))
        .collect();
    assert_eq!(by_id["cancel-open"]["result"]["status"], "requested");
    assert_eq!(by_id["opening"]["error"]["code"], "cancelled");
    let caps = worker.call(json!({"schema_version":1,"id":"caps-after","op":"capabilities"}));
    assert_eq!(caps["result"]["active_sessions"], 1);
    assert_eq!(caps["result"]["isolated_extraction"], true);
    let other=worker.call(json!({"schema_version":1,"id":"other-expand","op":"expand","session_id":sid,"reference":reference,"before":1,"after":1,"unit":"words"}));
    assert_eq!(other["ok"], true, "{other}");
    worker.finish();
}

#[test]
fn actual_native_idle_ttl_releases_source_and_rejects_old_refs_without_reading_source() {
    let fixture = Fixture::new("idle-ttl");
    let mut worker = WorkerProcess::start_with_ttl(Some(&fixture.root), false, Some(500));
    let open = worker.call(json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":fixture.file}}));
    assert_eq!(open["ok"], true, "{open}");
    let sid = open["result"]["session_id"].clone();
    let search = worker.call(json!({"schema_version":1,"id":"s","op":"search","session_id":sid,"mode":"bm25","query":"danos","top_k":1}));
    assert_eq!(search["ok"], true, "{search}");
    let reference = search["result"]["entries"][0]["reference"].clone();
    // Removing the exact test-owned file makes accidental reacquisition observable.
    std::fs::remove_file(&fixture.file).unwrap();
    thread::sleep(Duration::from_millis(900));
    let caps = worker.call(json!({"schema_version":1,"id":"c","op":"capabilities"}));
    assert_eq!(caps["result"]["active_sessions"], 0);
    assert_eq!(caps["result"]["retained_source_chars"], 0);
    assert_eq!(caps["result"]["expired_sessions"], 1);
    // This counter is only incremented by maintenance, not request-boundary cleanup.
    assert_eq!(caps["result"]["idle_expired_sessions"], 1);
    let expired = worker.call(json!({"schema_version":1,"id":"e","op":"expand","session_id":sid,"reference":reference,"before":10,"after":10,"unit":"words"}));
    assert_eq!(expired["error"]["code"], "unknown_session");
    worker.finish();
}
impl Drop for WorkerProcess {
    fn drop(&mut self) {
        drop(self.input.take());
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
struct Fixture {
    root: PathBuf,
    file: PathBuf,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join(format!("native-retrieval-{name}-{}", std::process::id()));
        assert!(root.starts_with(base));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("source.txt");
        std::fs::write(
            &file,
            "O agente responde pelos danos. Não se presume culpa; exceção exige prova.",
        )
        .unwrap();
        Self { root, file }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.root);
    }
}

#[test]
fn actual_native_sessions_expand_export_and_expire_on_restart() {
    let fixture = Fixture::new("lexical");
    let original = Document::load(Source::Local(fixture.file.clone()), &Limits::default()).unwrap();
    let mut worker = WorkerProcess::start(Some(&fixture.root), false);
    assert_eq!(worker.raw("not-json")["error"]["code"], "invalid_request");
    let opened=worker.call(json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":fixture.file}}));
    assert_eq!(opened["ok"], true, "{opened}");
    let sid = opened["result"]["session_id"].as_str().unwrap().to_string();
    std::fs::write(&fixture.file, "Texto substituído depois de open.").unwrap();
    let searched=worker.call(json!({"schema_version":1,"id":"q","op":"search","session_id":sid,"mode":"bm25","query":"danos culpa","top_k":1}));
    assert_eq!(searched["ok"], true, "{searched}");
    let reference = searched["result"]["entries"][0]["reference"]
        .as_str()
        .unwrap();
    let expanded=worker.call(json!({"schema_version":1,"id":"e","op":"expand","session_id":sid,"reference":reference,"before":10,"after":10,"unit":"characters"}));
    assert_eq!(expanded["ok"], true, "{expanded}");
    assert!(expanded["result"]["evidence"]["text"]
        .as_str()
        .unwrap()
        .contains("Não se presume culpa"));
    let reference = expanded["result"]["reference"].as_str().unwrap();
    let packed = worker.call(
        json!({"schema_version":1,"id":"p","op":"pack","session_id":sid,"references":[reference]}),
    );
    assert_eq!(packed["ok"], true, "{packed}");
    let pack: EvidencePack = serde_json::from_value(packed["result"].clone()).unwrap();
    pack.verify_against(&[&original]).unwrap();
    worker.finish();
    let mut restarted = WorkerProcess::start(None, false);
    let stale=restarted.call(json!({"schema_version":1,"id":"old","op":"expand","session_id":sid,"reference":reference,"before":1,"after":1,"unit":"words"}));
    assert_eq!(stale["error"]["code"], "unknown_session");
    restarted.finish();
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "actual pinned CPU model shared across retrieval worker requests"]
fn actual_native_hybrid_reuses_one_model_and_snapshot() {
    let fixture = Fixture::new("cpu");
    let mut worker = WorkerProcess::start(Some(&fixture.root), true);
    let opened=worker.call(json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":fixture.file}}));
    let sid = opened["result"]["session_id"].as_str().unwrap();
    let first=worker.call(json!({"schema_version":1,"id":"q1","op":"search","session_id":sid,"mode":"hybrid_prefilter","query":"Quem responde pelo dano?","top_k":1}));
    assert_eq!(first["ok"], true, "{first}");
    let second=worker.call(json!({"schema_version":1,"id":"q2","op":"search","session_id":sid,"mode":"hybrid","query":"Qual a responsabilidade pelo prejuízo?","top_k":1}));
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(
        first["result"]["search"]["model"]["load_ms"],
        second["result"]["search"]["model"]["load_ms"]
    );
    assert_eq!(first["result"]["search"]["model"]["completed_batches"], 2);
    assert_eq!(second["result"]["search"]["model"]["completed_batches"], 4);
    assert_eq!(
        first["result"]["entries"][0]["evidence"]["snapshot_id"],
        second["result"]["entries"][0]["evidence"]["snapshot_id"]
    );
    worker.finish();
}
