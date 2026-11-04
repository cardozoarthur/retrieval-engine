//! Explicit opt-in network qualification; not run by cargo test.
use retrieval_engine::{Document, Limits, Source, Unit};
use serde_json::json;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = "https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl";
    let mut receipts = Vec::new();
    for extension in ["html", "pdf"] {
        let started = Instant::now();
        let doc = Document::load(
            Source::Remote(format!("{base}.{extension}")),
            &Limits::default(),
        )?;
        let elapsed = started.elapsed().as_millis();
        let hits = doc.find("responsabilidade", 100)?;
        let first = hits
            .first()
            .ok_or("official source did not contain expected literal")?;
        let context = doc.expand(first, 8, 15, Unit::Words, 2000)?;
        assert!(context.text.contains(&first.text));
        assert!(context.start < first.start && context.end > first.end);
        assert_eq!(context.snapshot_id, first.snapshot_id);
        if extension == "pdf" {
            assert!(!first.pages.is_empty());
        }
        receipts.push(
            json!({"provenance":doc.provenance(), "page_count":doc.pages().len(),
            "warnings":doc.warnings(), "literal_hit_count_capped_at_100": hits.len(),
            "hit":first,"expanded_context":context,"acquisition_and_extraction_ms":elapsed,
            "expansion_used_same_in_memory_document":true}),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"schema_version":1,
        "kind":"actual_public_https_qualification", "sources":receipts,
        "legal_validity_verified":false,"llm_calls":0,"model_inference":false}))?
    );
    Ok(())
}
