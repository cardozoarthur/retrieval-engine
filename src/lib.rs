//! Independent retrieval primitives. Offsets are Unicode scalar indices.
mod acquisition;
pub mod benchmark;
mod control;
mod dispatcher;
pub use control::{CancelResult, RequestControl};
pub use dispatcher::{serve_jsonl, JsonlConfig};
mod document;
mod evidence;
pub mod evidence_store;
#[cfg(windows)]
pub mod foundry_adapter;
#[cfg(feature = "cpu-onnx")]
mod hybrid;
mod incremental;
#[cfg(windows)]
mod isolation;
mod lexical;
#[cfg(windows)]
pub mod process_client;
mod vector_formats;
pub use incremental::{OpenTaskConfig, OpenTaskController};
pub use vector_formats::{StoredVector, VectorFormat};
#[cfg(windows)]
mod native_job;
#[cfg(windows)]
pub use isolation::{
    run_extraction_helper, AcquiredSource, ExtractionEvent, ExtractionTask, HelperFailure,
    IsolationConfig, IsolationReport,
};
#[cfg(feature = "cpu-onnx")]
pub mod benchmark_model;
#[cfg(feature = "cpu-onnx")]
mod semantic;
#[cfg(feature = "cpu-onnx")]
mod vector_cache;
#[cfg(feature = "cpu-onnx")]
pub use vector_cache::CacheStats;
mod worker;
pub use document::{
    Document, Error, Evidence, Format, Limits, LoadTimings, PageObserver, PageRange, Provenance,
    Source,
};
pub use evidence::{
    EvidenceLimits, EvidencePack, EvidenceSession, EvidenceView, Expansion, PackPayload,
    ReferenceRecord,
};
#[cfg(feature = "cpu-onnx")]
pub use hybrid::{
    hybrid_search, hybrid_search_with_control, Coverage, HybridBudget, HybridCost, HybridHit,
    HybridOptions, HybridResponse, Reranker, SemanticCoverage,
};
pub use lexical::{ChunkConfig, IndexLimits, IndexStats, LexicalIndex, RankedHit};
#[cfg(feature = "cpu-onnx")]
pub use semantic::{
    cosine_similarity, CpuConfig, E5Embedder, EmbeddingBatch, InputKind, ModelInfo,
};
pub use worker::MAX_REQUEST_BYTES as MAX_RETRIEVAL_REQUEST_BYTES;
pub use worker::{
    parse_retrieval_request, Operation, RetrievalWorker, SearchMode, SourceSpec, WorkerConfig,
    WorkerFault, WorkerRequest, WorkerResponse,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Characters,
    Words,
}

pub struct Snapshot {
    source: String,
    text: String,
}
#[derive(Debug)]
pub struct Context<'a> {
    pub source: &'a str,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Snapshot {
    pub fn new(source: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            text: text.into(),
        }
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn find(&self, query: &str) -> Result<Vec<(usize, usize)>, &'static str> {
        if query.is_empty() {
            return Err("empty query");
        }
        Ok(self
            .text
            .match_indices(query)
            .map(|(byte, matched)| {
                let start = self.text[..byte].chars().count();
                (start, start + matched.chars().count())
            })
            .collect())
    }
    pub fn expand(
        &self,
        hit: (usize, usize),
        before: usize,
        after: usize,
        unit: Unit,
        max_chars: usize,
    ) -> Result<Context<'_>, &'static str> {
        expand_text(
            &self.source,
            &self.text,
            hit,
            before,
            after,
            unit,
            max_chars,
        )
    }
}

pub(crate) fn expand_text<'a>(
    source: &'a str,
    text: &str,
    hit: (usize, usize),
    before: usize,
    after: usize,
    unit: Unit,
    max_chars: usize,
) -> Result<Context<'a>, &'static str> {
    let chars: Vec<char> = text.chars().collect();
    let (mut start, mut end) = hit;
    if start >= end || end > chars.len() {
        return Err("invalid span");
    }
    match unit {
        Unit::Characters => {
            start = start.saturating_sub(before);
            end = end.saturating_add(after).min(chars.len());
        }
        Unit::Words => {
            // Finish any word containing the hit boundary, then add words.
            while start > 0 && !chars[start - 1].is_whitespace() {
                start -= 1;
            }
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            for _ in 0..before.min(chars.len()) {
                while start > 0 && chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                if start == 0 {
                    break;
                }
            }
            for _ in 0..after.min(chars.len()) {
                while end < chars.len() && chars[end].is_whitespace() {
                    end += 1;
                }
                while end < chars.len() && !chars[end].is_whitespace() {
                    end += 1;
                }
                if end == chars.len() {
                    break;
                }
            }
        }
    }
    if end - start > max_chars {
        return Err("context budget exceeded");
    }
    Ok(Context {
        source,
        start,
        end,
        text: chars[start..end].iter().collect(),
    })
}
