//! Pinned E5 CPU inference. Optional; lexical use never loads a native runtime.
use crate::document::{error, Error};
use crate::vector_cache::{CacheStats, CachedVector, VectorCache};
use crate::RequestControl;
use ort::{
    execution_providers::CPUExecutionProvider,
    session::{builder::GraphOptimizationLevel, run_options::RunOptions, Session},
    value::Tensor,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokenizers::Tokenizer;

const MODEL_SHA: &str = "4d24e2bc01a447951524466ef533e52944bf48509e6552810bcee1a2711cb02c";
const TOKENIZER_SHA: &str = "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39";
const RUNTIME_SHA: &str = "579b636403983254346a5c1d80bd28f1519cd1e284cd204f8d4ff41f8d711559";
const SHARED_SHA: &str = "ba00ea1ef846c9b909c7854bc56c51051a20f9773b3e1153dda118d4b85d0b93";
const REVISION: &str = "761b726dd34fb83930e26aab4e9ac3899aa1fa78";
const DIMENSIONS: usize = 384;
static RUNTIME_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CpuConfig {
    pub threads: usize,
    pub batch_size: usize,
    pub max_tokens: usize,
    pub allow_truncation: bool,
    /// Opt-in tensor batching: dynamic Int8 scales may depend on other rows.
    pub packed_batching: bool,
    /// Accounted retained cache budget, not an RSS cap. Zero disables caching.
    pub cache_max_bytes: usize,
    /// Absolute vector lifetime; reads never extend it. Zero disables caching.
    pub cache_ttl_ms: u64,
}
impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            threads: 2,
            batch_size: 4,
            max_tokens: 512,
            allow_truncation: false,
            packed_batching: false,
            cache_max_bytes: 16 * 1024 * 1024,
            cache_ttl_ms: 300_000,
        }
    }
}
impl CpuConfig {
    fn validate(self) -> Result<(), Error> {
        if !(1..=8).contains(&self.threads)
            || !(1..=8).contains(&self.batch_size)
            || !(8..=512).contains(&self.max_tokens)
            || self.cache_max_bytes > 64 * 1024 * 1024
            || self.cache_ttl_ms > 3_600_000
        {
            return Err(error(
                "invalid CPU config: threads/batch 1..8, tokens 8..512, cache <=64MiB/1h",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Query,
    Passage,
}
impl InputKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Query => "query: ",
            Self::Passage => "passage: ",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct EmbeddingBatch {
    pub vectors: Vec<Vec<f32>>,
    pub original_token_counts: Vec<usize>,
    pub used_token_counts: Vec<usize>,
    pub truncated: Vec<bool>,
    pub tokenization_us: u128,
    pub inference_us: u128,
    pub pooling_us: u128,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub executed_model_tokens: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelInfo {
    pub model: String,
    pub conversion_revision: String,
    pub model_sha256: String,
    pub runtime_version: String,
    pub provider: String,
    pub dimensions: usize,
    pub weight_format: String,
    pub vector_format: String,
    pub config: CpuConfig,
    pub load_ms: u128,
    pub completed_batches: u64,
    pub inference_runs: u64,
    pub inference_attempts: u64,
    /// ORT returned an error while this request was cancelled/expired.
    pub interrupted_runs: u64,
    pub cache: CacheStats,
}

#[derive(Deserialize)]
struct Resource {
    sha256: String,
    bytes: u64,
}
#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    model: String,
    revision: String,
    runtime_version: String,
    weight_format: String,
    dimensions: usize,
    files: BTreeMap<String, Resource>,
}

pub struct E5Embedder {
    session: Session,
    tokenizer: Tokenizer,
    info: ModelInfo,
    cache: VectorCache,
}

pub(crate) fn hash_file(path: &Path, expected: &str, bytes: u64) -> Result<(), Error> {
    let mut file =
        File::open(path).map_err(|_| error("required embedding resource unavailable"))?;
    if file.metadata().map_err(error)?.len() != bytes {
        return Err(error("embedding resource size mismatch"));
    }
    let mut digest = Sha256::new();
    let mut block = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut block).map_err(error)?;
        if n == 0 {
            break;
        }
        digest.update(&block[..n]);
    }
    if format!("{:x}", digest.finalize()) != expected {
        return Err(error("embedding resource SHA256 mismatch"));
    }
    Ok(())
}

// Shared by the production E5 loader and the opt-in benchmark driver. Each caller
// verifies the pinned runtime files before this process-wide initialization.
pub(crate) fn initialize_runtime(root: &Path) -> Result<(), Error> {
    let runtime = root.join("onnxruntime.dll");
    {
        let mut initialized = RUNTIME_PATH
            .lock()
            .map_err(|_| error("runtime initialization poisoned"))?;
        if let Some(path) = initialized.as_ref() {
            if path != &runtime {
                return Err(error(
                    "process already bound to a different ONNX runtime path",
                ));
            }
        } else {
            let committed = ort::init_from(runtime.to_string_lossy())
                .with_name("retrieval-engine-cpu")
                .commit()
                .map_err(error)?;
            if !committed {
                return Err(error(
                    "ORT environment was already initialized outside this engine",
                ));
            }
            *initialized = Some(runtime);
        }
    }
    Ok(())
}

impl E5Embedder {
    pub fn load(model_dir: impl AsRef<Path>, config: CpuConfig) -> Result<Self, Error> {
        config.validate()?;
        let started = Instant::now();
        let root = model_dir
            .as_ref()
            .canonicalize()
            .map_err(|_| error("embedding resource directory unavailable"))?;
        let mut manifest_bytes = Vec::new();
        File::open(root.join("manifest.json"))
            .map_err(error)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut manifest_bytes)
            .map_err(error)?;
        if manifest_bytes.len() > 64 * 1024 {
            return Err(error("embedding manifest too large"));
        }
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| error("invalid embedding manifest"))?;
        if manifest.schema_version != 1
            || manifest.model != "intfloat/multilingual-e5-small"
            || manifest.revision != REVISION
            || manifest.runtime_version != "1.22.0"
            || manifest.weight_format != "int8"
            || manifest.dimensions != DIMENSIONS
        {
            return Err(error("unsupported embedding manifest contract"));
        }
        for (name, expected, size) in [
            ("model.onnx", MODEL_SHA, 118054593),
            ("tokenizer.json", TOKENIZER_SHA, 17082730),
            ("onnxruntime.dll", RUNTIME_SHA, 12418080),
            ("onnxruntime_providers_shared.dll", SHARED_SHA, 22064),
        ] {
            let resource = manifest
                .files
                .get(name)
                .ok_or_else(|| error("embedding resource missing from manifest"))?;
            if resource.sha256 != expected || resource.bytes != size {
                return Err(error("unapproved embedding resource identity"));
            }
            hash_file(&root.join(name), expected, size)?;
        }
        initialize_runtime(&root)?;
        let mut tokenizer = Tokenizer::from_file(root.join("tokenizer.json"))
            .map_err(|_| error("tokenizer load failed"))?;
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(None)
            .map_err(|_| error("tokenizer configuration failed"))?;
        let session = Session::builder()
            .map_err(error)?
            .with_execution_providers([CPUExecutionProvider::default().build()])
            .map_err(error)?
            .with_intra_threads(config.threads)
            .map_err(error)?
            .with_inter_threads(1)
            .map_err(error)?
            .with_parallel_execution(false)
            .map_err(error)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(error)?
            .commit_from_file(root.join("model.onnx"))
            .map_err(error)?;
        let input_names: BTreeMap<_, _> = session
            .inputs
            .iter()
            .map(|i| (i.name.as_str(), ()))
            .collect();
        if !input_names.contains_key("input_ids")
            || !input_names.contains_key("attention_mask")
            || input_names
                .keys()
                .any(|x| !matches!(*x, "input_ids" | "attention_mask" | "token_type_ids"))
            || session.outputs.len() != 1
        {
            return Err(error("unexpected ONNX graph input/output contract"));
        }
        let cache = VectorCache::new(
            if config.packed_batching {
                0
            } else {
                config.cache_max_bytes
            },
            Duration::from_millis(config.cache_ttl_ms),
        );
        let cache_stats = cache.stats();
        Ok(Self {
            session,
            tokenizer,
            cache,
            info: ModelInfo {
                model: manifest.model,
                conversion_revision: manifest.revision,
                model_sha256: MODEL_SHA.into(),
                runtime_version: manifest.runtime_version,
                provider: "CPUExecutionProvider".into(),
                dimensions: DIMENSIONS,
                weight_format: "int8".into(),
                vector_format: "normalized_fp32".into(),
                config,
                load_ms: started.elapsed().as_millis(),
                completed_batches: 0,
                inference_runs: 0,
                inference_attempts: 0,
                interrupted_runs: 0,
                cache: cache_stats,
            },
        })
    }
    pub fn info(&self) -> &ModelInfo {
        &self.info
    }
    /// Allows a resident host to release expired entries while idle.
    pub fn maintenance(&mut self) {
        self.cache.purge(Instant::now());
        self.info.cache = self.cache.stats();
    }
    /// Counts real model tokens, including role prefix and special tokens; no inference.
    pub fn token_count(&self, kind: InputKind, text: &str) -> Result<usize, Error> {
        if text.trim().is_empty() || text.len() > 16 * 1024 {
            return Err(error("embedding input empty or exceeds 16KiB"));
        }
        self.tokenizer
            .encode(format!("{}{text}", kind.prefix()), true)
            .map(|encoded| encoded.len())
            .map_err(|_| error("tokenization failed"))
    }
    pub fn embed(&mut self, kind: InputKind, texts: &[String]) -> Result<EmbeddingBatch, Error> {
        self.embed_inner(kind, texts, None)
    }
    /// Does not commit/finish the request: the consumer must still admit evidence atomically.
    pub fn embed_with_control(
        &mut self,
        kind: InputKind,
        texts: &[String],
        control: &RequestControl,
    ) -> Result<EmbeddingBatch, Error> {
        self.embed_inner(kind, texts, Some(control))
    }
    fn embed_inner(
        &mut self,
        kind: InputKind,
        texts: &[String],
        control: Option<&RequestControl>,
    ) -> Result<EmbeddingBatch, Error> {
        if let Some(control) = control {
            control.checkpoint()?;
        }
        if texts.is_empty() || texts.len() > self.info.config.batch_size {
            return Err(error("embedding batch budget exceeded or empty"));
        }
        if texts
            .iter()
            .any(|t| t.trim().is_empty() || t.len() > 16 * 1024)
        {
            return Err(error("embedding input empty or exceeds 16KiB"));
        }
        let result = if !self.info.config.packed_batching {
            // DynamicQuantizeLinear scales an entire activation tensor. Packed
            // rows can therefore alter another row's representation. Stable mode
            // keeps model/session resident but executes each text independently.
            let mut combined = EmbeddingBatch {
                vectors: Vec::new(),
                original_token_counts: Vec::new(),
                used_token_counts: Vec::new(),
                truncated: Vec::new(),
                tokenization_us: 0,
                inference_us: 0,
                pooling_us: 0,
                cache_hits: 0,
                cache_misses: 0,
                executed_model_tokens: 0,
            };
            for text in texts {
                if let Some(control) = control {
                    control.checkpoint()?;
                }
                let key = cache_key(self.info.config, kind, text);
                let cached = self.cache.get(&key, Instant::now());
                self.info.cache = self.cache.stats();
                if let Some(value) = cached {
                    combined.vectors.push(value.vector.to_vec());
                    combined.original_token_counts.push(value.original);
                    combined.used_token_counts.push(value.used);
                    combined.truncated.push(value.truncated);
                    combined.cache_hits += 1;
                    continue;
                }
                let mut part = self.embed_tensor(kind, std::slice::from_ref(text), control)?;
                if let Some(control) = control {
                    control.checkpoint()?;
                }
                self.cache.insert(
                    key,
                    CachedVector {
                        vector: part.vectors[0]
                            .as_slice()
                            .try_into()
                            .map_err(|_| error("invalid cache vector shape"))?,
                        original: part.original_token_counts[0],
                        used: part.used_token_counts[0],
                        truncated: part.truncated[0],
                    },
                    Instant::now(),
                );
                self.info.cache = self.cache.stats();
                combined.cache_misses += usize::from(self.info.cache.enabled);
                combined.executed_model_tokens += part.executed_model_tokens;
                combined.vectors.append(&mut part.vectors);
                combined
                    .original_token_counts
                    .append(&mut part.original_token_counts);
                combined
                    .used_token_counts
                    .append(&mut part.used_token_counts);
                combined.truncated.append(&mut part.truncated);
                combined.tokenization_us += part.tokenization_us;
                combined.inference_us += part.inference_us;
                combined.pooling_us += part.pooling_us;
            }
            combined
        } else {
            self.embed_tensor(kind, texts, control)?
        };
        if let Some(control) = control {
            control.checkpoint()?;
        }
        self.info.completed_batches += 1;
        Ok(result)
    }

    fn embed_tensor(
        &mut self,
        kind: InputKind,
        texts: &[String],
        control: Option<&RequestControl>,
    ) -> Result<EmbeddingBatch, Error> {
        let started = Instant::now();
        let mut ids = Vec::new();
        let mut original = Vec::new();
        let mut used = Vec::new();
        let mut truncated = Vec::new();
        for text in texts {
            if let Some(control) = control {
                control.checkpoint()?;
            }
            let encoding = self
                .tokenizer
                .encode(format!("{}{text}", kind.prefix()), true)
                .map_err(|_| error("tokenization failed"))?;
            let mut tokens = encoding.get_ids().to_vec();
            original.push(tokens.len());
            let cut = tokens.len() > self.info.config.max_tokens;
            if cut {
                if !self.info.config.allow_truncation {
                    return Err(error("input exceeds model token window; split passage or explicitly allow truncation"));
                }
                let eos = *tokens
                    .last()
                    .ok_or_else(|| error("tokenizer produced no tokens"))?;
                tokens.truncate(self.info.config.max_tokens);
                *tokens
                    .last_mut()
                    .ok_or_else(|| error("invalid truncation"))? = eos;
            }
            truncated.push(cut);
            used.push(tokens.len());
            ids.push(tokens);
        }
        let batch = ids.len();
        let seq = used
            .iter()
            .copied()
            .max()
            .ok_or_else(|| error("empty tokenization"))?;
        let mut input = vec![1i64; batch * seq]; // XLM-R <pad> id=1
        let mut mask = vec![0i64; batch * seq];
        for (row, tokens) in ids.iter().enumerate() {
            for (col, token) in tokens.iter().enumerate() {
                input[row * seq + col] = i64::from(*token);
                mask[row * seq + col] = 1;
            }
        }
        let tokenization_us = started.elapsed().as_micros();
        let started = Instant::now();
        let input_tensor = Tensor::from_array(([batch, seq], input)).map_err(error)?;
        let mask_tensor = Tensor::from_array(([batch, seq], mask.clone())).map_err(error)?;
        let needs_types = self
            .session
            .inputs
            .iter()
            .any(|i| i.name == "token_type_ids");
        let run_options = Arc::new(RunOptions::new().map_err(error)?);
        let _interrupt = if let Some(control) = control {
            let terminate = run_options.clone();
            Some(control.interrupt(Arc::new(move || {
                let _ = terminate.terminate();
            }))?)
        } else {
            None
        };
        if let Some(control) = control {
            control.checkpoint()?;
        }
        self.info.inference_attempts += 1;
        let run = if needs_types {
            let types =
                Tensor::from_array(([batch, seq], vec![0i64; batch * seq])).map_err(error)?;
            self.session.run_with_options(ort::inputs! { "input_ids" => input_tensor, "attention_mask" => mask_tensor, "token_type_ids" => types }, &run_options)
        } else {
            self.session.run_with_options(
                ort::inputs! { "input_ids" => input_tensor, "attention_mask" => mask_tensor },
                &run_options,
            )
        };
        let output = match run {
            Ok(output) => output,
            Err(failure) => {
                if let Some(control) = control {
                    if let Err(cancelled) = control.checkpoint() {
                        self.info.interrupted_runs += 1;
                        return Err(cancelled);
                    }
                }
                return Err(error(failure));
            }
        };
        if let Some(control) = control {
            control.checkpoint()?;
        }
        let inference_us = started.elapsed().as_micros();
        let started = Instant::now();
        let (shape, hidden) = output[0].try_extract_tensor::<f32>().map_err(error)?;
        if shape.as_ref() != [batch as i64, seq as i64, DIMENSIONS as i64] {
            return Err(error("unexpected E5 output dimensions"));
        }
        let vectors = pool(hidden, &mask, batch, seq, DIMENSIONS)?;
        if let Some(control) = control {
            control.checkpoint()?;
        }
        self.info.inference_runs += 1;
        let executed_model_tokens = used.iter().sum();
        Ok(EmbeddingBatch {
            vectors,
            original_token_counts: original,
            used_token_counts: used,
            truncated,
            tokenization_us,
            inference_us,
            pooling_us: started.elapsed().as_micros(),
            cache_hits: 0,
            cache_misses: 0,
            executed_model_tokens,
        })
    }
}

fn cache_key(config: CpuConfig, kind: InputKind, text: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    // Versioned recipe includes resources, CPU profile and exact input role.
    hash.update(b"retrieval-vector-cache/1;masked-mean-f64;l2-f32;single-row;");
    hash.update(MODEL_SHA);
    hash.update(TOKENIZER_SHA);
    hash.update(RUNTIME_SHA);
    hash.update((config.threads as u64).to_le_bytes());
    hash.update((config.max_tokens as u64).to_le_bytes());
    hash.update([u8::from(config.allow_truncation)]);
    hash.update(kind.prefix());
    hash.update(text.as_bytes());
    hash.finalize().into()
}

pub(crate) fn pool(
    hidden: &[f32],
    mask: &[i64],
    batch: usize,
    seq: usize,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, Error> {
    if hidden.len() != batch * seq * dimensions || mask.len() != batch * seq {
        return Err(error("invalid pooling shape"));
    }
    let mut output = Vec::with_capacity(batch);
    for row in 0..batch {
        let mut vector = vec![0.0f64; dimensions];
        let mut count = 0;
        for col in 0..seq {
            if mask[row * seq + col] == 0 {
                continue;
            }
            count += 1;
            for d in 0..dimensions {
                vector[d] += f64::from(hidden[(row * seq + col) * dimensions + d]);
            }
        }
        if count == 0 {
            return Err(error("empty attention mask"));
        }
        for v in &mut vector {
            *v /= count as f64;
        }
        let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
        if !norm.is_finite() || norm <= f64::EPSILON {
            return Err(error("non-finite or zero embedding"));
        }
        output.push(vector.iter().map(|v| (v / norm) as f32).collect());
    }
    Ok(output)
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Result<f32, Error> {
    if left.is_empty() || left.len() != right.len() {
        return Err(error("incompatible embedding dimensions"));
    }
    let (mut dot, mut a, mut b) = (0.0f64, 0.0f64, 0.0f64);
    for (&l, &r) in left.iter().zip(right) {
        if !l.is_finite() || !r.is_finite() {
            return Err(error("non-finite embedding"));
        }
        dot += f64::from(l) * f64::from(r);
        a += f64::from(l).powi(2);
        b += f64::from(r).powi(2);
    }
    if a <= f64::EPSILON || b <= f64::EPSILON {
        return Err(error("zero embedding"));
    }
    Ok((dot / (a * b).sqrt()).clamp(-1.0, 1.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_identity_separates_roles_profiles_and_exact_text() {
        let base = CpuConfig::default();
        let key = cache_key(base, InputKind::Passage, "ação");
        assert_ne!(key, cache_key(base, InputKind::Query, "ação"));
        assert_ne!(key, cache_key(base, InputKind::Passage, "acão"));
        assert_ne!(key, cache_key(base, InputKind::Passage, "ação "));
        for changed in [
            CpuConfig {
                max_tokens: 32,
                ..base
            },
            CpuConfig { threads: 1, ..base },
            CpuConfig {
                allow_truncation: true,
                ..base
            },
        ] {
            assert_ne!(key, cache_key(changed, InputKind::Passage, "ação"));
        }
    }
    #[test]
    fn pooling_masks_padding_and_normalizes() {
        let pooled = pool(&[3., 4., 999., 999.], &[1, 0], 1, 2, 2).unwrap();
        assert!((pooled[0][0] - 0.6).abs() < 1e-6);
        assert!((pooled[0][1] - 0.8).abs() < 1e-6);
        assert!(pool(&[0., 0.], &[0], 1, 1, 2).is_err());
        assert_eq!(InputKind::Query.prefix(), "query: ");
        assert_eq!(InputKind::Passage.prefix(), "passage: ");
    }

    #[test]
    fn resource_hash_rejects_same_size_tampering() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("hash-resource-{}.bin", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(hash_file(&path, expected, 3).is_ok());
        assert!(hash_file(&path, expected, 4).is_err());
        std::fs::write(&path, b"abd").unwrap();
        assert!(hash_file(&path, expected, 3).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
