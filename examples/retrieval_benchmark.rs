//! Bounded end-to-end CPU measurement. No stdout report until every source,
//! gold/window/budget/inference check has passed. One profile per process.
use retrieval_engine::{
    benchmark::{evaluate, Corpus, RankedSpan},
    benchmark_model::{BenchmarkEncoder, BenchmarkProfile},
    ChunkConfig, Document, IndexLimits, InputKind, LexicalIndex, Limits, RequestControl, Source,
    StoredVector, VectorFormat,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    sync::Arc,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err(
            "usage: retrieval_benchmark DATASET e5-int8|e5-fp32|minilm-int8|minilm-fp32".into(),
        );
    }
    let profile = match args[1].as_str() {
        "e5-int8" => BenchmarkProfile::E5Int8,
        "e5-fp32" => BenchmarkProfile::E5Fp32,
        "minilm-int8" => BenchmarkProfile::MiniLmInt8,
        "minilm-fp32" => BenchmarkProfile::MiniLmFp32,
        _ => return Err("unknown benchmark profile".into()),
    };
    let mut bytes = Vec::new();
    std::fs::File::open(&args[0])?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    let corpus = Corpus::parse(&bytes)?;
    let dataset_sha = format!("{:x}", Sha256::digest(&bytes));
    let mut documents = Vec::new();
    let mut acquisition = Vec::new();
    for source in &corpus.sources {
        let (doc, timing) = Document::load_measured(
            Source::Remote(source.requested_source.clone()),
            &Limits::default(),
        )?;
        acquisition.push(json!({"provenance":doc.provenance(),"timings":timing,"pages":doc.pages().len(),"warnings":doc.warnings()}));
        documents.push(doc);
    }
    corpus.validate_documents(&documents)?; // before model loading or scoring
    let started = Instant::now();
    let index = LexicalIndex::build(
        documents.into_iter().map(Arc::new).collect(),
        ChunkConfig {
            tokens_per_chunk: corpus.chunk_tokens,
            overlap_tokens: corpus.overlap_tokens,
        },
        IndexLimits {
            max_chunks: 3000,
            ..IndexLimits::default()
        },
    )?;
    let index_us = started.elapsed().as_micros();
    let chunks = (0..index.stats().chunks)
        .map(|i| index.chunk_evidence(i).ok_or("missing chunk"))
        .collect::<Result<Vec<_>, _>>()?;
    // Refuse a dataset for which no common candidate contains the full gold.
    for q in &corpus.queries {
        for g in &q.gold {
            if !chunks
                .iter()
                .any(|(s, e)| *s == g.source_index && e.start <= g.start && e.end >= g.end)
            {
                return Err(format!("gold {} is not wholly covered by any window", q.id).into());
            }
        }
    }
    let mut encoder = BenchmarkEncoder::load("assets", profile, 2)?;
    // Full preflight prevents wasting inference on a corpus that only fits some
    // models. Same versioned windows; never silently split one model differently.
    let preflight_started = Instant::now();
    for (_, e) in &chunks {
        encoder.token_count(InputKind::Passage, &e.text)?;
    }
    for q in &corpus.queries {
        encoder.token_count(InputKind::Query, &q.text)?;
    }
    let token_preflight_us = preflight_started.elapsed().as_micros();
    let control = RequestControl::new(Duration::from_secs(180))?;
    let cold = encoder.embed(InputKind::Query, &corpus.queries[0].text, &control)?;
    let mut vectors = Vec::new();
    let (mut tokenize, mut infer, mut pool, mut tokens) = (0u128, 0u128, 0u128, 0usize);
    let passage_start = Instant::now();
    for (_, e) in &chunks {
        let measured = encoder.embed(InputKind::Passage, &e.text, &control)?;
        tokenize += measured.tokenization_us;
        infer += measured.inference_us;
        pool += measured.pooling_us;
        tokens += measured.tokens;
        vectors.push(measured.vector);
    }
    let passage_wall_us = passage_start.elapsed().as_micros();
    let mut results = Vec::new();
    let mut representations = Vec::new();
    let formats = [VectorFormat::Fp32, VectorFormat::Int8, VectorFormat::Binary];
    for format in formats {
        let started = Instant::now();
        let stored = vectors
            .iter()
            .map(|v| StoredVector::encode(v, format))
            .collect::<Result<Vec<_>, _>>()?;
        let encode_us = started.elapsed().as_micros();
        representations.push(json!({"format":format,"encode_us":encode_us,"payload_bytes":stored.iter().map(StoredVector::payload_bytes).sum::<usize>(),"wire_bytes":stored.iter().map(|v|v.to_bytes().len()).sum::<usize>(),"includes_int8_scale":true,"excludes_text_index_allocator":true}));
        // These query embeddings are intentionally real inference for each format
        // and repeat, not cached. Timings/counters expose the resulting work.
        for repeat in 0..2 {
            for q in &corpus.queries {
                control.checkpoint()?;
                let measured = encoder.embed(InputKind::Query, &q.text, &control)?;
                let started = Instant::now();
                let query = StoredVector::encode(&measured.vector, format)?;
                let query_encode_us = started.elapsed().as_micros();
                let started = Instant::now();
                let mut scores = stored
                    .iter()
                    .enumerate()
                    .map(|(i, v)| query.similarity(v).map(|s| (i, s as f64)))
                    .collect::<Result<Vec<_>, _>>()?;
                let comparison_us = started.elapsed().as_micros();
                let started = Instant::now();
                scores.sort_by(|(ai, a), (bi, b)| b.total_cmp(a).then(ai.cmp(bi)));
                let sorting_us = started.elapsed().as_micros();
                let started = Instant::now();
                let lexical = index.search(&q.text, chunks.len().min(1000))?;
                let lexical_us = started.elapsed().as_micros();
                let lexical_ids: Vec<_> = lexical.iter().map(|h| h.chunk_index).collect();
                let semantic_ids: Vec<_> = scores.iter().map(|&(i, _)| i).collect();
                let started = Instant::now();
                let mut fused = vec![0f64; chunks.len()];
                for (rank, &id) in lexical_ids.iter().enumerate() {
                    fused[id] += 1. / (60. + (rank + 1) as f64);
                }
                for (rank, &id) in semantic_ids.iter().enumerate() {
                    fused[id] += 1. / (60. + (rank + 1) as f64);
                }
                let mut hybrid_ids: Vec<_> = (0..chunks.len()).collect();
                hybrid_ids.sort_by(|&a, &b| fused[b].total_cmp(&fused[a]).then(a.cmp(&b)));
                let fusion_us = started.elapsed().as_micros();
                let mut rankings = Vec::new();
                for (mode, ids) in [
                    ("bm25", &lexical_ids),
                    ("semantic", &semantic_ids),
                    ("hybrid_rrf60", &hybrid_ids),
                ] {
                    let spans: Vec<_> = ids
                        .iter()
                        .map(|&i| {
                            let (s, e) = &chunks[i];
                            RankedSpan {
                                source_index: *s,
                                start: e.start,
                                end: e.end,
                            }
                        })
                        .collect();
                    let metrics = [1, 3, 5, 10]
                        .into_iter()
                        .map(|k| evaluate(&q.gold, &spans, k))
                        .collect::<Result<Vec<_>, _>>()?;
                    let top:Vec<Value>=ids.iter().take(10).map(|&id|{let(s,e)=&chunks[id];json!({"chunk_index":id,"source_index":s,"start":e.start,"end":e.end,"pages":e.pages,"quote_sha256":format!("{:x}",Sha256::digest(e.text.as_bytes()))})}).collect();
                    rankings.push(json!({"mode":mode,"metrics":metrics,"top":top}));
                }
                results.push(json!({"query_id":q.id,"category":q.category,"gold":q.gold,"repeat":repeat,"format":format,"rankings":rankings,"tokens":measured.tokens,"tokenization_us":measured.tokenization_us,"inference_us":measured.inference_us,"pooling_us":measured.pooling_us,"query_encode_us":query_encode_us,"comparison_us":comparison_us,"sorting_us":sorting_us,"bm25_us":lexical_us,"fusion_us":fusion_us}));
            }
        }
    }
    control.checkpoint()?;
    let expected_runs = 1 + chunks.len() + 3 * 2 * corpus.queries.len();
    if encoder.info().inference_runs != expected_runs {
        return Err("native inference accounting mismatch".into());
    }
    let report = json!({"schema_version":1,"complete":true,"dataset":corpus.name,"dataset_sha256":dataset_sha,"scope":corpus.scope,"build_profile":"cargo dev: Rust unoptimized, debug=0, incremental=false; vendor native runtime optimized","hardware_measurement":"host wrapper records OS/hardware/process memory; no controlled-device or universal latency claim","source_timings":acquisition,"index":{"config":index.config(),"stats":index.stats(),"build_us":index_us},"model":encoder.info(),"token_preflight_us":token_preflight_us,"session_first_query":{"tokenization_us":cold.tokenization_us,"inference_us":cold.inference_us,"pooling_us":cold.pooling_us,"tokens":cold.tokens,"os_page_cache_controlled":false},"passage_embeddings":{"count":chunks.len(),"tokens":tokens,"wall_us":passage_wall_us,"tokenization_us":tokenize,"inference_us":infer,"pooling_us":pool},"representations":representations,"results":results,"protocol":{"repeats":2,"cache_enabled":false,"single_row_native_batches":true,"silent_truncation":false,"semantic_coverage":"all indexed windows","lexical_max_candidates":1000,"semantic_similarity_threshold":null,"negative_outputs_are_not_confidence":true,"gold_gain":"one full source anchor per positive query; overlapping windows cannot gain twice","fusion":"RRF60, complete semantic ranking plus up to 1000 positive BM25 results","large_llm_calls":0,"held_out_generalization_proven":false,"default_promotion_authorized":false}});
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
