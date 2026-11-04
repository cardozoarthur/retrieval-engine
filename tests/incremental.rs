#![cfg(windows)]
use retrieval_engine::{EvidencePack, OpenTaskConfig, RetrievalWorker, WorkerConfig};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
fn worker(config: Option<OpenTaskConfig>) -> RetrievalWorker {
    RetrievalWorker::new(WorkerConfig {
        local_roots: vec![root()],
        extraction_helper: Some(PathBuf::from(env!("CARGO_BIN_EXE_retrieval-worker"))),
        open_tasks: config.unwrap_or_default(),
        ..Default::default()
    })
    .unwrap()
}
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}
fn rpc(worker: &mut RetrievalWorker, request: Value) -> Value {
    serde_json::to_value(worker.handle_json(&serde_json::to_vec(&request).unwrap())).unwrap()
}
fn start(worker: &mut RetrievalWorker, file: &str) -> String {
    let response = rpc(
        worker,
        json!({"schema_version":1,"id":"s","op":"start_open","source":{"kind":"local","path":root().join(file)}}),
    );
    assert_eq!(response["ok"], true, "{response}");
    response["result"]["task_id"].as_str().unwrap().into()
}
fn poll(worker: &mut RetrievalWorker, id: &str, events: usize) -> Value {
    let response = rpc(
        worker,
        json!({"schema_version":1,"id":"p","op":"poll_open","task_id":id,"max_events":events}),
    );
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}
fn ready(worker: &mut RetrievalWorker, id: &str) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < end);
        let value = poll(worker, id, 64);
        if value["state"] == "ready" {
            break;
        }
        assert_eq!(value["state"], "extracting", "{value}");
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn partial_find_expand_bind_and_pack_keep_exact_version_and_anchor() {
    let mut worker = worker(None);
    let id = start(&mut worker, "progressive.pdf");
    let end = Instant::now() + Duration::from_secs(10);
    let partial = loop {
        assert!(Instant::now() < end);
        let p = poll(&mut worker, &id, 2);
        if p["coverage"]["completed_pages"].as_u64().unwrap() > 0 {
            break p;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(partial["state"], "extracting");
    assert_eq!(partial["coverage"]["end_of_source_known"], false);
    let missing = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"m","op":"peek_open","task_id":id,"query":"retrieval page 128.","top_k":1}),
    );
    assert!(missing["result"]["entries"].as_array().unwrap().is_empty());
    assert_eq!(
        missing["result"]["coverage"]["full_snapshot_verified"],
        false
    );
    let found = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"q","op":"peek_open","task_id":id,"query":"Synthetic retrieval page 1.","top_k":1}),
    );
    assert_eq!(found["ok"], true, "{found}");
    let first = found["result"]["entries"][0].clone();
    let reference = first["reference"].as_str().unwrap();
    assert!(reference.starts_with("p_"));
    let expanded = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"e","op":"expand_open","task_id":id,"reference":reference,"before":0,"after":4,"unit":"words"}),
    );
    assert_eq!(expanded["ok"], true, "{expanded}");
    let preview = expanded["result"]["entry"].clone();
    assert_eq!(preview["provisional"], true);
    assert_eq!(
        preview["record"]["anchor_start"],
        first["record"]["anchor_start"]
    );
    assert!(preview["text"]
        .as_str()
        .unwrap()
        .contains("Exception requires proof."));
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"early","op":"finish_open","task_id":id})
        )["error"]["code"],
        "task_not_ready"
    );
    ready(&mut worker, &id);
    let finished = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"f","op":"finish_open","task_id":id,"references":[preview["reference"]]}),
    );
    assert_eq!(finished["ok"], true, "{finished}");
    let final_view = &finished["result"]["bindings"][0]["final"];
    assert_eq!(final_view["evidence"]["text"], preview["text"]);
    assert_eq!(
        final_view["record"]["anchor_start"],
        first["record"]["anchor_start"]
    );
    assert_eq!(
        final_view["record"]["anchor_end"],
        first["record"]["anchor_end"]
    );
    assert_eq!(
        finished["result"]["provenance"]["raw_sha256"],
        first["record"]["raw_sha256"]
    );
    assert_eq!(
        finished["result"]["preview_lineage"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let sid = finished["result"]["session_id"].clone();
    let pack = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"pack","op":"pack","session_id":sid,"references":[final_view["reference"]]}),
    );
    assert_eq!(pack["ok"], true, "{pack}");
    let pack: EvidencePack = serde_json::from_value(pack["result"].clone()).unwrap();
    pack.verify_integrity().unwrap();
    assert_eq!(pack.payload.ancestors.len(), 1);
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"bad","op":"expand","session_id":sid,"reference":preview["reference"],"before":1,"after":1,"unit":"characters"})
        )["error"]["code"],
        "unknown_reference"
    );
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"twice","op":"finish_open","task_id":id})
        )["error"]["code"],
        "task_finalized"
    );
    let caps = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"caps","op":"capabilities"}),
    );
    assert_eq!(caps["result"]["live_open_tasks"], 0);
    assert_eq!(caps["result"]["reserved_open_chars"], 0);
    assert_eq!(caps["result"]["active_sessions"], 1);
}
#[test]
fn cancellation_quota_and_ready_ttl_invalidate_all_provisional_data() {
    let mut worker = worker(Some(OpenTaskConfig {
        max_live: 1,
        idle_ttl: Duration::from_millis(200),
        ..Default::default()
    }));
    let id = start(&mut worker, "progressive.pdf");
    let rejected = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"over","op":"start_open","source":{"kind":"local","path":root().join("two-pages.pdf")}}),
    );
    assert_eq!(rejected["error"]["code"], "resource_limit");
    let controller = worker.open_task_controller();
    assert_eq!(controller.cancel(&id).unwrap()["status"], "requested");
    worker.maintenance();
    assert_eq!(controller.status(&id).unwrap()["state"], "cancelled");
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"old","op":"peek_open","task_id":id,"query":"Synthetic"})
        )["error"]["code"],
        "task_cancelled"
    );
    let next = start(&mut worker, "two-pages.pdf");
    ready(&mut worker, &next);
    std::thread::sleep(Duration::from_millis(250));
    worker.maintenance();
    assert_eq!(controller.status(&next).unwrap()["state"], "expired");
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"expired","op":"finish_open","task_id":next})
        )["error"]["code"],
        "task_expired"
    );
    let caps = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"caps","op":"capabilities"}),
    );
    assert_eq!(caps["result"]["reserved_open_chars"], 0);
    assert_eq!(caps["result"]["active_sessions"], 0);
}

#[test]
fn invalid_binding_and_resource_reservations_never_admit_partial_sessions() {
    let mut worker = RetrievalWorker::new(WorkerConfig {
        local_roots: vec![root()],
        extraction_helper: Some(PathBuf::from(env!("CARGO_BIN_EXE_retrieval-worker"))),
        max_total_source_chars: 2000,
        ..Default::default()
    })
    .unwrap();
    let id = start(&mut worker, "two-pages.pdf");
    let caps = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"c","op":"capabilities"}),
    );
    assert_eq!(caps["result"]["reserved_open_chars"], 2000);
    let blocked = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"b","op":"open","source":{"kind":"local","path":root().join("two-pages.pdf")}}),
    );
    assert_eq!(blocked["error"]["code"], "resource_limit");
    ready(&mut worker, &id);
    let found = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"q","op":"peek_open","task_id":id,"query":"prova","top_k":1}),
    );
    let reference = found["result"]["entries"][0]["reference"].clone();
    let failed = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"bad","op":"finish_open","task_id":id,"references":[reference,"p_missing"]}),
    );
    assert_eq!(failed["error"]["code"], "unknown_reference");
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"c","op":"capabilities"})
        )["result"]["active_sessions"],
        0
    );
    assert_eq!(
        worker.open_task_controller().cancel(&id).unwrap()["status"],
        "requested"
    );
    let failed = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"f","op":"finish_open","task_id":id,"references":[reference]}),
    );
    assert_eq!(failed["error"]["code"], "task_cancelled");
    let caps = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"c","op":"capabilities"}),
    );
    assert_eq!(caps["result"]["active_sessions"], 0);
    assert_eq!(caps["result"]["reserved_open_chars"], 0);
}

#[test]
fn late_pdf_failure_invalidates_previously_found_provisional_reference() {
    let mut worker = worker(None);
    let id = start(&mut worker, "late-error.pdf");
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < end);
        let p = poll(&mut worker, &id, 2);
        assert_eq!(
            p["state"], "extracting",
            "failure fixture must deliver its first valid page"
        );
        if p["coverage"]["completed_pages"].as_u64().unwrap() > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let found = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"q","op":"peek_open","task_id":id,"query":"retrieval page 1.","top_k":1}),
    );
    let reference = found["result"]["entries"][0]["reference"].clone();
    assert!(reference.is_string(), "{found}");
    loop {
        assert!(Instant::now() < end);
        let p = poll(&mut worker, &id, 64);
        if p["state"] == "failed" {
            break;
        }
        assert_eq!(
            p["state"], "extracting",
            "late corruption must not finalize a prefix: {p}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let expanded = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"e","op":"expand_open","task_id":id,"reference":reference,"before":1,"after":1,"unit":"words"}),
    );
    assert_eq!(expanded["error"]["code"], "task_failed");
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"f","op":"finish_open","task_id":id})
        )["error"]["code"],
        "task_failed"
    );
    let caps = rpc(
        &mut worker,
        json!({"schema_version":1,"id":"c","op":"capabilities"}),
    );
    assert_eq!(caps["result"]["reserved_open_chars"], 0);
    assert_eq!(caps["result"]["active_sessions"], 0);
}

#[test]
fn verified_snapshot_still_reports_pages_without_extractable_text() {
    let mut worker = worker(None);
    let id = start(&mut worker, "mixed.pdf");
    ready(&mut worker, &id);
    let state = poll(&mut worker, &id, 1);
    assert_eq!(state["coverage"]["full_snapshot_verified"], true);
    assert_eq!(state["coverage"]["pages_without_text"], 1);
    assert_eq!(state["coverage"]["completed_pages"], 3);
}

#[test]
fn task_deadline_record_eviction_and_worker_drop_release_source_leases() {
    let mut worker = worker(Some(OpenTaskConfig {
        max_live: 1,
        max_entries: 2,
        extraction_timeout: Duration::from_millis(300),
        ..Default::default()
    }));
    let first = start(&mut worker, "progressive.pdf");
    let controller = worker.open_task_controller();
    std::thread::sleep(Duration::from_millis(350));
    worker.maintenance();
    assert_eq!(controller.status(&first).unwrap()["state"], "failed");
    assert_eq!(
        rpc(
            &mut worker,
            json!({"schema_version":1,"id":"f","op":"finish_open","task_id":first})
        )["error"]["code"],
        "task_failed"
    );
    let second = start(&mut worker, "progressive.pdf");
    controller.cancel(&second).unwrap();
    worker.maintenance();
    let third = start(&mut worker, "progressive.pdf");
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert!(
        controller.status(&first).is_err(),
        "terminal record must be evicted at bounded capacity"
    );
    drop(worker);
    assert!(
        controller.status(&third).is_err(),
        "dropped worker cannot retain a live source lease"
    );
}
