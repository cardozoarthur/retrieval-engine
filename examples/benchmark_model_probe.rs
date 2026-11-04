//! Real native probe, not a corpus quality benchmark. All output is admitted only
//! after repeatability, no-truncation and vector format checks succeed.
use retrieval_engine::{
    benchmark_model::{BenchmarkEncoder, BenchmarkProfile},
    cosine_similarity, CpuConfig, E5Embedder, InputKind, RequestControl, StoredVector,
    VectorFormat,
};
use serde_json::json;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile = match std::env::args().nth(1).as_deref() {
        Some("e5-int8") => BenchmarkProfile::E5Int8,
        Some("e5-fp32") => BenchmarkProfile::E5Fp32,
        Some("minilm-int8") => BenchmarkProfile::MiniLmInt8,
        Some("minilm-fp32") => BenchmarkProfile::MiniLmFp32,
        _ => {
            return Err(
                "usage: benchmark_model_probe e5-int8|e5-fp32|minilm-int8|minilm-fp32".into(),
            )
        }
    };
    let mut encoder = BenchmarkEncoder::load("assets", profile, 2)?;
    let control = RequestControl::new(Duration::from_secs(60))?;
    let query = "Quem paga pelos prejuízos causados por vazamento de informações?";
    let passage="O responsável indeniza os danos decorrentes do tratamento ilícito de dados pessoais, salvo as exceções previstas na lei.";
    let first = encoder.embed(InputKind::Query, query, &control)?;
    let second = encoder.embed(InputKind::Passage, passage, &control)?;
    let repeated = encoder.embed(InputKind::Query, query, &control)?;
    let stability = cosine_similarity(&first.vector, &repeated.vector)?;
    assert!(stability > 0.99999);
    assert!(encoder
        .token_count(InputKind::Passage, &"responsabilidade ".repeat(1000))
        .is_err());
    let mut formats = Vec::new();
    for format in [VectorFormat::Fp32, VectorFormat::Int8, VectorFormat::Binary] {
        let q = StoredVector::encode(&first.vector, format)?;
        let p = StoredVector::encode(&second.vector, format)?;
        assert_eq!(
            StoredVector::from_bytes(&p.to_bytes())?.to_bytes(),
            p.to_bytes()
        );
        formats.push(json!({"format":format,"payload_bytes_per_vector":p.payload_bytes(),"wire_bytes_per_vector":p.to_bytes().len(),"similarity":q.similarity(&p)?}));
    }
    let production_match = if profile == BenchmarkProfile::E5Int8 {
        let mut production = E5Embedder::load(
            "assets/e5-small-int8",
            CpuConfig {
                cache_max_bytes: 0,
                ..CpuConfig::default()
            },
        )?;
        let actual = production.embed(InputKind::Query, &[query.into()])?;
        let similarity = cosine_similarity(&first.vector, &actual.vectors[0])?;
        assert!(similarity > 0.99999);
        Some(similarity)
    } else {
        None
    };
    control.checkpoint()?;
    assert_eq!(encoder.info().inference_runs, 3);
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"schema_version":1,"scope":"real CPU model probe; synthetic text; not retrieval-quality validation","model":encoder.info(),"stability_cosine":stability,"matches_production_e5_cosine":production_match,"tokens":[first.tokens,second.tokens,repeated.tokens],"tokenization_us":[first.tokenization_us,second.tokenization_us,repeated.tokenization_us],"inference_us":[first.inference_us,second.inference_us,repeated.inference_us],"pooling_us":[first.pooling_us,second.pooling_us,repeated.pooling_us],"formats":formats,"window_excess_rejected":true,"large_llm_calls":0})
        )?
    );
    Ok(())
}
