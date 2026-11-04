//! On-demand hybrid retrieval. No disk writes, model reloads, or hidden fallback.
use crate::document::{error, sha};
use crate::{
    cosine_similarity, E5Embedder, EmbeddingBatch, Error, Evidence, InputKind, LexicalIndex,
    ModelInfo, RankedHit, RequestControl,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCoverage {
    Full,
    LexicalPrefilter,
}

#[derive(Clone, Debug)]
pub struct HybridBudget {
    pub max_semantic_chunks: usize,
    pub max_model_tokens: usize,
    pub max_text_bytes: usize,
    pub max_vector_bytes: usize,
    pub max_rerank_candidates: usize,
    pub max_duration: Duration,
}
impl Default for HybridBudget {
    fn default() -> Self {
        Self {
            max_semantic_chunks: 128,
            max_model_tokens: 65_536,
            max_text_bytes: 512 * 1024,
            max_vector_bytes: 1024 * 1024,
            max_rerank_candidates: 16,
            max_duration: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HybridOptions {
    pub coverage: SemanticCoverage,
    pub top_k: usize,
    pub lexical_candidates: usize,
    pub semantic_candidates: usize,
    pub min_cosine: Option<f32>,
    pub rrf_k: u32,
    pub budget: HybridBudget,
}
impl Default for HybridOptions {
    fn default() -> Self {
        Self {
            coverage: SemanticCoverage::Full,
            top_k: 10,
            lexical_candidates: 64,
            semantic_candidates: 64,
            min_cosine: None,
            rrf_k: 60,
            budget: HybridBudget::default(),
        }
    }
}
impl HybridOptions {
    pub fn validate(&self) -> Result<(), Error> {
        let b = &self.budget;
        if !(1..=1000).contains(&self.top_k)
            || !(1..=1000).contains(&self.lexical_candidates)
            || !(1..=1000).contains(&self.semantic_candidates)
            || !(1..=1000).contains(&self.rrf_k)
            || self
                .min_cosine
                .is_some_and(|v| !v.is_finite() || !(-1.0..=1.0).contains(&v))
            || !(1..=4096).contains(&b.max_semantic_chunks)
            || !(1..=2_000_000).contains(&b.max_model_tokens)
            || !(1..=64 * 1024 * 1024).contains(&b.max_text_bytes)
            || !(1..=64 * 1024 * 1024).contains(&b.max_vector_bytes)
            || !(1..=128).contains(&b.max_rerank_candidates)
            || b.max_duration.is_zero()
            || b.max_duration > Duration::from_secs(300)
        {
            return Err(error("invalid hybrid options or budget"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct HybridHit {
    pub document_index: usize,
    pub chunk_index: usize,
    pub evidence: Evidence,
    pub rrf_score: f64,
    pub lexical_rank: Option<usize>,
    pub lexical_score: Option<f64>,
    pub semantic_rank: Option<usize>,
    pub cosine: Option<f32>,
    pub matched_terms: Vec<String>,
    pub rerank_score: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct Coverage {
    pub scope: String,
    pub policy: SemanticCoverage,
    pub indexed_chunks: usize,
    pub semantic_chunks: usize,
    pub omitted_chunks: usize,
    pub complete_index: bool,
    pub semantic_ranked_candidates: usize,
    pub empty_pdf_pages: usize,
    pub model_truncated: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct HybridCost {
    pub model_tokens: usize,
    pub passage_text_bytes: usize,
    /// Maximum FP32 output payload for query + one streamed batch;
    /// excludes runtime, hidden tensors, vector metadata and RSS.
    pub vector_bytes: usize,
    pub preflight_us: u128,
    pub tokenization_us: u128,
    pub inference_us: u128,
    pub pooling_us: u128,
    pub comparison_and_fusion_us: u128,
    pub rerank_us: u128,
    pub elapsed_ms: u128,
    pub model_batches: u64,
    pub native_inference_runs: u64,
    /// Planned budget tokens remain in model_tokens even on a cache hit.
    pub executed_model_tokens: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub reranked_candidates: usize,
}
impl HybridCost {
    fn charge(&mut self, batch: &EmbeddingBatch) {
        self.tokenization_us += batch.tokenization_us;
        self.inference_us += batch.inference_us;
        self.pooling_us += batch.pooling_us;
        self.executed_model_tokens += batch.executed_model_tokens;
        self.cache_hits += batch.cache_hits;
        self.cache_misses += batch.cache_misses;
    }
}

#[derive(Debug, Serialize)]
pub struct HybridResponse {
    pub schema_version: u32,
    pub query_sha256: String,
    pub hits: Vec<HybridHit>,
    pub coverage: Coverage,
    pub cost: HybridCost,
    pub model: ModelInfo,
    pub reranker: Option<String>,
    pub rrf_k: u32,
    pub min_cosine: Option<f32>,
}

/// A trusted local consumer extension. Its internals are not sandboxed here.
/// Return one finite score per input position; evidence cannot be replaced.
pub trait Reranker {
    fn name(&self) -> &str;
    fn rerank(&mut self, query: &str, candidates: &[HybridHit]) -> Result<Vec<f32>, Error>;
}

fn check_deadline(
    start: Instant,
    options: &HybridOptions,
    control: &RequestControl,
) -> Result<(), Error> {
    control.checkpoint()?;
    if start.elapsed() >= options.budget.max_duration {
        return Err(error("hybrid deadline exceeded; no partial result"));
    }
    Ok(())
}

fn verify_batch(batch: &EmbeddingBatch, counts: &[usize], dimensions: usize) -> Result<(), Error> {
    if batch.vectors.len() != counts.len()
        || batch.original_token_counts != counts
        || batch.used_token_counts != counts
        || batch.truncated.len() != counts.len()
        || batch.truncated.iter().any(|v| *v)
        || batch
            .vectors
            .iter()
            .any(|v| v.len() != dimensions || v.iter().any(|n| !n.is_finite()))
    {
        return Err(error(
            "model violated hybrid preflight; truncated or invalid vectors",
        ));
    }
    Ok(())
}

pub fn hybrid_search(
    index: &LexicalIndex,
    model: &mut E5Embedder,
    query: &str,
    options: &HybridOptions,
    reranker: Option<&mut dyn Reranker>,
) -> Result<HybridResponse, Error> {
    options.validate()?;
    let control = RequestControl::new(options.budget.max_duration)?;
    match hybrid_search_with_control(index, model, query, options, reranker, &control) {
        Ok(response) => control.commit(|| Ok(response)),
        Err(failure) => {
            control.finish();
            Err(failure)
        }
    }
}

/// Returns candidate evidence without committing the surrounding request.
/// The host must commit reference/session mutations using the same control.
pub fn hybrid_search_with_control(
    index: &LexicalIndex,
    model: &mut E5Embedder,
    query: &str,
    options: &HybridOptions,
    reranker: Option<&mut dyn Reranker>,
    control: &RequestControl,
) -> Result<HybridResponse, Error> {
    let started = Instant::now();
    options.validate()?;
    control.limit_deadline(started + options.budget.max_duration)?;
    check_deadline(started, options, control)?;
    let lexical = index.search(query, options.lexical_candidates)?;
    let selected: Vec<usize> = match options.coverage {
        SemanticCoverage::Full => {
            if index.stats().chunks > options.budget.max_semantic_chunks {
                return Err(error("full hybrid coverage exceeds semantic chunk budget; select prefilter explicitly or raise budget"));
            }
            (0..index.stats().chunks).collect()
        }
        SemanticCoverage::LexicalPrefilter => lexical
            .iter()
            .take(options.budget.max_semantic_chunks)
            .map(|h| h.chunk_index)
            .collect(),
    };
    let mut cost = HybridCost::default();
    for id in &selected {
        cost.passage_text_bytes += index
            .chunk_text_bytes(*id)
            .ok_or_else(|| error("unknown chunk"))?;
        if cost.passage_text_bytes > options.budget.max_text_bytes {
            return Err(error("hybrid passage text budget exceeded"));
        }
    }
    let chunks = selected
        .iter()
        .map(|id| {
            index
                .chunk_evidence(*id)
                .map(|(_, e)| e)
                .ok_or_else(|| error("unknown chunk"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let batches_before = model.info().completed_batches;
    let runs_before = model.info().inference_runs;
    let mut similarities = BTreeMap::new();
    if !chunks.is_empty() {
        cost.vector_bytes = (chunks.len().min(model.info().config.batch_size) + 1)
            * model.info().dimensions
            * std::mem::size_of::<f32>();
        if cost.vector_bytes > options.budget.max_vector_bytes {
            return Err(error("hybrid retained-vector budget exceeded"));
        }
        let preflight = Instant::now();
        let query_count = model.token_count(InputKind::Query, query)?;
        let mut counts = Vec::with_capacity(chunks.len());
        cost.model_tokens = query_count;
        for chunk in &chunks {
            check_deadline(started, options, control)?;
            let count = model.token_count(InputKind::Passage, &chunk.text)?;
            if count > model.info().config.max_tokens {
                return Err(error("passage exceeds model token window; reduce ChunkConfig, hybrid never silently truncates"));
            }
            cost.model_tokens += count;
            if cost.model_tokens > options.budget.max_model_tokens {
                return Err(error("hybrid model-token budget exceeded"));
            }
            counts.push(count);
        }
        if query_count > model.info().config.max_tokens {
            return Err(error("query exceeds model token window"));
        }
        cost.preflight_us = preflight.elapsed().as_micros();
        check_deadline(started, options, control)?;
        let mut query_batch =
            model.embed_with_control(InputKind::Query, &[query.into()], control)?;
        verify_batch(&query_batch, &[query_count], model.info().dimensions)?;
        cost.charge(&query_batch);
        let query_vector = query_batch
            .vectors
            .pop()
            .ok_or_else(|| error("missing query vector"))?;
        let batch_size = model.info().config.batch_size;
        for (group_index, group) in chunks.chunks(batch_size).enumerate() {
            check_deadline(started, options, control)?;
            let texts = group.iter().map(|c| c.text.clone()).collect::<Vec<_>>();
            let embedded = model.embed_with_control(InputKind::Passage, &texts, control)?;
            let base = group_index * batch_size;
            verify_batch(
                &embedded,
                &counts[base..base + group.len()],
                model.info().dimensions,
            )?;
            cost.charge(&embedded);
            check_deadline(started, options, control)?;
            let comparison = Instant::now();
            for (offset, vector) in embedded.vectors.iter().enumerate() {
                similarities.insert(
                    selected[base + offset],
                    cosine_similarity(&query_vector, vector)?,
                );
            }
            cost.comparison_and_fusion_us += comparison.elapsed().as_micros();
        }
    }
    check_deadline(started, options, control)?;
    let fusion = Instant::now();
    let mut dense: Vec<_> = similarities
        .iter()
        .map(|(&id, &score)| (id, score))
        .filter(|(_, score)| options.min_cosine.is_none_or(|min| *score >= min))
        .collect();
    dense.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then(a.cmp(b)));
    dense.truncate(options.semantic_candidates);
    let semantic_ranked_candidates = dense.len();
    let mut hits = fuse(index, lexical, &dense, &similarities, options.rrf_k)?;
    cost.comparison_and_fusion_us += fusion.elapsed().as_micros();
    check_deadline(started, options, control)?;
    let (reranker, reranked) = apply_reranker(
        query,
        &mut hits,
        reranker,
        options.budget.max_rerank_candidates,
    )?;
    cost.rerank_us = reranked.0;
    cost.reranked_candidates = reranked.1;
    check_deadline(started, options, control)?;
    hits.truncate(options.top_k);
    cost.elapsed_ms = started.elapsed().as_millis();
    cost.model_batches = model.info().completed_batches - batches_before;
    cost.native_inference_runs = model.info().inference_runs - runs_before;
    let total = index.stats().chunks;
    Ok(HybridResponse {
        schema_version: 1,
        query_sha256: sha(query.as_bytes()),
        hits,
        coverage: Coverage {
            scope: "indexed_token_windows_not_raw_source_or_legal_validity".into(),
            policy: options.coverage,
            indexed_chunks: total,
            semantic_chunks: selected.len(),
            omitted_chunks: total - selected.len(),
            complete_index: selected.len() == total,
            semantic_ranked_candidates,
            empty_pdf_pages: (0..index.stats().documents)
                .filter_map(|i| index.document(i))
                .flat_map(|d| d.pages())
                .filter(|p| !p.has_text)
                .count(),
            model_truncated: false,
        },
        cost,
        model: model.info().clone(),
        reranker,
        rrf_k: options.rrf_k,
        min_cosine: options.min_cosine,
    })
}

fn fuse(
    index: &LexicalIndex,
    lexical: Vec<RankedHit>,
    dense: &[(usize, f32)],
    all_similarities: &BTreeMap<usize, f32>,
    k: u32,
) -> Result<Vec<HybridHit>, Error> {
    let mut fused = BTreeMap::new();
    for (position, hit) in lexical.into_iter().enumerate() {
        let rank = position + 1;
        fused.insert(
            hit.chunk_index,
            HybridHit {
                document_index: hit.document_index,
                chunk_index: hit.chunk_index,
                evidence: hit.evidence,
                rrf_score: 1.0 / (f64::from(k) + rank as f64),
                lexical_rank: Some(rank),
                lexical_score: Some(hit.score),
                semantic_rank: None,
                cosine: all_similarities.get(&hit.chunk_index).copied(),
                matched_terms: hit.matched_terms,
                rerank_score: None,
            },
        );
    }
    for (position, (id, cosine)) in dense.iter().enumerate() {
        let rank = position + 1;
        let (document_index, evidence) = index
            .chunk_evidence(*id)
            .ok_or_else(|| error("invalid fusion chunk"))?;
        let item = fused.entry(*id).or_insert(HybridHit {
            document_index,
            chunk_index: *id,
            evidence,
            rrf_score: 0.0,
            lexical_rank: None,
            lexical_score: None,
            semantic_rank: None,
            cosine: Some(*cosine),
            matched_terms: Vec::new(),
            rerank_score: None,
        });
        item.semantic_rank = Some(rank);
        item.rrf_score += 1.0 / (f64::from(k) + rank as f64);
    }
    let mut hits: Vec<_> = fused.into_values().collect();
    hits.sort_by(|a, b| {
        b.rrf_score
            .total_cmp(&a.rrf_score)
            .then(a.chunk_index.cmp(&b.chunk_index))
    });
    Ok(hits)
}

fn apply_reranker(
    query: &str,
    hits: &mut [HybridHit],
    reranker: Option<&mut dyn Reranker>,
    max: usize,
) -> Result<(Option<String>, (u128, usize)), Error> {
    let Some(reranker) = reranker else {
        return Ok((None, (0, 0)));
    };
    if reranker.name().is_empty() || reranker.name().len() > 128 {
        return Err(error("invalid reranker identity"));
    }
    let name = reranker.name().to_string();
    let count = hits.len().min(max);
    if count == 0 {
        return Ok((Some(name), (0, 0)));
    }
    let started = Instant::now();
    let scores = reranker
        .rerank(query, &hits[..count])
        .map_err(|_| error("local reranker failed; no partial result"))?;
    if scores.len() != count || scores.iter().any(|score| !score.is_finite()) {
        return Err(error(
            "reranker returned invalid score count or non-finite scores",
        ));
    }
    for (hit, score) in hits.iter_mut().take(count).zip(scores) {
        hit.rerank_score = Some(score);
    }
    hits[..count].sort_by(|a, b| {
        b.rerank_score
            .unwrap_or(f32::NEG_INFINITY)
            .total_cmp(&a.rerank_score.unwrap_or(f32::NEG_INFINITY))
            .then(b.rrf_score.total_cmp(&a.rrf_score))
            .then(a.chunk_index.cmp(&b.chunk_index))
    });
    Ok((Some(name), (started.elapsed().as_micros(), count)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkConfig, Document, IndexLimits, Limits};
    use std::sync::Arc;
    fn index() -> LexicalIndex {
        let docs = (0..3)
            .map(|i| {
                Arc::new(
                    Document::from_bytes(
                        &format!("fixture-{i}"),
                        b"alpha nao dispensa prova artigo 42",
                        "text/plain",
                        &Limits::default(),
                    )
                    .unwrap(),
                )
            })
            .collect();
        LexicalIndex::build(docs, ChunkConfig::default(), IndexLimits::default()).unwrap()
    }
    #[test]
    fn rrf_uses_ranks_and_preserves_evidence() {
        let index = index();
        let lexical = index.search("alpha", 2).unwrap();
        let dense = [(1, 0.5), (2, 0.4)]; // Algorithm fixture, not model inference.
        let fused = fuse(&index, lexical, &dense, &dense.into_iter().collect(), 60).unwrap();
        assert_eq!(
            fused.iter().map(|h| h.chunk_index).collect::<Vec<_>>(),
            vec![1, 0, 2]
        );
        assert!((fused[0].rrf_score - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
        assert!(fused[0]
            .evidence
            .text
            .contains("nao dispensa prova artigo 42"));
        let tied = fuse(
            &index,
            index.search("alpha", 1).unwrap(),
            &[(1, 0.9)],
            &BTreeMap::new(),
            60,
        )
        .unwrap();
        assert_eq!(tied[0].chunk_index, 0);
    }
    struct FixtureReranker {
        mode: u8,
    }
    impl Reranker for FixtureReranker {
        fn name(&self) -> &str {
            "algorithm-fixture-not-a-model"
        }
        fn rerank(&mut self, _: &str, candidates: &[HybridHit]) -> Result<Vec<f32>, Error> {
            match self.mode {
                0 => Ok((0..candidates.len()).map(|i| i as f32).collect()),
                1 => Ok(vec![]),
                2 => Ok(vec![f32::NAN; candidates.len()]),
                _ => Err(error("fixture failure")),
            }
        }
    }
    #[test]
    fn optional_reranker_is_bounded_and_cannot_replace_evidence() {
        let index = index();
        let mut hits = fuse(
            &index,
            index.search("alpha", 3).unwrap(),
            &[],
            &BTreeMap::new(),
            60,
        )
        .unwrap();
        let first = hits[0].evidence.clone();
        let (_, (_, count)) =
            apply_reranker("q", &mut hits, Some(&mut FixtureReranker { mode: 0 }), 2).unwrap();
        assert_eq!(count, 2);
        assert_eq!(hits[0].chunk_index, 1);
        assert_eq!(hits[1].evidence.text, first.text);
        assert_eq!(hits[1].evidence.snapshot_id, first.snapshot_id);
        assert!(hits[2].rerank_score.is_none());
        for mode in [1, 2, 3] {
            assert!(
                apply_reranker("q", &mut hits, Some(&mut FixtureReranker { mode }), 2).is_err()
            );
        }
    }
}
