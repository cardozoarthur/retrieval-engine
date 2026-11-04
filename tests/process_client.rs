#![cfg(windows)]
use retrieval_engine::{
    process_client::{ClientConfig, ProcessClient},
    Operation, SearchMode, SourceSpec, Unit,
};
use std::{path::PathBuf, time::Duration};
fn client() -> ProcessClient {
    ProcessClient::spawn(ClientConfig {
        program: PathBuf::from(env!("CARGO_BIN_EXE_retrieval-worker")),
        local_roots: vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")],
        ..Default::default()
    })
    .unwrap()
}
#[test]
fn real_rust_consumer_searches_expands_exports_and_closes_same_snapshot() {
    let client = client();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two-pages.pdf");
    let session = client
        .open(
            SourceSpec::Local {
                path: source.to_string_lossy().into(),
            },
            Duration::from_secs(10),
        )
        .unwrap();
    let found = session
        .search("página", SearchMode::Bm25, 3, Duration::from_secs(5))
        .unwrap();
    assert!(!found.entries.is_empty());
    let reference = found.entries[0].reference.clone();
    let expanded = session
        .expand(&reference, 3, 8, Unit::Words, Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        expanded.evidence.snapshot_id,
        session.provenance().snapshot_id
    );
    let pack = session
        .pack(&[expanded.reference], Duration::from_secs(5))
        .unwrap();
    pack.verify_integrity().unwrap();
    session.close(Duration::from_secs(5)).unwrap();
    let a = client
        .submit(Operation::Capabilities, Duration::from_secs(5))
        .unwrap();
    let b = client
        .submit(Operation::Capabilities, Duration::from_secs(5))
        .unwrap();
    assert_ne!(a.id(), b.id());
    assert_eq!(b.wait().unwrap()["active_sessions"], 0);
    assert_eq!(a.wait().unwrap()["active_sessions"], 0);
    client.shutdown();
    assert!(client.is_closed());
}
#[test]
fn invalid_config_request_and_closed_transport_are_explicit() {
    let config = ClientConfig {
        max_in_flight: 0,
        ..Default::default()
    };
    assert!(ProcessClient::spawn(config).is_err());
    let client = client();
    assert!(client
        .submit(Operation::Capabilities, Duration::ZERO)
        .is_err());
    assert!(client
        .submit(
            Operation::Search {
                session_id: "x".into(),
                query: "".into(),
                mode: SearchMode::Literal,
                top_k: 1
            },
            Duration::from_secs(1)
        )
        .is_err());
    client.shutdown();
    assert!(client
        .submit(Operation::Capabilities, Duration::from_secs(1))
        .is_err());
}

#[test]
fn exhausted_work_budget_keeps_management_available() {
    let client = ProcessClient::spawn(ClientConfig {
        program: env!("CARGO_BIN_EXE_retrieval-worker").into(),
        max_work_requests: 1,
        ..Default::default()
    })
    .unwrap();
    let request = client
        .submit(Operation::Capabilities, Duration::from_secs(5))
        .unwrap();
    let id = request.id().to_owned();
    request.wait().unwrap();
    assert!(matches!(
        client.submit(Operation::Capabilities, Duration::from_secs(5)),
        Err(retrieval_engine::process_client::ClientError::ResourceLimit)
    ));
    let status = client
        .submit(
            Operation::RequestStatus {
                request_id: id.clone(),
            },
            Duration::from_secs(5),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(status["state"], "finished");
    let cancel = client
        .submit(Operation::Cancel { request_id: id }, Duration::from_secs(5))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(cancel["status"], "too_late");
    assert_eq!(cancel["work_finished"], true);
}

#[test]
fn oversized_response_and_worker_exit_fail_pending_calls() {
    let client = ProcessClient::spawn(ClientConfig {
        program: env!("CARGO_BIN_EXE_retrieval-worker").into(),
        local_roots: vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")],
        max_response_bytes: 1024,
        ..Default::default()
    })
    .unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/progressive.pdf");
    let result = client.open(
        SourceSpec::Local {
            path: source.to_string_lossy().into(),
        },
        Duration::from_secs(10),
    );
    assert!(matches!(
        result,
        Err(retrieval_engine::process_client::ClientError::Protocol(_))
    ));
    assert!(client.is_closed());
    client.shutdown();
    let exits = ProcessClient::spawn(ClientConfig {
        program: env!("CARGO_BIN_EXE_retrieval-worker").into(),
        prefix_args: vec!["--unsupported-client-fixture".into()],
        ..Default::default()
    })
    .unwrap();
    match exits.submit(Operation::Capabilities, Duration::from_secs(5)) {
        Ok(request) => assert!(request.wait().is_err()),
        Err(error) => assert!(matches!(
            error,
            retrieval_engine::process_client::ClientError::Closed
        )),
    }
    exits.shutdown();
}

#[test]
fn shutdown_observes_the_owned_worker_and_active_extraction_tree() {
    let client = client();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/progressive.pdf");
    let started = client
        .submit(
            Operation::StartOpen {
                source: SourceSpec::Local {
                    path: source.to_string_lossy().into(),
                },
            },
            Duration::from_secs(5),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(started["state"], "extracting");
    let report = client.shutdown();
    assert!(report.exit_observed);
    assert!(report.tree_exit_observed);
    assert!(report.tree_processes_before_stop.unwrap() >= 2);
    let repeated = client.shutdown();
    assert!(repeated.exit_observed && repeated.tree_exit_observed);
}

#[test]
fn local_deadline_is_not_reported_as_success_and_is_reconcilable() {
    let client = client();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/progressive.pdf");
    let request = client
        .submit(
            Operation::Open {
                source: SourceSpec::Local {
                    path: source.to_string_lossy().into(),
                },
            },
            Duration::from_millis(1),
        )
        .unwrap();
    let id = request.id().to_owned();
    assert!(request.wait().is_err());
    let _cancel = client
        .submit(
            Operation::Cancel {
                request_id: id.clone(),
            },
            Duration::from_secs(5),
        )
        .unwrap()
        .wait()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = client
            .submit(
                Operation::RequestStatus {
                    request_id: id.clone(),
                },
                Duration::from_secs(5),
            )
            .unwrap()
            .wait()
            .unwrap();
        if status["state"] == "finished" {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!client.is_closed());
    let caps = client
        .submit(Operation::Capabilities, Duration::from_secs(5))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(caps["active_sessions"], 0);
}

#[cfg(feature = "sqlite-store")]
#[test]
fn real_worker_pack_flows_into_rust_sqlite_and_survives_process_shutdown() {
    use retrieval_engine::evidence_store::{
        EvidenceStore, SqliteEvidenceStore, StoreLimits, VerificationMethod,
    };
    let client = client();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two-pages.pdf");
    let session = client
        .open(
            SourceSpec::Local {
                path: source.to_string_lossy().into(),
            },
            Duration::from_secs(10),
        )
        .unwrap();
    let found = session
        .search(
            "responsabilidade",
            SearchMode::Literal,
            1,
            Duration::from_secs(5),
        )
        .unwrap();
    let expanded = session
        .expand(
            &found.entries[0].reference,
            5,
            8,
            Unit::Words,
            Duration::from_secs(5),
        )
        .unwrap();
    let verified = session
        .verified_pack(&[expanded.reference], Duration::from_secs(5))
        .unwrap();
    assert_eq!(verified.method(), VerificationMethod::TrustedWorkerExport);
    let path = std::env::temp_dir().join(format!(
        "retrieval-client-store-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut store =
        SqliteEvidenceStore::create(&path, "host-project", StoreLimits::default()).unwrap();
    let receipt = store.put("selection-1", &verified).unwrap();
    session.close(Duration::from_secs(5)).unwrap();
    client.shutdown();
    drop(store);
    let mut reopened =
        SqliteEvidenceStore::open(&path, "host-project", StoreLimits::default()).unwrap();
    let (stored, pack) = reopened.get("selection-1").unwrap().unwrap();
    assert_eq!(stored.receipt_sha256, receipt.receipt_sha256);
    assert_eq!(pack.sha256, verified.pack().sha256);
    stored.verify(&pack).unwrap();
    drop(reopened);
    std::fs::remove_file(path).unwrap();
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "requires explicitly installed pinned E5 CPU assets; actual resident client integration"]
fn actual_rust_client_reuses_e5_across_queries() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let client = ProcessClient::spawn(ClientConfig {
        program: env!("CARGO_BIN_EXE_retrieval-worker").into(),
        local_roots: vec![root.join("tests/fixtures")],
        model_dir: Some(root.join("assets/e5-small-int8")),
        ..Default::default()
    })
    .unwrap();
    let session = client
        .open(
            SourceSpec::Local {
                path: root
                    .join("tests/fixtures/two-pages.pdf")
                    .to_string_lossy()
                    .into(),
            },
            Duration::from_secs(10),
        )
        .unwrap();
    let one = session
        .search(
            "Quem responde por prejuízos?",
            SearchMode::Hybrid,
            2,
            Duration::from_secs(30),
        )
        .unwrap();
    let two = session
        .search(
            "Quem responde por prejuízos?",
            SearchMode::Hybrid,
            2,
            Duration::from_secs(30),
        )
        .unwrap();
    assert!(!one.entries.is_empty());
    assert!(!two.entries.is_empty());
    assert_eq!(
        one.search["model"]["load_ms"],
        two.search["model"]["load_ms"]
    );
    assert!(
        two.search["model"]["cache"]["hits"].as_u64().unwrap()
            > one.search["model"]["cache"]["hits"].as_u64().unwrap()
    );
    assert_eq!(two.search["cost"]["native_inference_runs"], 0);
    session.close(Duration::from_secs(5)).unwrap();
    client.shutdown();
}
