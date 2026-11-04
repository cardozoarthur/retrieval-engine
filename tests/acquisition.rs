use retrieval_engine::{Document, Format, Limits, Source, Unit};

fn html(bytes: &[u8]) -> Document {
    Document::from_bytes(
        "fixture.html",
        bytes,
        "text/html; charset=utf-8",
        &Limits::default(),
    )
    .unwrap()
}

#[test]
fn html_preserves_meaning_and_expands_same_snapshot() {
    let doc = html("<html><head><title>Norma</title><style>proibido</style></head><body><p>Não é permitido <b>divulgar</b> dados &amp; fotos.</p><script>divulgar segredo</script><p hidden>oculto</p><p><s>Versão antiga</s> Versão atual.</p></body></html>".as_bytes());
    assert_eq!(doc.provenance().format, Format::Html);
    let hits = doc.find("divulgar", 10).unwrap();
    assert_eq!(hits.len(), 1);
    let expanded = doc.expand(&hits[0], 4, 3, Unit::Words, 200).unwrap();
    assert!(expanded
        .text
        .contains("Não é permitido divulgar dados & fotos."));
    assert!(!doc.text().contains("oculto"));
    assert!(!doc.text().contains("segredo"));
    assert!(doc.text().contains("[deleted]Versão antiga[/deleted]"));
    assert_eq!(doc.provenance().raw_sha256.len(), 64);
    assert_ne!(doc.provenance().raw_sha256, doc.provenance().text_sha256);
    assert_eq!(doc.provenance().snapshot_id, expanded.snapshot_id);
}

#[test]
fn evidence_cannot_move_between_versions() {
    let first = html(b"<p>Norma antiga</p>");
    let second = html(b"<p>Norma atual</p>");
    let hit = first.find("Norma", 10).unwrap().remove(0);
    assert!(second.expand(&hit, 1, 1, Unit::Words, 100).is_err());
    let mut changed = hit.clone();
    changed.text = "falso".into();
    assert!(first.expand(&changed, 0, 0, Unit::Words, 100).is_err());
    assert!(first.find("ausente", 10).unwrap().is_empty());
    assert!(first.find("", 10).is_err());
}

#[test]
fn encodings_and_rejections() {
    let latin = Document::from_bytes(
        "x",
        b"<p>A\xe7\xe3o</p>",
        "text/html; charset=iso-8859-1",
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(latin.text(), "Ação");
    let meta = Document::from_bytes(
        "x",
        b"<meta charset=windows-1252><p>A\xe7\xe3o</p>",
        "text/html",
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(meta.text(), "Ação");
    for (bytes, mime) in [
        (b"\xff".as_slice(), "text/plain; charset=utf-8"),
        (b"x".as_slice(), "image/png"),
        (b"x".as_slice(), "text/plain; charset=unknown"),
        (b"%PDF-fake".as_slice(), "application/pdf"),
    ] {
        assert!(
            Document::from_bytes("x", bytes, mime, &Limits::default()).is_err(),
            "{mime}"
        );
    }
    let limits = Limits {
        max_bytes: 3,
        ..Limits::default()
    };
    assert!(Document::from_bytes("x", b"1234", "text/plain", &limits).is_err());
    let limits = Limits {
        max_text_chars: 3,
        ..Limits::default()
    };
    assert!(Document::from_bytes("x", b"1234", "text/plain", &limits).is_err());
    assert!(Document::from_bytes("x", b"<p> </p>", "text/html", &Limits::default()).is_err());
}

#[test]
fn pdf_pages_and_cross_page_context() {
    let bytes = include_bytes!("fixtures/two-pages.pdf");
    let doc = Document::from_bytes(
        "two-pages.pdf",
        bytes,
        "application/pdf",
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(doc.pages().len(), 2);
    let hit = doc.find("responsabilidade", 10).unwrap().remove(0);
    assert_eq!(hit.pages, vec![2]);
    let expanded = doc
        .expand(&hit, 1000, 1000, Unit::Characters, 10000)
        .unwrap();
    assert_eq!(expanded.pages, vec![1, 2]);
    assert!(expanded.text.contains("primeira"));
    let limits = Limits {
        max_pages: 1,
        ..Limits::default()
    };
    assert!(Document::from_bytes("x", bytes, "application/pdf", &limits).is_err());
    let blank = include_bytes!("fixtures/blank.pdf");
    let error = Document::from_bytes("blank.pdf", blank, "application/pdf", &Limits::default())
        .err()
        .unwrap();
    assert!(error.to_string().contains("OCR"));
}

#[test]
fn unsafe_urls_fail_before_request() {
    for url in [
        "http://example.com",
        "https://user:pass@example.com",
        "https://127.0.0.1",
        "https://[::1]",
        "https://169.254.169.254",
        "https://192.168.1.1",
        "https://example.com:8443",
    ] {
        assert!(
            Document::load(Source::Remote(url.into()), &Limits::default()).is_err(),
            "{url}"
        );
    }
}

#[test]
fn mixed_pdf_keeps_page_numbers_and_reports_coverage() {
    let doc = Document::from_bytes(
        "mixed.pdf",
        include_bytes!("fixtures/mixed.pdf"),
        "application/pdf",
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(doc.pages().len(), 3);
    assert!(!doc.pages()[1].has_text);
    assert!(doc.warnings().iter().any(|w| w.contains("page 2")));
    let hit = doc.find("excecao", 10).unwrap().remove(0);
    assert_eq!(hit.pages, vec![3]);
    let all = doc
        .expand(&hit, 1000, 1000, Unit::Characters, 1000)
        .unwrap();
    assert_eq!(all.pages, vec![1, 2, 3]);
}

#[test]
fn local_sources_are_bounded_and_canonical() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let doc = Document::load(Source::Local(root.join("README.md")), &Limits::default()).unwrap();
    assert!(doc.provenance().final_source.ends_with("README.md"));
    assert_eq!(doc.provenance().format, Format::Text);
    assert!(!doc.find("Retrieval Engine", 1).unwrap().is_empty());
    assert!(Document::load(
        Source::Local(root.join("README.md")),
        &Limits {
            max_bytes: 1,
            ..Limits::default()
        }
    )
    .is_err());
    assert!(Document::load(Source::Local(root.join("missing.pdf")), &Limits::default()).is_err());
    assert!(Document::load(Source::Local(root.to_path_buf()), &Limits::default()).is_err());
}
#[test]
fn observed_pdf_emits_first_page_before_later_pages_and_can_stop() {
    let source = Source::Local(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two-pages.pdf"),
    );
    let mut seen = Vec::new();
    let result = Document::load_observed(source, &Limits::default(), &mut |text, page, total| {
        let page = page.unwrap();
        assert_eq!(total, 2);
        seen.push(page.number);
        assert!(text.contains("primeira pagina"));
        Err(retrieval_engine::Error(
            "stop after first provisional page".into(),
        ))
    });
    assert!(result.is_err());
    assert_eq!(seen, vec![1]);
}
