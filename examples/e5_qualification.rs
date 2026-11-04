use retrieval_engine::{cosine_similarity, CpuConfig, E5Embedder, InputKind};
use serde_json::json;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args()
        .nth(1)
        .unwrap_or("assets/e5-small-int8".into());
    let mut model = E5Embedder::load(&root, CpuConfig::default())?;
    let query = "Quem paga pelos prejuízos causados por vazamento de informações?";
    let passages: Vec<String> = [
        "O agente responde pela reparação dos danos decorrentes do tratamento indevido de dados pessoais.",
        "O transporte rodoviário de cargas depende do recolhimento de tributos e frete.",
        "A receita de bolo inclui farinha, ovos e leite.",
    ].into_iter().map(String::from).collect();
    let q = model.embed(InputKind::Query, &[query.into()])?;
    let p = model.embed(InputKind::Passage, &passages)?;
    let started = Instant::now();
    let scores = p
        .vectors
        .iter()
        .map(|v| cosine_similarity(&q.vectors[0], v))
        .collect::<Result<Vec<_>, _>>()?;
    let comparison_us = started.elapsed().as_micros();
    assert!(scores[0] > scores[1] && scores[0] > scores[2]);
    let repeated = model.embed(InputKind::Passage, &passages[..1])?;
    let repeat_cosine = cosine_similarity(&p.vectors[0], &repeated.vectors[0])?;
    assert!(repeat_cosine > 0.999);
    let timings = |batch: &retrieval_engine::EmbeddingBatch| {
        json!({"tokenization_us":batch.tokenization_us,
        "inference_us":batch.inference_us,"pooling_us":batch.pooling_us,"original_token_counts":batch.original_token_counts,
        "truncated":batch.truncated})
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"schema_version":1,"actual_cpu_inference":true,
        "model":model.info(),"query":query,"passages":passages,"cosine_scores":scores,
        "same_passage_batch_vs_single_cosine":repeat_cosine,"query_timing":timings(&q),"passages_timing":timings(&p),
        "comparison_us":comparison_us,"synthetic_quality_fixture":true,"legal_relevance_validated":false,
        "gpu_used":false,"large_llm_calls":0}))?
    );
    Ok(())
}
