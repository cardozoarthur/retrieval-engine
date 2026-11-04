#![cfg(feature = "cpu-onnx")]
use retrieval_engine::{
    hybrid_search, ChunkConfig, CpuConfig, Document, E5Embedder, HybridBudget, HybridOptions,
    IndexLimits, LexicalIndex, Limits, SemanticCoverage, Unit,
};
use std::{sync::Arc, time::Duration};

#[test]
fn invalid_hybrid_options_fail_without_model() {
    for options in [
        HybridOptions {
            top_k: 0,
            ..HybridOptions::default()
        },
        HybridOptions {
            rrf_k: 0,
            ..HybridOptions::default()
        },
        HybridOptions {
            min_cosine: Some(f32::NAN),
            ..HybridOptions::default()
        },
        HybridOptions {
            budget: HybridBudget {
                max_semantic_chunks: 0,
                ..HybridBudget::default()
            },
            ..HybridOptions::default()
        },
    ] {
        assert!(options.validate().is_err());
    }
}

fn doc(name: &str, text: &str) -> Arc<Document> {
    Arc::new(Document::from_bytes(name, text.as_bytes(), "text/plain", &Limits::default()).unwrap())
}

#[test]
#[ignore = "actual E5 CPU hybrid retrieval, coverage, budgets and no silent truncation; pinned assets"]
fn actual_full_hybrid_finds_paraphrase_without_lexical_overlap() {
    let mut model = E5Embedder::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/e5-small-int8"),
        CpuConfig {
            allow_truncation: true,
            ..CpuConfig::default()
        },
    )
    .unwrap();
    let relevant = doc("relevant", "O agente indeniza vítimas quando há tratamento ilícito com dados pessoais. Não se presume culpa; exceção exige prova, artigo 42.");
    let index = LexicalIndex::build(
        vec![
            relevant.clone(),
            doc(
                "transport",
                "Cargas seguem pela rodovia com frete previamente pago.",
            ),
            doc("food", "A receita inclui farinha, ovos, leite."),
        ],
        ChunkConfig::default(),
        IndexLimits::default(),
    )
    .unwrap();
    let query = "Quem paga pelos prejuízos causados por vazamento de informações?";
    assert!(index.search(query, 10).unwrap().is_empty());
    for budget in [
        HybridBudget {
            max_semantic_chunks: 1,
            ..HybridBudget::default()
        },
        HybridBudget {
            max_model_tokens: 1,
            ..HybridBudget::default()
        },
        HybridBudget {
            max_vector_bytes: 1,
            ..HybridBudget::default()
        },
        HybridBudget {
            max_text_bytes: 1,
            ..HybridBudget::default()
        },
        HybridBudget {
            max_duration: Duration::from_nanos(1),
            ..HybridBudget::default()
        },
    ] {
        assert!(hybrid_search(
            &index,
            &mut model,
            query,
            &HybridOptions {
                budget,
                ..HybridOptions::default()
            },
            None
        )
        .is_err());
        assert_eq!(
            model.info().inference_runs,
            0,
            "preflight failure must not infer"
        );
    }
    let response =
        hybrid_search(&index, &mut model, query, &HybridOptions::default(), None).unwrap();
    assert!(response.coverage.complete_index);
    assert_eq!(response.coverage.semantic_chunks, 3);
    assert!(!response.coverage.model_truncated);
    assert_eq!(response.hits[0].evidence.source, "relevant");
    assert!(response.hits[0].lexical_rank.is_none());
    let repeat = hybrid_search(&index, &mut model, query, &HybridOptions::default(), None).unwrap();
    assert_eq!(repeat.cost.native_inference_runs, 0);
    assert_eq!(repeat.cost.executed_model_tokens, 0);
    assert_eq!(repeat.cost.cache_hits, 4);
    assert_eq!(repeat.cost.model_tokens, response.cost.model_tokens);
    assert_eq!(
        serde_json::to_value(&repeat.hits).unwrap(),
        serde_json::to_value(&response.hits).unwrap()
    );
    let expanded = relevant
        .expand(&response.hits[0].evidence, 10, 10, Unit::Words, 1000)
        .unwrap();
    assert!(expanded.text.contains("Não se presume culpa"));
    assert!(expanded.text.contains("42"));
    let completed = model.info().completed_batches;
    let filtered = hybrid_search(
        &index,
        &mut model,
        query,
        &HybridOptions {
            coverage: SemanticCoverage::LexicalPrefilter,
            ..HybridOptions::default()
        },
        None,
    )
    .unwrap();
    assert!(!filtered.coverage.complete_index);
    assert_eq!(filtered.coverage.omitted_chunks, 3);
    assert!(filtered.hits.is_empty());
    assert_eq!(model.info().completed_batches, completed);
    let strict = hybrid_search(
        &index,
        &mut model,
        query,
        &HybridOptions {
            min_cosine: Some(1.0),
            ..HybridOptions::default()
        },
        None,
    )
    .unwrap();
    assert!(strict.hits.is_empty());

    let large = LexicalIndex::build(
        vec![doc("long", &"palavra ".repeat(1000))],
        ChunkConfig {
            tokens_per_chunk: 4096,
            overlap_tokens: 0,
        },
        IndexLimits::default(),
    )
    .unwrap();
    let before = model.info().inference_runs;
    let error = hybrid_search(
        &large,
        &mut model,
        "palavra",
        &HybridOptions::default(),
        None,
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("token window"));
    assert_eq!(model.info().inference_runs, before);

    let mixed = Arc::new(
        Document::from_bytes(
            "mixed.pdf",
            include_bytes!("fixtures/mixed.pdf"),
            "application/pdf",
            &Limits::default(),
        )
        .unwrap(),
    );
    let mixed_index =
        LexicalIndex::build(vec![mixed], ChunkConfig::default(), IndexLimits::default()).unwrap();
    let coverage = hybrid_search(
        &mixed_index,
        &mut model,
        "pagina",
        &HybridOptions::default(),
        None,
    )
    .unwrap()
    .coverage;
    assert!(coverage.complete_index);
    assert_eq!(
        coverage.empty_pdf_pages, 1,
        "index coverage must not hide a textless PDF page"
    );
}
