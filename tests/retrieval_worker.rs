use retrieval_engine::{parse_retrieval_request, RetrievalWorker, WorkerConfig};
use serde_json::{json, Value};
use std::path::PathBuf;

fn call(worker: &mut RetrievalWorker, request: Value) -> Value {
    serde_json::to_value(worker.handle_json(&serde_json::to_vec(&request).unwrap())).unwrap()
}

#[test]
fn pre_cancelled_requests_do_not_mutate_sessions_or_references() {
    use retrieval_engine::{CancelResult, RequestControl};
    use std::time::Duration;
    let fixture = Fixture::new("cancelled");
    let mut worker = RetrievalWorker::new(WorkerConfig {
        local_roots: vec![fixture.root.clone()],
        ..Default::default()
    })
    .unwrap();
    let open = json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":fixture.root.join("source.txt")}});
    let cancelled = RequestControl::new(Duration::from_secs(5)).unwrap();
    assert_eq!(cancelled.cancel(), CancelResult::Requested);
    assert_eq!(
        worker
            .handle_json_with_control(&serde_json::to_vec(&open).unwrap(), &cancelled)
            .error
            .unwrap()
            .code,
        "cancelled"
    );
    assert_eq!(
        call(
            &mut worker,
            json!({"schema_version":1,"id":"c","op":"capabilities"})
        )["result"]["active_sessions"],
        0
    );
    let sid = call(&mut worker, open)["result"]["session_id"].clone();
    for request in [
        json!({"schema_version":1,"id":"s","op":"search","session_id":sid,"query":"norma","mode":"bm25"}),
        json!({"schema_version":1,"id":"c","op":"close","session_id":sid}),
    ] {
        let cancelled = RequestControl::new(Duration::from_secs(5)).unwrap();
        cancelled.cancel();
        let reply =
            worker.handle_json_with_control(&serde_json::to_vec(&request).unwrap(), &cancelled);
        assert_eq!(reply.error.unwrap().code, "cancelled");
    }
    let control = RequestControl::new(Duration::from_secs(5)).unwrap();
    let searched=worker.handle_json_with_control(&serde_json::to_vec(&json!({"schema_version":1,"id":"s","op":"search","session_id":sid,"query":"norma","mode":"bm25"})).unwrap(),&control);
    assert!(searched.ok);
    assert_eq!(
        searched.result.unwrap()["entries"][0]["record"]["ordinal"],
        1
    );
    assert_eq!(control.cancel(), CancelResult::TooLate);
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "actual controlled worker E5 cancellation preserves another session and admits no partial refs"]
fn actual_worker_cancelled_hybrid_does_not_admit_references() {
    use retrieval_engine::{CancelResult, RequestControl};
    use std::time::{Duration, Instant};
    let fixture = Fixture::new("native-control");
    std::fs::write(
        fixture.root.join("source.txt"),
        "A norma não permite divulgar dados. Exceção exige prova. ".repeat(160),
    )
    .unwrap();
    let mut worker = RetrievalWorker::new(WorkerConfig {
        local_roots: vec![fixture.root.clone()],
        model_dir: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/e5-small-int8")),
        ..Default::default()
    })
    .unwrap();
    let open = || json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":fixture.root.join("source.txt")}});
    let first = call(&mut worker, open())["result"]["session_id"].clone();
    let second = call(&mut worker, open())["result"]["session_id"].clone();
    let original = call(
        &mut worker,
        json!({"schema_version":1,"id":"other","op":"search","session_id":second,"mode":"literal","query":"Exceção","top_k":1}),
    );
    let reference = original["result"]["entries"][0]["reference"].clone();
    let control = RequestControl::new(Duration::from_secs(15)).unwrap();
    let running = control.clone();
    let request=serde_json::to_vec(&json!({"schema_version":1,"id":"cancelled-search","op":"search","session_id":first,"mode":"hybrid","query":"norma dados prova","top_k":1})).unwrap();
    let job = std::thread::spawn(move || {
        let result = worker.handle_json_with_control(&request, &running);
        (worker, result)
    });
    let end = Instant::now() + Duration::from_secs(10);
    while !control.interrupt_armed() && !job.is_finished() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(control.interrupt_armed());
    assert_eq!(control.cancel(), CancelResult::Requested);
    let (mut worker, result) = job.join().unwrap();
    assert!(!result.ok);
    assert_eq!(result.error.unwrap().code, "cancelled");
    let after = call(
        &mut worker,
        json!({"schema_version":1,"id":"after","op":"search","session_id":first,"mode":"literal","query":"norma","top_k":1}),
    );
    assert_eq!(
        after["result"]["entries"][0]["record"]["ordinal"], 1,
        "cancelled search must not consume ordinals"
    );
    let other = call(
        &mut worker,
        json!({"schema_version":1,"id":"e","op":"expand","session_id":second,"reference":reference,"before":1,"after":1,"unit":"words"}),
    );
    assert_eq!(other["ok"], true, "{other}");
    assert_eq!(other["result"]["record"]["ordinal"], 2);
    let reused = call(
        &mut worker,
        json!({"schema_version":1,"id":"retry","op":"search","session_id":first,"mode":"hybrid_prefilter","query":"dados prova","top_k":1}),
    );
    assert_eq!(reused["ok"], true, "{reused}");
    assert!(
        reused["result"]["search"]["model"]["inference_attempts"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn protocol_shape_cases_match_actual_parser_without_network() {
    let cases: Value =
        serde_json::from_slice(include_bytes!("fixtures/retrieval-protocol-cases.json")).unwrap();
    for request in cases["valid_shape"].as_array().unwrap() {
        assert!(
            parse_retrieval_request(&serde_json::to_vec(request).unwrap()).is_ok(),
            "{request}"
        );
    }
    for request in cases["invalid_shape"].as_array().unwrap() {
        assert!(
            parse_retrieval_request(&serde_json::to_vec(request).unwrap()).is_err(),
            "{request}"
        );
    }
}

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join(format!("worker-{name}-{}", std::process::id()));
        assert!(root.starts_with(&base));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("source.txt"),
            "A norma não permite divulgar dados. Exceção exige prova.",
        )
        .unwrap();
        Self { root }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.root.join("source.txt"));
        let _ = std::fs::remove_dir(&self.root);
    }
}

#[test]
fn worker_keeps_snapshot_across_calls_exports_pack_and_closes() {
    let fixture = Fixture::new("snapshot");
    let mut worker = RetrievalWorker::new(WorkerConfig {
        local_roots: vec![fixture.root.clone()],
        ..WorkerConfig::default()
    })
    .unwrap();
    let opened = call(
        &mut worker,
        json!({"schema_version":1,"id":"open","op":"open","source":{"kind":"local","path":fixture.root.join("source.txt")}}),
    );
    assert_eq!(opened["ok"], true, "{opened}");
    let sid = opened["result"]["session_id"].as_str().unwrap().to_string();
    std::fs::write(
        fixture.root.join("source.txt"),
        "Fonte substituída depois da aquisição.",
    )
    .unwrap();
    let searched = call(
        &mut worker,
        json!({"schema_version":1,"id":"q","op":"search","session_id":sid,"mode":"literal","query":"divulgar"}),
    );
    assert_eq!(searched["ok"], true, "{searched}");
    let reference = searched["result"]["entries"][0]["reference"]
        .as_str()
        .unwrap();
    let expanded = call(
        &mut worker,
        json!({"schema_version":1,"id":"e","op":"expand","session_id":sid,"reference":reference,"before":3,"after":2,"unit":"words"}),
    );
    assert_eq!(expanded["ok"], true, "{expanded}");
    assert!(expanded["result"]["evidence"]["text"]
        .as_str()
        .unwrap()
        .contains("não permite divulgar dados."));
    let expanded_ref = expanded["result"]["reference"].as_str().unwrap();
    let pack = call(
        &mut worker,
        json!({"schema_version":1,"id":"p","op":"pack","session_id":sid,"references":[expanded_ref]}),
    );
    assert_eq!(pack["ok"], true, "{pack}");
    let artifact: retrieval_engine::EvidencePack =
        serde_json::from_value(pack["result"].clone()).unwrap();
    artifact.verify_integrity().unwrap();
    assert_eq!(artifact.payload.ancestors.len(), 1);
    assert_eq!(
        call(
            &mut worker,
            json!({"schema_version":1,"id":"c","op":"close","session_id":sid})
        )["ok"],
        true
    );
    let stale = call(
        &mut worker,
        json!({"schema_version":1,"id":"stale","op":"expand","session_id":sid,"reference":expanded_ref,"before":1,"after":1,"unit":"characters"}),
    );
    assert_eq!(stale["error"]["code"], "unknown_session");
}

#[test]
fn worker_enforces_host_roots_session_budget_and_invalid_requests() {
    let fixture = Fixture::new("limits");
    let mut denied = RetrievalWorker::new(WorkerConfig::default()).unwrap();
    let request = json!({"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":fixture.root.join("source.txt")}});
    assert_eq!(
        call(&mut denied, request.clone())["error"]["code"],
        "forbidden_source"
    );
    let mut worker = RetrievalWorker::new(WorkerConfig {
        local_roots: vec![fixture.root.clone()],
        max_sessions: 1,
        ..WorkerConfig::default()
    })
    .unwrap();
    let outside = Fixture::new("outside");
    assert_eq!(
        call(
            &mut worker,
            json!({"schema_version":1,"id":"outside","op":"open","source":{"kind":"local","path":outside.root.join("source.txt")}})
        )["error"]["code"],
        "forbidden_source"
    );
    let opened = call(&mut worker, request.clone());
    assert_eq!(opened["ok"], true);
    assert_eq!(
        call(&mut worker, request)["error"]["code"],
        "resource_limit"
    );
    let rejected = call(
        &mut worker,
        json!({"schema_version":1,"id":"bad","op":"capabilities","model_dir":"injected"}),
    );
    assert_eq!(rejected["id"], "bad");
    assert_eq!(rejected["error"]["code"], "invalid_request");
    assert!(!rejected.to_string().contains("injected"));
    let huge = worker.handle_json(&vec![b'x'; 128 * 1024 + 1]);
    assert!(!huge.ok);
}
