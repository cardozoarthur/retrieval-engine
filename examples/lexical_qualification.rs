//! Local cost smoke measurement plus opt-in official-source lexical retrieval.
use retrieval_engine::{ChunkConfig, Document, IndexLimits, LexicalIndex, Limits, Source, Unit};
use serde_json::json;
use std::{sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let text = (0..5000).map(|i| format!("Artigo {i}. O transporte depende de prazo e entrega. A responsabilidade exige prova e não dispensa exceção.\n")).collect::<String>();
    let local = Arc::new(Document::from_bytes(
        "synthetic-5000-clauses",
        text.as_bytes(),
        "text/plain",
        &Limits::default(),
    )?);
    let started = Instant::now();
    let index = LexicalIndex::build(vec![local], ChunkConfig::default(), IndexLimits::default())?;
    let build_us = started.elapsed().as_micros();
    let started = Instant::now();
    for _ in 0..100 {
        assert!(!index.search("responsabilidade 4917", 5)?.is_empty());
    }
    let query_100_us = started.elapsed().as_micros();
    let mut result = json!({"schema_version":1,"synthetic_cost_smoke":{
        "source_bytes":text.len(),"build_us":build_us,"query_100_us":query_100_us,"stats":index.stats(),
        "measurement_kind":"local unoptimized Rust dev profile; no universal latency claim",
        "memory_kind":"accounting units including retained source; not RSS"},"llm_calls":0});
    if std::env::args().any(|a| a == "--official") {
        let source = "https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf";
        let doc = Arc::new(Document::load(
            Source::Remote(source.into()),
            &Limits::default(),
        )?);
        let started = Instant::now();
        let index = LexicalIndex::build(
            vec![doc.clone()],
            ChunkConfig::default(),
            IndexLimits::default(),
        )?;
        let build_us = started.elapsed().as_micros();
        let started = Instant::now();
        let hits = index.search("responsabilidade tratamento dados", 3)?;
        let query_us = started.elapsed().as_micros();
        let hit = hits.first().ok_or("official lexical retrieval empty")?;
        assert_eq!(hit.matched_terms.len(), 3);
        let expanded = doc.expand(&hit.evidence, 10, 10, Unit::Words, 10000)?;
        result["actual_public_source"] = json!({"provenance":doc.provenance(),"stats":index.stats(),
            "build_us":build_us,"query_us":query_us,"ranked_hit":hit,"expanded":expanded,
            "legal_relevance_validated":false,"semantic_inference":false});
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
