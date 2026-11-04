//! Curation aid, run before any retrieval quality comparison. Output need not be
//! retained once the small source hashes/gold anchors are versioned in the corpus.
use retrieval_engine::{Document, Limits, Source};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let urls=["https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf","https://www.gov.br/saude/pt-br/assuntos/saude-de-a-a-z/d/dengue"];
    let mut sources = Vec::new();
    for url in urls {
        let (doc, timing) =
            Document::load_measured(Source::Remote(url.into()), &Limits::default())?;
        sources.push(serde_json::json!({"url":url,"provenance":doc.provenance(),"text":doc.text(),"timing":timing}));
    }
    println!("{}", serde_json::to_string(&sources)?);
    Ok(())
}
