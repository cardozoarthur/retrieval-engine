use retrieval_engine::{
    hybrid_search, ChunkConfig, CpuConfig, Document, E5Embedder, HybridOptions, IndexLimits,
    LexicalIndex, Limits, SemanticCoverage, Source, Unit,
};
use serde_json::json;
use std::{sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = E5Embedder::load("assets/e5-small-int8", CpuConfig::default())?;
    let docs: Vec<_> = [
        ("synthetic-relevant", "O agente indeniza vítimas quando há tratamento ilícito com dados pessoais. Não se presume culpa; exceção exige prova, artigo 42."),
        ("synthetic-transport", "Cargas seguem pela rodovia com frete previamente pago."),
        ("synthetic-food", "A receita inclui farinha, ovos, leite."),
    ].into_iter().map(|(name,text)| Document::from_bytes(name,text.as_bytes(),"text/plain",&Limits::default()).map(Arc::new)).collect::<Result<_,_>>()?;
    let index = LexicalIndex::build(docs, ChunkConfig::default(), IndexLimits::default())?;
    let query = "Quem paga pelos prejuízos causados por vazamento de informações?";
    let lexical = index.search(query, 10)?;
    assert!(lexical.is_empty());
    let response = hybrid_search(&index, &mut model, query, &HybridOptions::default(), None)?;
    assert_eq!(response.hits[0].evidence.source, "synthetic-relevant");
    let filtered = hybrid_search(
        &index,
        &mut model,
        query,
        &HybridOptions {
            coverage: SemanticCoverage::LexicalPrefilter,
            ..HybridOptions::default()
        },
        None,
    )?;
    assert!(filtered.hits.is_empty() && !filtered.coverage.complete_index);
    let mut result = json!({"schema_version":1,"actual_e5_cpu":true,"model_loaded_once_in_example_source":true,
        "synthetic_paraphrase_fixture":{"query":query,"bm25_hits":lexical.len(),"full":response,"prefilter":filtered},
        "large_llm_calls":0,"juridical_quality_validated":false});
    if std::env::args().any(|a| a == "--official") {
        let started = Instant::now();
        let doc = Arc::new(Document::load(Source::Remote("https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf".into()),&Limits::default())?);
        let acquisition_ms = started.elapsed().as_millis();
        let started = Instant::now();
        let index = LexicalIndex::build(
            vec![doc.clone()],
            ChunkConfig::default(),
            IndexLimits::default(),
        )?;
        let build_us = started.elapsed().as_micros();
        let mut options = HybridOptions {
            top_k: 3,
            ..HybridOptions::default()
        };
        if std::env::args().any(|a| a == "--prefilter-only") {
            options.coverage = SemanticCoverage::LexicalPrefilter;
            options.lexical_candidates = 16;
            options.semantic_candidates = 16;
            options.budget.max_semantic_chunks = 16;
        }
        let response = hybrid_search(
            &index,
            &mut model,
            "Quem deve reparar danos provocados por uso indevido de informações pessoais?",
            &options,
            None,
        )?;
        assert!(!response.hits.is_empty());
        if options.coverage == SemanticCoverage::Full {
            assert!(response.coverage.complete_index);
        } else {
            assert_eq!(response.coverage.semantic_chunks, 16);
            assert!(!response.coverage.complete_index);
        }
        assert!(response.hits.iter().all(|h| !h.evidence.pages.is_empty()));
        let expanded = doc.expand(&response.hits[0].evidence, 10, 20, Unit::Words, 12000)?;
        result["actual_public_source"] = json!({"provenance":doc.provenance(),"acquisition_ms":acquisition_ms,
            "index_build_us":build_us,"index_stats":index.stats(),"response":response,
            "expanded_first_hit_same_snapshot":expanded,"warnings":doc.warnings()});
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
