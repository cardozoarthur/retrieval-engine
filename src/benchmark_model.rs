//! Opt-in, pinned CPU drivers for measurement. Never changes the production E5
//! default or accepts arbitrary native graphs. No vector cache, no truncation,
//! one text per native run. Resource setup is explicit and separate.
use crate::{
    document::error,
    semantic::{hash_file, initialize_runtime, pool},
    Error, InputKind, RequestControl,
};
use ort::{
    execution_providers::CPUExecutionProvider,
    session::{builder::GraphOptimizationLevel, run_options::RunOptions, Session},
    value::Tensor,
};
use serde::Serialize;
use std::{path::Path, sync::Arc, time::Instant};
use tokenizers::Tokenizer;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkProfile {
    E5Int8,
    E5Fp32,
    MiniLmInt8,
    MiniLmFp32,
}

#[derive(Clone, Debug, Serialize)]
pub struct BenchmarkModelInfo {
    pub profile: BenchmarkProfile,
    pub model: &'static str,
    pub revision: &'static str,
    pub model_sha256: &'static str,
    pub tokenizer_sha256: &'static str,
    pub dimensions: usize,
    pub max_tokens: usize,
    pub pooling: &'static str,
    pub role_prefixes: bool,
    pub weight_format: &'static str,
    pub provider: &'static str,
    pub runtime_version: &'static str,
    pub threads: usize,
    pub load_us: u128,
    pub inference_runs: usize,
    pub cache_enabled: bool,
}

struct Resource {
    path: &'static str,
    sha: &'static str,
    bytes: u64,
}
struct Recipe {
    name: &'static str,
    revision: &'static str,
    model: Resource,
    tokenizer: Resource,
    window: usize,
    prefixes: bool,
    weight: &'static str,
}
impl BenchmarkProfile {
    fn recipe(self) -> Recipe {
        let e5 = matches!(self, Self::E5Int8 | Self::E5Fp32);
        let (path, sha, bytes, weight) = match self {
            Self::E5Int8 => (
                "e5-small-int8/model.onnx",
                "4d24e2bc01a447951524466ef533e52944bf48509e6552810bcee1a2711cb02c",
                118054593,
                "int8",
            ),
            Self::E5Fp32 => (
                "benchmark/e5-fp32.onnx",
                "4aa845c27760e06e9a686b9d8b5d440eae4b6612cd09e5b522b716d3941f77ff",
                470268533,
                "fp32",
            ),
            Self::MiniLmInt8 => (
                "benchmark/minilm-int8.onnx",
                "98a01d88b7de996cdea58c32ca71208c09968d143798814b2ea09d3439dc334f",
                118453870,
                "quint8_avx2",
            ),
            Self::MiniLmFp32 => (
                "benchmark/minilm-fp32.onnx",
                "10f7a088420252b26caf819236ca2c9d2987afd0fc06fec7553b542a5655a05a",
                470301610,
                "fp32",
            ),
        };
        Recipe {
            name: if e5 {
                "intfloat/multilingual-e5-small"
            } else {
                "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2"
            },
            revision: if e5 {
                "761b726dd34fb83930e26aab4e9ac3899aa1fa78"
            } else {
                "e8f8c211226b894fcb81acc59f3b34ba3efd5f42"
            },
            model: Resource { path, sha, bytes },
            tokenizer: if e5 {
                Resource {
                    path: "e5-small-int8/tokenizer.json",
                    sha: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
                    bytes: 17082730,
                }
            } else {
                Resource {
                    path: "benchmark/minilm-tokenizer.json",
                    sha: "2c3387be76557bd40970cec13153b3bbf80407865484b209e655e5e4729076b8",
                    bytes: 9081518,
                }
            },
            window: if e5 { 512 } else { 128 },
            prefixes: e5,
            weight,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MeasuredEmbedding {
    pub vector: Vec<f32>,
    pub tokens: usize,
    pub tokenization_us: u128,
    pub inference_us: u128,
    pub pooling_us: u128,
}
pub struct BenchmarkEncoder {
    session: Session,
    tokenizer: Tokenizer,
    info: BenchmarkModelInfo,
}
impl BenchmarkEncoder {
    pub fn load(
        assets: impl AsRef<Path>,
        profile: BenchmarkProfile,
        threads: usize,
    ) -> Result<Self, Error> {
        if !(1..=8).contains(&threads) {
            return Err(error("benchmark threads must be 1..8"));
        }
        let start = Instant::now();
        let root = assets.as_ref().canonicalize().map_err(error)?;
        let recipe = profile.recipe();
        for r in [&recipe.model, &recipe.tokenizer] {
            hash_file(&root.join(r.path), r.sha, r.bytes)?;
        }
        let runtime = root.join("e5-small-int8");
        hash_file(
            &runtime.join("onnxruntime.dll"),
            "579b636403983254346a5c1d80bd28f1519cd1e284cd204f8d4ff41f8d711559",
            12418080,
        )?;
        hash_file(
            &runtime.join("onnxruntime_providers_shared.dll"),
            "ba00ea1ef846c9b909c7854bc56c51051a20f9773b3e1153dda118d4b85d0b93",
            22064,
        )?;
        initialize_runtime(&runtime)?;
        let mut tokenizer =
            Tokenizer::from_file(root.join(recipe.tokenizer.path)).map_err(error)?;
        tokenizer.with_padding(None);
        tokenizer.with_truncation(None).map_err(error)?;
        let session = Session::builder()
            .map_err(error)?
            .with_execution_providers([CPUExecutionProvider::default().build()])
            .map_err(error)?
            .with_intra_threads(threads)
            .map_err(error)?
            .with_inter_threads(1)
            .map_err(error)?
            .with_parallel_execution(false)
            .map_err(error)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(error)?
            .commit_from_file(root.join(recipe.model.path))
            .map_err(error)?;
        if session.outputs.len() != 1
            || !session.inputs.iter().any(|i| i.name == "input_ids")
            || !session.inputs.iter().any(|i| i.name == "attention_mask")
            || session.inputs.iter().any(|i| {
                !matches!(
                    i.name.as_str(),
                    "input_ids" | "attention_mask" | "token_type_ids"
                )
            })
        {
            return Err(error("unexpected benchmark ONNX graph contract"));
        }
        Ok(Self {
            session,
            tokenizer,
            info: BenchmarkModelInfo {
                profile,
                model: recipe.name,
                revision: recipe.revision,
                model_sha256: recipe.model.sha,
                tokenizer_sha256: recipe.tokenizer.sha,
                dimensions: 384,
                max_tokens: recipe.window,
                pooling: "masked_mean_f64_l2",
                role_prefixes: recipe.prefixes,
                weight_format: recipe.weight,
                provider: "CPUExecutionProvider",
                runtime_version: "1.22.0",
                threads,
                load_us: start.elapsed().as_micros(),
                inference_runs: 0,
                cache_enabled: false,
            },
        })
    }
    pub fn info(&self) -> &BenchmarkModelInfo {
        &self.info
    }
    fn tokens(&self, kind: InputKind, text: &str) -> Result<Vec<i64>, Error> {
        if text.trim().is_empty() || text.len() > 16 * 1024 {
            return Err(error("benchmark input empty or exceeds 16KiB"));
        }
        let prefix = if self.info.role_prefixes {
            match kind {
                InputKind::Query => "query: ",
                InputKind::Passage => "passage: ",
            }
        } else {
            ""
        };
        let encoding = self
            .tokenizer
            .encode(format!("{prefix}{text}"), true)
            .map_err(error)?;
        if encoding.is_empty() || encoding.len() > self.info.max_tokens {
            return Err(error(
                "benchmark input exceeds model window; no truncation allowed",
            ));
        }
        Ok(encoding.get_ids().iter().map(|&v| i64::from(v)).collect())
    }
    pub fn token_count(&self, kind: InputKind, text: &str) -> Result<usize, Error> {
        Ok(self.tokens(kind, text)?.len())
    }
    pub fn embed(
        &mut self,
        kind: InputKind,
        text: &str,
        control: &RequestControl,
    ) -> Result<MeasuredEmbedding, Error> {
        control.checkpoint()?;
        let started = Instant::now();
        let ids = self.tokens(kind, text)?;
        let n = ids.len();
        let tokenization_us = started.elapsed().as_micros();
        let started = Instant::now();
        let input = Tensor::from_array(([1, n], ids)).map_err(error)?;
        let mask = Tensor::from_array(([1, n], vec![1i64; n])).map_err(error)?;
        let opts = Arc::new(RunOptions::new().map_err(error)?);
        let terminate = opts.clone();
        let _interrupt = control.interrupt(Arc::new(move || {
            let _ = terminate.terminate();
        }))?;
        control.checkpoint()?;
        let run = if self
            .session
            .inputs
            .iter()
            .any(|i| i.name == "token_type_ids")
        {
            let types = Tensor::from_array(([1, n], vec![0i64; n])).map_err(error)?;
            self.session.run_with_options(
                ort::inputs! {"input_ids"=>input,"attention_mask"=>mask,"token_type_ids"=>types},
                &opts,
            )
        } else {
            self.session.run_with_options(
                ort::inputs! {"input_ids"=>input,"attention_mask"=>mask},
                &opts,
            )
        };
        control.checkpoint()?;
        let output = run.map_err(error)?;
        let inference_us = started.elapsed().as_micros();
        let started = Instant::now();
        let (shape, hidden) = output[0].try_extract_tensor::<f32>().map_err(error)?;
        if shape.as_ref() != [1, n as i64, 384] {
            return Err(error("unexpected benchmark output dimensions"));
        }
        let mut vectors = pool(hidden, &vec![1; n], 1, n, 384)?;
        control.checkpoint()?;
        self.info.inference_runs += 1;
        Ok(MeasuredEmbedding {
            vector: vectors.remove(0),
            tokens: n,
            tokenization_us,
            inference_us,
            pooling_us: started.elapsed().as_micros(),
        })
    }
}
