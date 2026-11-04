use retrieval_engine::benchmark::{evaluate, Corpus, GoldAnchor, RankedSpan};
fn span(a: usize, b: usize) -> RankedSpan {
    RankedSpan {
        source_index: 0,
        start: a,
        end: b,
    }
}
#[test]
fn overlapping_hits_do_not_multiply_relevance_gain() {
    let gold = vec![GoldAnchor {
        source_index: 0,
        start: 20,
        end: 30,
        quote_sha256: "a".repeat(64),
    }];
    let ranks = [span(1, 10), span(19, 31), span(18, 32)];
    let m = evaluate(&gold, &ranks, 3).unwrap();
    assert_eq!(m.recall, Some(1.));
    assert_eq!(m.reciprocal_rank, Some(0.5));
    assert!((m.ndcg.unwrap() - 1. / 3.0_f64.log2()).abs() < 1e-9);
    assert_eq!(m.found_anchors, 1);
    assert_eq!(evaluate(&gold, &ranks, 1).unwrap().recall, Some(0.));
}
#[test]
fn partial_quote_wrong_source_and_no_evidence_are_distinct() {
    let gold = vec![GoldAnchor {
        source_index: 0,
        start: 20,
        end: 30,
        quote_sha256: "a".repeat(64),
    }];
    let wrong = [
        span(20, 29),
        RankedSpan {
            source_index: 1,
            start: 10,
            end: 40,
        },
    ];
    assert_eq!(evaluate(&gold, &wrong, 5).unwrap().recall, Some(0.));
    let m = evaluate(&[], &wrong, 5).unwrap();
    assert_eq!(m.recall, None);
    assert_eq!(m.negative_returned_any, Some(true));
    assert_eq!(
        evaluate(&[], &[], 5).unwrap().negative_returned_any,
        Some(false)
    );
    assert!(evaluate(&gold, &[span(1, 5), span(1, 5)], 5).is_err());
}
#[test]
fn corpus_rejects_bad_version_duplicate_ids_and_stale_documents() {
    let bytes = include_bytes!("../benchmarks/pt-official-pilot-v1.json");
    let corpus = Corpus::parse(bytes).unwrap();
    assert_eq!(corpus.queries.len(), 24);
    assert!(corpus.validate_documents(&[]).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    value["schema_version"] = 2.into();
    assert!(Corpus::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    value["schema_version"] = 1.into();
    value["queries"][1]["id"] = value["queries"][0]["id"].clone();
    assert!(Corpus::parse(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn corpus_checks_the_actual_snapshot_and_every_gold_hash() {
    use retrieval_engine::{Document, Limits};
    use sha2::{Digest, Sha256};
    let doc = Document::from_bytes(
        "https://example.org/test",
        "Não se presume culpa.".as_bytes(),
        "text/plain",
        &Limits::default(),
    )
    .unwrap();
    let p = doc.provenance();
    let mut value = serde_json::json!({"schema_version":1,"name":"test","scope":"fixture only","chunk_tokens":24,"overlap_tokens":12,"sources":[{"requested_source":p.requested_source,"final_source":p.final_source,"raw_sha256":p.raw_sha256,"text_sha256":p.text_sha256,"snapshot_id":p.snapshot_id}],"queries":[{"id":"q","text":"Presume culpa?","category":"negation","gold":[{"source_index":0,"start":0,"end":3,"quote_sha256":format!("{:x}",Sha256::digest("Não".as_bytes()))}]}]});
    let parse = |v: &serde_json::Value| Corpus::parse(&serde_json::to_vec(v).unwrap()).unwrap();
    parse(&value)
        .validate_documents(std::slice::from_ref(&doc))
        .unwrap();
    value["queries"][0]["gold"][0]["quote_sha256"] = "0".repeat(64).into();
    assert!(parse(&value)
        .validate_documents(std::slice::from_ref(&doc))
        .is_err());
    value["sources"][0]["raw_sha256"] = "0".repeat(64).into();
    assert!(parse(&value).validate_documents(&[doc]).is_err());
}
