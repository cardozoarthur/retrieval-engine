use retrieval_engine::{ChunkConfig, Document, IndexLimits, LexicalIndex, Limits, Source, Unit};
use std::{sync::Arc, time::Instant};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 || args.len() > 7 {
        return Err(
            "usage: retrieval-engine SOURCE QUERY [BEFORE=20] [AFTER=20] [chars|words] [literal|bm25|hybrid|hybrid-prefilter] [MODEL_DIR]"
                .into(),
        );
    }
    let before = args.get(2).map(|x| x.parse()).transpose()?.unwrap_or(20);
    let after = args.get(3).map(|x| x.parse()).transpose()?.unwrap_or(20);
    let unit = match args.get(4).map(String::as_str).unwrap_or("chars") {
        "chars" => Unit::Characters,
        "words" => Unit::Words,
        _ => return Err("unit must be chars or words".into()),
    };
    let mode = args.get(5).map(String::as_str).unwrap_or("literal");
    if !matches!(mode, "literal" | "bm25" | "hybrid" | "hybrid-prefilter") {
        return Err("mode must be literal, bm25, hybrid or hybrid-prefilter".into());
    }
    if args.len() == 7 && !mode.starts_with("hybrid") {
        return Err("MODEL_DIR is only valid in hybrid modes".into());
    }
    if mode.starts_with("hybrid") && !cfg!(feature = "cpu-onnx") {
        return Err(
            "hybrid requires a build with --features cpu-onnx and pinned local assets".into(),
        );
    }
    let source = if args[0].contains("://") {
        Source::Remote(args[0].clone())
    } else {
        Source::Local(args[0].clone().into())
    };
    let started = Instant::now();
    let doc = Arc::new(Document::load(source, &Limits::default())?);
    let acquired_ms = started.elapsed().as_millis();
    let (results, search) = if mode == "literal" {
        let hits = doc.find(&args[1], 100)?;
        let results = hits
            .iter()
            .map(|hit| {
                Ok(serde_json::json!({
                    "hit":hit,"context":doc.expand(hit,before,after,unit,16_384)?
                }))
            })
            .collect::<Result<Vec<_>, retrieval_engine::Error>>()?;
        (
            results,
            serde_json::json!({"mode":"literal","max_hits":100,"case_sensitive":true,
            "result_limit_reached":hits.len()==100}),
        )
    } else if mode == "bm25" {
        let build_start = Instant::now();
        let index = LexicalIndex::build(
            vec![doc.clone()],
            ChunkConfig::default(),
            IndexLimits::default(),
        )?;
        let build_us = build_start.elapsed().as_micros();
        let query_start = Instant::now();
        let hits = index.search(&args[1], 10)?;
        let query_us = query_start.elapsed().as_micros();
        let results = hits.iter().map(|ranked| Ok(serde_json::json!({
            "hit":ranked.evidence,"score":ranked.score,"matched_terms":ranked.matched_terms,
            "chunk_index":ranked.chunk_index,"context":doc.expand(&ranked.evidence,before,after,unit,16_384)?
        }))).collect::<Result<Vec<_>,retrieval_engine::Error>>()?;
        (
            results,
            serde_json::json!({"mode":"bm25","top_k":10,"k1":1.2,"b":0.75,
            "stats":index.stats(),"chunk_config":index.config(),"build_us":build_us,"query_us":query_us,
            "result_limit_reached":hits.len()==10,"semantic_inference":false}),
        )
    } else {
        #[cfg(feature = "cpu-onnx")]
        {
            use retrieval_engine::{
                hybrid_search, CpuConfig, E5Embedder, HybridOptions, SemanticCoverage,
            };
            let build_start = Instant::now();
            let index = LexicalIndex::build(
                vec![doc.clone()],
                ChunkConfig::default(),
                IndexLimits::default(),
            )?;
            let build_us = build_start.elapsed().as_micros();
            let mut model = E5Embedder::load(
                args.get(6)
                    .map(String::as_str)
                    .unwrap_or("assets/e5-small-int8"),
                CpuConfig::default(),
            )?;
            let mut options = HybridOptions {
                coverage: if mode == "hybrid" {
                    SemanticCoverage::Full
                } else {
                    SemanticCoverage::LexicalPrefilter
                },
                ..HybridOptions::default()
            };
            if mode == "hybrid-prefilter" {
                options.lexical_candidates = 16;
                options.semantic_candidates = 16;
                options.budget.max_semantic_chunks = 16;
            }
            let response = hybrid_search(&index, &mut model, &args[1], &options, None)?;
            let results = response.hits.iter().map(|ranked| Ok(serde_json::json!({
                "hit":ranked.evidence,"rrf_score":ranked.rrf_score,"lexical_rank":ranked.lexical_rank,
                "lexical_score":ranked.lexical_score,"semantic_rank":ranked.semantic_rank,"cosine":ranked.cosine,
                "matched_terms":ranked.matched_terms,"rerank_score":ranked.rerank_score,
                "context":doc.expand(&ranked.evidence,before,after,unit,16_384)?
            }))).collect::<Result<Vec<_>,retrieval_engine::Error>>()?;
            (
                results,
                serde_json::json!({"mode":mode,"top_k":options.top_k,"rrf_k":response.rrf_k,
                "min_cosine":response.min_cosine,"coverage":response.coverage,"cost":response.cost,
                "model":response.model,"reranker":response.reranker,"semantic_inference":true,
                "query_sha256":response.query_sha256,"index_stats":index.stats(),"index_build_us":build_us}),
            )
        }
        #[cfg(not(feature = "cpu-onnx"))]
        {
            return Err("hybrid requires --features cpu-onnx".into());
        }
    };
    let result = serde_json::json!({ "schema_version": 1, "provenance": doc.provenance(),
        "warnings": doc.warnings(), "pages": doc.pages(), "results": results,
        "search": search, "acquisition_and_extraction_ms":acquired_ms });
    serde_json::to_writer_pretty(std::io::stdout().lock(), &result)?;
    println!();
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
