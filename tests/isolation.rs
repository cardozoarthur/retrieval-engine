#![cfg(windows)]
use retrieval_engine::{
    Document, ExtractionEvent, ExtractionTask, IsolationConfig, Limits, RequestControl, Source,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn source(name: &str) -> Source {
    Source::Local(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
}
fn program() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_retrieval-worker"))
}

#[test]
fn isolated_pdf_matches_snapshot_and_confirms_helper_exit() {
    let original = Document::load(source("two-pages.pdf"), &Limits::default()).unwrap();
    let control = RequestControl::new(Duration::from_secs(10)).unwrap();
    let mut task = ExtractionTask::start(
        program(),
        source("two-pages.pdf"),
        Limits::default(),
        IsolationConfig::default(),
        control.clone(),
    )
    .unwrap();
    let mut seen = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < end);
        if let Some(event) = task.next_event(Duration::from_millis(20)).unwrap() {
            match event {
                ExtractionEvent::Chunk {
                    page: Some(page),
                    final_chunk: true,
                    ..
                } => seen.push(page.number),
                ExtractionEvent::Complete { .. } => break,
                _ => {}
            }
        }
    }
    assert_eq!(seen, vec![1, 2]);
    let (isolated, report) = task.finish().unwrap();
    assert_eq!(isolated.text(), original.text());
    assert_eq!(isolated.pages(), original.pages());
    assert_eq!(
        isolated.provenance().snapshot_id,
        original.provenance().snapshot_id
    );
    assert_eq!(
        isolated.provenance().raw_sha256,
        original.provenance().raw_sha256
    );
    assert!(report.helper_exit_confirmed && report.kill_on_close);
    assert_eq!(report.process_commit_limit_bytes, 256 * 1024 * 1024);
    assert!(report.peak_process_commit_bytes <= report.process_commit_limit_bytes);
    assert!(report.output_bytes > 0);
    control.commit(|| Ok(())).unwrap();
}

#[test]
fn progressive_page_is_available_while_helper_runs_and_cancel_invalidates_text() {
    let control = RequestControl::new(Duration::from_secs(10)).unwrap();
    let mut task = ExtractionTask::start(
        program(),
        source("progressive.pdf"),
        Limits::default(),
        IsolationConfig::default(),
        control.clone(),
    )
    .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < end);
        if matches!(
            task.next_event(Duration::from_millis(20)).unwrap(),
            Some(ExtractionEvent::Chunk {
                final_chunk: true,
                ..
            })
        ) {
            break;
        }
    }
    assert_eq!(task.completed_pages().len(), 1);
    assert!(task.provisional_text().contains("retrieval page 1"));
    assert!(
        task.helper_is_running().unwrap(),
        "provisional page must precede helper completion"
    );
    control.cancel();
    assert!(task.next_event(Duration::ZERO).is_err());
    assert!(task.provisional_text().is_empty());
    assert!(task.completed_pages().is_empty());
    assert!(task.finish().is_err());
    assert!(control
        .commit::<()>(|| panic!("cancelled prefix cannot become final evidence"))
        .is_err());
}

#[test]
fn isolated_output_budget_and_ocr_fail_without_final_document() {
    let control = RequestControl::new(Duration::from_secs(10)).unwrap();
    let task = ExtractionTask::start(
        program(),
        source("progressive.pdf"),
        Limits::default(),
        IsolationConfig {
            max_output_bytes: 4 * 1024,
            max_event_bytes: 4 * 1024,
            ..Default::default()
        },
        control.clone(),
    )
    .unwrap();
    // Page frames + terminal ranges exceed this explicit output budget.
    assert!(task.finish().is_err());
    control.finish();
    let control = RequestControl::new(Duration::from_secs(10)).unwrap();
    let task = ExtractionTask::start(
        program(),
        source("blank.pdf"),
        Limits::default(),
        IsolationConfig::default(),
        control.clone(),
    )
    .unwrap();
    assert!(task
        .finish()
        .err()
        .unwrap()
        .to_string()
        .contains("OCR required"));
    control.finish();
}

#[test]
fn helper_without_job_rejects_handshake_before_source_acquisition() {
    use std::os::windows::process::CommandExt;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new(program())
        .arg("--extract-helper")
        .creation_flags(0x08000000)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let input = serde_json::json!({"schema_version":1,"source":source("two-pages.pdf"),"limits":Limits::default(),"memory_bytes":256*1024*1024});
    writeln!(child.stdin.take().unwrap(), "{input}").unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(!result.status.success());
    assert!(
        result.stdout.is_empty(),
        "unisolated helper cannot emit source or page events"
    );
}

#[test]
fn changing_original_after_acquisition_cannot_rebind_provisional_pages() {
    struct Fixture {
        root: PathBuf,
        file: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.file);
            let _ = std::fs::remove_dir(&self.root);
        }
    }
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp");
    std::fs::create_dir_all(&base).unwrap();
    let root = base.join(format!("isolation-revision-{}", std::process::id()));
    assert!(root.starts_with(base));
    std::fs::create_dir(&root).unwrap();
    let fixture = Fixture {
        file: root.join("source.pdf"),
        root,
    };
    let Source::Local(original) = source("progressive.pdf") else {
        unreachable!()
    };
    std::fs::copy(original, &fixture.file).unwrap();
    let source = Source::Local(fixture.file.clone());
    let before = Document::load(source.clone(), &Limits::default()).unwrap();
    let control = RequestControl::new(Duration::from_secs(10)).unwrap();
    let mut task = ExtractionTask::start(
        program(),
        source,
        Limits::default(),
        IsolationConfig::default(),
        control.clone(),
    )
    .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while task.source().is_none() {
        assert!(Instant::now() < end);
        task.next_event(Duration::from_millis(20)).unwrap();
    }
    let acquired = task.source().unwrap().raw_sha256.clone();
    std::fs::write(&fixture.file, b"Replaced original after acquisition.").unwrap();
    let (final_doc, report) = task.finish().unwrap();
    assert!(report.helper_exit_confirmed);
    assert_eq!(final_doc.provenance().raw_sha256, acquired);
    assert_eq!(
        final_doc.provenance().snapshot_id,
        before.provenance().snapshot_id
    );
    assert_eq!(final_doc.text(), before.text());
    assert_eq!(final_doc.pages().len(), 128);
    assert!(final_doc.text().contains("retrieval page 128"));
    control.commit(|| Ok(())).unwrap();
}

#[test]
fn isolated_html_preserves_charset_and_unicode_offsets_across_chunks() {
    use std::io::Write;
    struct OwnedFile(PathBuf);
    impl Drop for OwnedFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp");
    std::fs::create_dir_all(&base).unwrap();
    let path = base.join(format!("isolation-html-{}.html", std::process::id()));
    assert!(path.starts_with(base));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let owned = OwnedFile(path);
    let html=format!("<html><head><meta charset='windows-1252'></head><body><p hidden>forbidden</p>{}</body></html>","<p>Não há culpa presumida. Exceção exige prova.</p>".repeat(700));
    let (encoded, _, invalid) = encoding_rs::WINDOWS_1252.encode(&html);
    assert!(!invalid);
    file.write_all(&encoded).unwrap();
    drop(file);
    let source = Source::Local(owned.0.clone());
    let original = Document::load(source.clone(), &Limits::default()).unwrap();
    let control = RequestControl::new(Duration::from_secs(10)).unwrap();
    let mut task = ExtractionTask::start(
        program(),
        source,
        Limits::default(),
        IsolationConfig::default(),
        control.clone(),
    )
    .unwrap();
    let mut chars = 0;
    let mut chunks = 0;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < until);
        if let Some(event) = task.next_event(Duration::from_millis(20)).unwrap() {
            match event {
                ExtractionEvent::Chunk {
                    page,
                    total_pages,
                    start,
                    text,
                    ..
                } => {
                    assert!(page.is_none());
                    assert_eq!(total_pages, 0);
                    assert_eq!(start, chars);
                    chars += text.chars().count();
                    chunks += 1;
                }
                ExtractionEvent::Complete { .. } => break,
                _ => {}
            }
        }
    }
    let (document, report) = task.finish().unwrap();
    assert!(report.helper_exit_confirmed && chunks > 1);
    assert_eq!(chars, document.text().chars().count());
    assert_eq!(document.text(), original.text());
    assert_eq!(
        document.provenance().snapshot_id,
        original.provenance().snapshot_id
    );
    assert_eq!(
        document.provenance().charset.as_deref(),
        Some("windows-1252")
    );
    assert!(document
        .text()
        .contains("Não há culpa presumida. Exceção exige prova."));
    assert!(!document.text().contains("forbidden"));
    control.commit(|| Ok(())).unwrap();
}
