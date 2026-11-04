//! Ephemeral, deterministic BM25 over original-text token windows.
use crate::document::{error, Error};
use crate::{Document, Evidence};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    sync::Arc,
};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

const K1: f64 = 1.2;
const B: f64 = 0.75;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ChunkConfig {
    pub tokens_per_chunk: usize,
    pub overlap_tokens: usize,
}
impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            tokens_per_chunk: 192,
            overlap_tokens: 32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IndexLimits {
    pub max_documents: usize,
    pub max_chunks: usize,
    pub max_tokens: usize,
    pub max_accounted_bytes: usize,
    pub max_token_chars: usize,
}
impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_documents: 64,
            max_chunks: 20_000,
            max_tokens: 2_000_000,
            max_accounted_bytes: 128 * 1024 * 1024,
            max_token_chars: 1024,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct IndexStats {
    pub documents: usize,
    pub chunks: usize,
    /// Original tokens, before window overlap.
    pub tokens: usize,
    pub indexed_tokens: usize,
    pub unique_terms: usize,
    pub postings: usize,
    /// Conservative accounting units, NOT allocator usage or RSS.
    pub accounted_bytes: usize,
    pub peak_accounted_bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RankedHit {
    pub document_index: usize,
    pub chunk_index: usize,
    pub score: f64,
    pub matched_terms: Vec<String>,
    pub evidence: Evidence,
}

struct Token {
    term: String,
    start: usize,
    end: usize,
    byte_start: usize,
    byte_end: usize,
}
struct Chunk {
    document: usize,
    start: usize,
    end: usize,
    byte_start: usize,
    byte_end: usize,
    tokens: usize,
}
struct Posting {
    chunk: usize,
    count: usize,
}

pub struct LexicalIndex {
    documents: Vec<Arc<Document>>,
    chunks: Vec<Chunk>,
    terms: BTreeMap<String, Vec<Posting>>,
    stats: IndexStats,
    config: ChunkConfig,
}

/// Tokenize original spans first; normalization must never change their offsets.
fn tokenize(
    text: &str,
    max_tokens: usize,
    max_chars: usize,
    budget: usize,
) -> Result<(Vec<Token>, usize), Error> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut accounted = 0;
    for (scalar_count, (byte, ch)) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
        .enumerate()
    {
        let part = ch.is_alphanumeric() || (start.is_some() && is_combining_mark(ch));
        if part {
            let (start_scalar, _) = *start.get_or_insert((scalar_count, byte));
            if scalar_count - start_scalar >= max_chars {
                return Err(error("token character budget exceeded"));
            }
        } else if let Some((first, first_byte)) = start.take() {
            if tokens.len() >= max_tokens {
                return Err(error("index token budget exceeded"));
            }
            let term: String = text[first_byte..byte]
                .nfkd()
                .filter(|c| !is_combining_mark(*c))
                .flat_map(char::to_lowercase)
                .collect();
            accounted += size_of::<Token>() + term.len() * 2 + 32;
            if accounted > budget {
                return Err(error(
                    "index accounting budget exceeded during tokenization",
                ));
            }
            tokens.push(Token {
                term,
                start: first,
                end: scalar_count,
                byte_start: first_byte,
                byte_end: byte,
            });
        }
    }
    Ok((tokens, accounted))
}

impl LexicalIndex {
    pub fn build(
        documents: Vec<Arc<Document>>,
        config: ChunkConfig,
        limits: IndexLimits,
    ) -> Result<Self, Error> {
        if config.tokens_per_chunk == 0
            || config.tokens_per_chunk > 4096
            || config.overlap_tokens >= config.tokens_per_chunk
        {
            return Err(error("invalid chunk configuration"));
        }
        if limits.max_documents == 0
            || limits.max_documents > 1024
            || limits.max_chunks == 0
            || limits.max_chunks > 200_000
            || limits.max_tokens == 0
            || limits.max_tokens > 16_000_000
            || limits.max_accounted_bytes == 0
            || limits.max_accounted_bytes > 1024 * 1024 * 1024
            || limits.max_token_chars == 0
            || limits.max_token_chars > 4096
        {
            return Err(error("invalid index limits"));
        }
        if documents.len() > limits.max_documents {
            return Err(error("index document budget exceeded"));
        }
        let mut identities = BTreeSet::new();
        if documents
            .iter()
            .any(|d| !identities.insert(&d.provenance().snapshot_id))
        {
            return Err(error("duplicate document snapshot"));
        }
        let mut stats = IndexStats {
            documents: documents.len(),
            ..IndexStats::default()
        };
        // Account retained source text as well, although Arc avoids duplicating it.
        stats.accounted_bytes = documents.iter().map(|d| d.text().len() + 256).sum();
        if stats.accounted_bytes > limits.max_accounted_bytes {
            return Err(error("index source accounting budget exceeded"));
        }
        let mut chunks = Vec::new();
        let mut terms: BTreeMap<String, Vec<Posting>> = BTreeMap::new();
        for (document, doc) in documents.iter().enumerate() {
            let (tokens, temporary) = tokenize(
                doc.text(),
                limits.max_tokens.saturating_sub(stats.tokens),
                limits.max_token_chars,
                limits
                    .max_accounted_bytes
                    .saturating_sub(stats.accounted_bytes),
            )?;
            stats.tokens += tokens.len();
            stats.peak_accounted_bytes = stats
                .peak_accounted_bytes
                .max(stats.accounted_bytes + temporary);
            let mut first = 0;
            while first < tokens.len() {
                if chunks.len() >= limits.max_chunks {
                    return Err(error("index chunk budget exceeded"));
                }
                let end = (first + config.tokens_per_chunk).min(tokens.len());
                let window = &tokens[first..end];
                let id = chunks.len();
                chunks.push(Chunk {
                    document,
                    start: window[0].start,
                    end: window[window.len() - 1].end,
                    byte_start: window[0].byte_start,
                    byte_end: window[window.len() - 1].byte_end,
                    tokens: window.len(),
                });
                stats.accounted_bytes += size_of::<Chunk>() * 2;
                stats.indexed_tokens += window.len();
                let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
                for token in window {
                    *counts.entry(&token.term).or_default() += 1;
                }
                for (term, count) in counts {
                    if !terms.contains_key(term) {
                        stats.accounted_bytes += 160 + term.len() * 2;
                    }
                    terms
                        .entry(term.into())
                        .or_default()
                        .push(Posting { chunk: id, count });
                    stats.accounted_bytes += size_of::<Posting>() * 2;
                    stats.postings += 1;
                }
                stats.peak_accounted_bytes = stats
                    .peak_accounted_bytes
                    .max(stats.accounted_bytes + temporary);
                if stats.peak_accounted_bytes > limits.max_accounted_bytes {
                    return Err(error("index accounting budget exceeded"));
                }
                if end == tokens.len() {
                    break;
                }
                first += config.tokens_per_chunk - config.overlap_tokens;
            }
        }
        stats.chunks = chunks.len();
        stats.unique_terms = terms.len();
        Ok(Self {
            documents,
            chunks,
            terms,
            stats,
            config,
        })
    }
    pub fn stats(&self) -> &IndexStats {
        &self.stats
    }
    pub fn config(&self) -> ChunkConfig {
        self.config
    }
    pub fn document(&self, index: usize) -> Option<&Arc<Document>> {
        self.documents.get(index)
    }

    pub fn chunk_text_bytes(&self, index: usize) -> Option<usize> {
        self.chunks.get(index).map(|c| c.byte_end - c.byte_start)
    }

    /// Original-text evidence and owning document position, without ranking.
    pub fn chunk_evidence(&self, index: usize) -> Option<(usize, Evidence)> {
        let chunk = self.chunks.get(index)?;
        let doc = &self.documents[chunk.document];
        Some((
            chunk.document,
            doc.evidence(
                chunk.start,
                chunk.end,
                doc.text()[chunk.byte_start..chunk.byte_end].into(),
            ),
        ))
    }

    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<RankedHit>, Error> {
        if top_k == 0 || top_k > 1000 {
            return Err(error("top_k must be 1..=1000"));
        }
        if query.len() > 4096 {
            return Err(error("query byte budget exceeded"));
        }
        let (tokens, _) = tokenize(query, 128, 1024, 64 * 1024)?;
        let query_terms: BTreeSet<_> = tokens.into_iter().map(|t| t.term).collect();
        if query_terms.is_empty() {
            return Err(error("query has no lexical tokens"));
        }
        if self.chunks.is_empty() {
            return Ok(Vec::new());
        }
        let n = self.chunks.len() as f64;
        let average = self.stats.indexed_tokens as f64 / n;
        let mut scores = vec![0.0; self.chunks.len()];
        for term in &query_terms {
            if let Some(postings) = self.terms.get(term) {
                let df = postings.len() as f64;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                for posting in postings {
                    let tf = posting.count as f64;
                    let length = self.chunks[posting.chunk].tokens as f64;
                    scores[posting.chunk] +=
                        idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length / average));
                }
            }
        }
        let mut candidates: Vec<_> = scores
            .into_iter()
            .enumerate()
            .filter(|(_, s)| *s > 0.0)
            .collect();
        candidates.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then(a.cmp(b)));
        Ok(candidates
            .into_iter()
            .take(top_k)
            .map(|(id, score)| {
                let chunk = &self.chunks[id];
                let doc = &self.documents[chunk.document];
                let matched_terms = query_terms
                    .iter()
                    .filter(|t| {
                        self.terms.get(*t).is_some_and(|postings| {
                            postings.binary_search_by_key(&id, |p| p.chunk).is_ok()
                        })
                    })
                    .cloned()
                    .collect();
                RankedHit {
                    document_index: chunk.document,
                    chunk_index: id,
                    score,
                    matched_terms,
                    evidence: doc.evidence(
                        chunk.start,
                        chunk.end,
                        doc.text()[chunk.byte_start..chunk.byte_end].into(),
                    ),
                }
            })
            .collect())
    }
}
