use retrieval_engine::{ChunkConfig, Document, IndexLimits, LexicalIndex, Limits, Unit};
use std::sync::Arc;

fn doc(source: &str, text: &str) -> Arc<Document> {
    Arc::new(
        Document::from_bytes(source, text.as_bytes(), "text/plain", &Limits::default()).unwrap(),
    )
}
fn index(docs: Vec<Arc<Document>>) -> LexicalIndex {
    LexicalIndex::build(docs, ChunkConfig::default(), IndexLimits::default()).unwrap()
}

#[test]
fn portuguese_terms_rank_and_remain_expandable() {
    let relevant = doc("a", "Art. 42. A responsabilidade pelo tratamento indevido não dispensa prova. Exceção prevista no artigo 43.");
    let idx = index(vec![
        doc("b", "Regras de transporte e frete."),
        relevant.clone(),
        doc("c", "Responsabilidade administrativa isolada."),
    ]);
    let hits = idx.search("RESPONSABILIDADE tratamento", 5).unwrap();
    assert_eq!(hits[0].evidence.source, "a");
    assert_eq!(
        hits[0].matched_terms,
        vec!["responsabilidade", "tratamento"]
    );
    assert!(hits[0].evidence.text.contains("não dispensa"));
    assert!(hits[0].evidence.text.contains("Exceção"));
    assert!(relevant
        .expand(&hits[0].evidence, 1, 1, Unit::Words, 1000)
        .is_ok());
    assert!(idx.search("vazamento", 5).unwrap().is_empty());
}

#[test]
fn accents_numbers_negation_and_duplicate_query_terms() {
    let idx = index(vec![
        doc("a", "AÇÃO artigo 42 não permite"),
        doc("b", "ação artigo 142 permite"),
    ]);
    assert_eq!(idx.search("acao 42", 5).unwrap()[0].evidence.source, "a");
    assert_eq!(idx.search("42", 5).unwrap().len(), 1);
    let negation = idx.search("não", 5).unwrap();
    assert_eq!(negation.len(), 1);
    assert_eq!(negation[0].matched_terms, vec!["nao"]);
    assert_eq!(
        idx.search("42 42", 5).unwrap()[0].score,
        idx.search("42", 5).unwrap()[0].score
    );
}

#[test]
fn bm25_formula_and_stable_ties() {
    let idx = index(vec![
        doc("first", "alpha alpha"),
        doc("second", "alpha alpha"),
    ]);
    let hits = idx.search("alpha", 2).unwrap();
    let idf = (1.0_f64 + (2.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
    let expected = idf * (2.0 * 2.2) / (2.0 + 1.2);
    assert!((hits[0].score - expected).abs() < 1e-12);
    assert_eq!(hits[0].evidence.source, "first");
    assert_eq!(hits[1].evidence.source, "second");
}

#[test]
fn overlapping_windows_keep_original_unicode_spans() {
    let source = doc("x", "um dois três quatro cinco seis sete oito nove");
    let idx = LexicalIndex::build(
        vec![source.clone()],
        ChunkConfig {
            tokens_per_chunk: 4,
            overlap_tokens: 2,
        },
        IndexLimits::default(),
    )
    .unwrap();
    let hit = idx.search("QUATRO cinco", 1).unwrap().remove(0);
    assert_eq!(hit.matched_terms.len(), 2);
    assert!(hit.evidence.text.contains("quatro cinco"));
    assert!(source.expand(&hit.evidence, 1, 1, Unit::Words, 200).is_ok());
    assert_eq!(idx.stats().chunks, 4);
}

#[test]
fn budgets_and_invalid_requests_fail_explicitly() {
    let docs = vec![doc("x", "um dois três quatro")];
    assert!(LexicalIndex::build(
        docs.clone(),
        ChunkConfig {
            tokens_per_chunk: 0,
            overlap_tokens: 0
        },
        IndexLimits::default()
    )
    .is_err());
    assert!(LexicalIndex::build(
        docs.clone(),
        ChunkConfig {
            tokens_per_chunk: 2,
            overlap_tokens: 2
        },
        IndexLimits::default()
    )
    .is_err());
    for limits in [
        IndexLimits {
            max_documents: 0,
            ..IndexLimits::default()
        },
        IndexLimits {
            max_tokens: 1,
            ..IndexLimits::default()
        },
        IndexLimits {
            max_chunks: 1,
            ..IndexLimits::default()
        },
        IndexLimits {
            max_accounted_bytes: 1,
            ..IndexLimits::default()
        },
    ] {
        assert!(LexicalIndex::build(
            docs.clone(),
            ChunkConfig {
                tokens_per_chunk: 2,
                overlap_tokens: 0
            },
            limits
        )
        .is_err());
    }
    let idx = index(docs);
    for query in ["", "...", "§ !"] {
        assert!(idx.search(query, 5).is_err());
    }
    assert!(idx.search("um", 0).is_err());
    assert!(idx.search("um", 1001).is_err());
    assert!(idx.search(&"a".repeat(5000), 5).is_err());
    let empty = index(vec![]);
    assert!(empty.search("query", 5).unwrap().is_empty());
}

#[test]
fn decomposed_unicode_and_concurrent_queries_keep_spans() {
    let source = doc(
        "unicode",
        "A ac\u{327}a\u{303}o não depende do artigo 142, mas do artigo 42.",
    );
    let idx = Arc::new(index(vec![source.clone()]));
    let expected = idx.search("AÇÃO 42", 1).unwrap();
    assert_eq!(expected[0].matched_terms, vec!["42", "acao"]);
    assert!(source
        .expand(&expected[0].evidence, 0, 0, Unit::Characters, 1000)
        .is_ok());
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let index = idx.clone();
            std::thread::spawn(move || index.search("AÇÃO 42", 1).unwrap()[0].score)
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), expected[0].score);
    }
    assert!(LexicalIndex::build(
        vec![source.clone(), source],
        ChunkConfig::default(),
        IndexLimits::default()
    )
    .is_err());
}
