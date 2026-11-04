//! Small, versioned retrieval evaluation contract. Labels describe source spans,
//! never generated answers. Overlapping windows cannot earn the same gain twice.
use crate::{
    document::{error, sha},
    Document, Error,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub schema_version: u32,
    pub name: String,
    pub scope: String,
    pub chunk_tokens: usize,
    pub overlap_tokens: usize,
    pub sources: Vec<CorpusSource>,
    pub queries: Vec<CorpusQuery>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusSource {
    pub requested_source: String,
    pub final_source: String,
    pub raw_sha256: String,
    pub text_sha256: String,
    pub snapshot_id: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusQuery {
    pub id: String,
    pub text: String,
    pub category: String,
    pub gold: Vec<GoldAnchor>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoldAnchor {
    pub source_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote_sha256: String,
}

impl Corpus {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 128 * 1024 {
            return Err(error("benchmark dataset exceeds 128KiB"));
        }
        let corpus: Self = serde_json::from_slice(bytes).map_err(error)?;
        if corpus.schema_version != 1
            || corpus.name.is_empty()
            || corpus.name.len() > 128
            || corpus.scope.len() > 4096
            || !(1..=8).contains(&corpus.sources.len())
            || !(1..=128).contains(&corpus.queries.len())
            || !(8..=192).contains(&corpus.chunk_tokens)
            || corpus.overlap_tokens >= corpus.chunk_tokens
        {
            return Err(error("invalid benchmark corpus limits/version"));
        }
        let valid_hash = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        };
        let mut ids = BTreeSet::new();
        for source in &corpus.sources {
            if !source.requested_source.starts_with("https://")
                || source.requested_source.len() > 4096
                || source.final_source.len() > 4096
                || [&source.raw_sha256, &source.text_sha256, &source.snapshot_id]
                    .iter()
                    .any(|s| !valid_hash(s))
            {
                return Err(error("invalid benchmark source identity"));
            }
        }
        for q in &corpus.queries {
            if q.id.is_empty()
                || q.id.len() > 128
                || !ids.insert(&q.id)
                || q.text.trim().is_empty()
                || q.text.len() > 4096
                || q.category.is_empty()
                || q.category.len() > 128
                || q.gold.len() > 1
            {
                return Err(error("invalid benchmark query/duplicate ID"));
            }
            let mut spans = BTreeSet::new();
            for g in &q.gold {
                if g.source_index >= corpus.sources.len()
                    || g.start >= g.end
                    || g.end - g.start > 4096
                    || !valid_hash(&g.quote_sha256)
                    || !spans.insert((g.source_index, g.start, g.end))
                {
                    return Err(error("invalid/duplicate benchmark gold anchor"));
                }
            }
        }
        Ok(corpus)
    }
    /// Must run before indexing/inference, including every source/gold quote.
    pub fn validate_documents(&self, documents: &[Document]) -> Result<(), Error> {
        if documents.len() != self.sources.len() {
            return Err(error("benchmark source count differs"));
        }
        for (expected, doc) in self.sources.iter().zip(documents) {
            let actual = doc.provenance();
            if actual.requested_source != expected.requested_source
                || actual.final_source != expected.final_source
                || actual.raw_sha256 != expected.raw_sha256
                || actual.text_sha256 != expected.text_sha256
                || actual.snapshot_id != expected.snapshot_id
            {
                return Err(error(
                    "benchmark source snapshot differs; no scoring admitted",
                ));
            }
        }
        for q in &self.queries {
            for g in &q.gold {
                let evidence = documents[g.source_index].read_span(g.start, g.end, 4096)?;
                if sha(evidence.text.as_bytes()) != g.quote_sha256 {
                    return Err(error("benchmark gold quote hash differs"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RankedSpan {
    pub source_index: usize,
    pub start: usize,
    pub end: usize,
}
#[derive(Debug, Serialize)]
pub struct RetrievalMetrics {
    pub k: usize,
    pub returned: usize,
    pub relevant_anchors: usize,
    pub found_anchors: usize,
    pub recall: Option<f64>,
    pub reciprocal_rank: Option<f64>,
    pub ndcg: Option<f64>,
    pub negative_returned_any: Option<bool>,
}
pub fn evaluate(
    gold: &[GoldAnchor],
    ranking: &[RankedSpan],
    k: usize,
) -> Result<RetrievalMetrics, Error> {
    if k == 0
        || k > 1000
        || ranking.len() > 20_000
        || gold.len() > 1
        || gold.iter().any(|g| g.start >= g.end)
    {
        return Err(error("invalid evaluation budget"));
    }
    let mut unique = BTreeSet::new();
    if ranking
        .iter()
        .any(|s| s.start >= s.end || !unique.insert((s.source_index, s.start, s.end)))
    {
        return Err(error("invalid/duplicate ranked span"));
    }
    let mut found = BTreeSet::new();
    let mut first = None;
    let mut dcg = 0.;
    for (rank, span) in ranking.iter().take(k).enumerate() {
        let mut gained = false;
        for (i, g) in gold.iter().enumerate() {
            if span.source_index == g.source_index
                && span.start <= g.start
                && span.end >= g.end
                && found.insert(i)
            {
                gained = true;
            }
        }
        if gained {
            first.get_or_insert(rank + 1);
            // One binary gain per returned window; newly covered extra anchors
            // still count toward recall. Pilot has one anchor per query.
            dcg += 1. / ((rank + 2) as f64).log2();
        }
    }
    let idcg: f64 = (0..k.min(gold.len()))
        .map(|rank| 1. / ((rank + 2) as f64).log2())
        .sum();
    Ok(RetrievalMetrics {
        k,
        returned: ranking.len().min(k),
        relevant_anchors: gold.len(),
        found_anchors: found.len(),
        recall: (!gold.is_empty()).then(|| found.len() as f64 / gold.len() as f64),
        reciprocal_rank: (!gold.is_empty()).then(|| first.map_or(0., |r| 1. / r as f64)),
        ndcg: (!gold.is_empty()).then(|| dcg / idcg),
        negative_returned_any: gold.is_empty().then_some(!ranking.is_empty()),
    })
}
