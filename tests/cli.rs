use std::process::Command;

#[test]
fn pdf_cli_emits_machine_readable_provenance_and_context() {
    let output = Command::new(env!("CARGO_BIN_EXE_retrieval-engine"))
        .args([
            "tests/fixtures/two-pages.pdf",
            "responsabilidade",
            "5",
            "10",
            "words",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["provenance"]["format"], "pdf");
    assert_eq!(value["results"][0]["hit"]["pages"][0], 2);
    assert!(value["results"][0]["context"]["text"]
        .as_str()
        .unwrap()
        .contains("responsabilidade"));
}

#[test]
fn cli_errors_never_emit_success_json() {
    for args in [
        vec!["nonexistent.txt", "query"],
        vec!["README.md", "query", "x"],
        vec!["README.md", "query", "1", "1", "bytes"],
        vec!["README.md", ""],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_retrieval-engine"))
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[cfg(not(feature = "cpu-onnx"))]
#[test]
fn hybrid_without_feature_fails_before_acquisition() {
    let output = Command::new(env!("CARGO_BIN_EXE_retrieval-engine"))
        .args(["missing.pdf", "query", "1", "1", "words", "hybrid"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cpu-onnx"));
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "actual CPU hybrid CLI with pinned assets"]
fn actual_hybrid_cli_preserves_pages_and_declares_coverage() {
    for mode in ["hybrid", "hybrid-prefilter"] {
        let output = Command::new(env!("CARGO_BIN_EXE_retrieval-engine"))
            .args([
                "tests/fixtures/two-pages.pdf",
                "responsabilidade culpa",
                "2",
                "2",
                "words",
                mode,
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["search"]["coverage"]["complete_index"], true);
        assert_eq!(value["search"]["model"]["provider"], "CPUExecutionProvider");
        assert!(value["results"][0]["hit"]["pages"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(2)));
        assert!(value["results"][0]["context"]["text"]
            .as_str()
            .unwrap()
            .contains("nao se aplica"));
        assert_eq!(value["search"]["mode"], mode);
    }
}

#[test]
fn bm25_cli_accepts_portuguese_token_queries() {
    let output = Command::new(env!("CARGO_BIN_EXE_retrieval-engine"))
        .args([
            "tests/fixtures/two-pages.pdf",
            "RESPONSABILIDADE culpa",
            "0",
            "0",
            "words",
            "bm25",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["search"]["mode"], "bm25");
    assert_eq!(value["search"]["semantic_inference"], false);
    assert!(value["results"][0]["score"].as_f64().unwrap() > 0.0);
    assert!(value["results"][0]["hit"]["text"]
        .as_str()
        .unwrap()
        .contains("nao se aplica"));
}

#[cfg(feature = "cpu-onnx")]
#[test]
#[ignore = "requires pinned assets; actual persistent native JSONL worker"]
fn actual_worker_handles_multiple_requests_without_reloading() {
    use std::{io::Write, process::Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_retrieval-embeddings"))
        .arg("assets/e5-small-int8")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        for line in [
            r#"{"schema_version":1,"id":"q1","kind":"query","texts":["Quem responde pelo dano?"]}"#,
            "malformed",
            r#"{"schema_version":1,"id":"p1","kind":"passage","texts":["A responsabilidade exige prova."]}"#,
            r#"{"schema_version":1,"id":"q2","kind":"query","texts":["Como reparar o prejuízo?"]}"#,
        ] {
            writeln!(input, "{line}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["id"], "q1");
    assert_eq!(lines[1]["ok"], false);
    assert_eq!(lines[2]["id"], "p1");
    assert_eq!(lines[3]["id"], "q2");
    assert_eq!(lines[0]["model"]["completed_batches"], 1);
    assert_eq!(lines[3]["model"]["completed_batches"], 3);
    assert_eq!(lines[0]["model"]["load_ms"], lines[3]["model"]["load_ms"]);
    assert_eq!(
        lines[3]["result"]["vectors"][0].as_array().unwrap().len(),
        384
    );
}
