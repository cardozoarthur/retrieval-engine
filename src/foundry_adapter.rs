//! External Foundry v1 adapter. Selected bytes/resources come from the trusted
//! host envelope, never input paths. No HTTP client is invoked by this adapter.
use crate::{
    document::{error, sha},
    ChunkConfig, Document, Error, EvidenceLimits, EvidencePack, EvidenceSession, ExtractionTask,
    IndexLimits, IsolationConfig, LexicalIndex, Limits, RequestControl, SearchMode, Source, Unit,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Job {
    api_version: u32,
    job_id: String,
    project_id: String,
    workflow_id: String,
    input: Value,
    output_directory: PathBuf,
    #[serde(default)]
    input_artifacts: Vec<Selected>,
    #[serde(default)]
    resources: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selected {
    artifact_id: String,
    sha256: String,
    bytes: u64,
    name: String,
    path: PathBuf,
    #[serde(default)]
    public_http: Option<Value>,
    /// Current host receipt metadata, separate from source identity and grants.
    #[serde(default)]
    origin: Option<Value>,
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Retrieve {
        artifact_id: String,
        query: String,
        mode: SearchMode,
        #[serde(default = "three")]
        top_k: usize,
        #[serde(default)]
        before: usize,
        #[serde(default)]
        after: usize,
        unit: Unit,
    },
    Expand {
        artifact_id: String,
        pack_artifact_id: String,
        reference: String,
        before: usize,
        after: usize,
        unit: Unit,
    },
}
fn three() -> usize {
    3
}
fn uuid(s: &str) -> bool {
    s.len() == 36
        && s != "00000000-0000-0000-0000-000000000000"
        && s.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
            }
        })
}
fn hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn regular(path: &Path) -> Result<PathBuf, Error> {
    let full = path
        .canonicalize()
        .map_err(|_| error("selected path unavailable"))?;
    let mut part = PathBuf::new();
    use std::os::windows::fs::MetadataExt;
    for component in path.components() {
        part.push(component);
        if fs::symlink_metadata(&part)
            .map_err(|_| error("selected path unavailable"))?
            .file_attributes()
            & 0x400
            != 0
        {
            return Err(error("reparse points not accepted in selected paths"));
        }
    }
    if !full.is_file() {
        return Err(error("selected input must be a regular file"));
    }
    Ok(full)
}
fn read_json(path: &Path, max: usize) -> Result<Value, Error> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(error)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() > max {
        return Err(error("adapter JSON budget exceeded"));
    }
    serde_json::from_slice(&bytes).map_err(|_| error("invalid adapter JSON"))
}
fn selected<'a>(job: &'a Job, id: &str, inputs: &Path) -> Result<&'a Selected, Error> {
    let item = job
        .input_artifacts
        .iter()
        .find(|a| a.artifact_id == id)
        .ok_or_else(|| error("input artifact was not selected by host"))?;
    if !item.path.is_absolute()
        || regular(&item.path)?
            != inputs
                .join(&item.artifact_id)
                .canonicalize()
                .map_err(|_| error("selected artifact missing from job inputs"))?
    {
        return Err(error("selected artifact outside job input binding"));
    }
    Ok(item)
}
fn verified_bytes(item: &Selected, max: usize) -> Result<Vec<u8>, Error> {
    if item.bytes == 0 || item.bytes > max as u64 {
        return Err(error("selected artifact exceeds adapter byte budget"));
    }
    let mut bytes = Vec::new();
    File::open(&item.path)
        .map_err(error)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() as u64 != item.bytes || sha(&bytes) != item.sha256 {
        return Err(error("selected artifact bytes/hash changed"));
    }
    Ok(bytes)
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn source(
    job: &Job,
    item: &Selected,
    out: &Path,
    control: &RequestControl,
) -> Result<(Arc<Document>, Value), Error> {
    let bytes = verified_bytes(item, 8 * 1024 * 1024)?;
    let extension = if bytes.starts_with(b"%PDF-") {
        "pdf"
    } else {
        match Path::new(&item.name)
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "md" | "csv" => "txt",
            "html" | "htm" => "html",
            _ => return Err(error("selected source must be text/HTML/PDF")),
        }
    };
    let work = out.join("work");
    fs::create_dir(&work).map_err(|_| error("job work already exists; no implicit replay"))?;
    let alias = Scratch(work.join(format!("selected.{extension}")));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&alias.0)
        .map_err(error)?;
    file.write_all(&bytes).map_err(error)?;
    file.sync_all().map_err(error)?;
    drop(file);
    drop(bytes);
    control.checkpoint()?;
    let helper = std::env::current_exe().map_err(error)?;
    let task = ExtractionTask::start(
        &helper,
        Source::Local(alias.0.clone()),
        Limits::default(),
        IsolationConfig::default(),
        control.clone(),
    )?;
    let (doc, isolation) = task.finish()?;
    if doc.provenance().raw_sha256 != item.sha256 || doc.provenance().raw_bytes as u64 != item.bytes
    {
        return Err(error("extracted source differs from selected artifact"));
    }
    let origin = format!(
        "urn:retrieval:selected:{}:{}",
        job.project_id, item.artifact_id
    );
    Ok((
        Arc::new(doc.selected_origin(origin)?),
        serde_json::to_value(isolation).map_err(error)?,
    ))
}
fn validate(job: &Job, input: &Input, out: &Path) -> Result<PathBuf, Error> {
    if job.api_version != 1
        || !uuid(&job.job_id)
        || !uuid(&job.project_id)
        || job.workflow_id != "request"
        || job.input_artifacts.is_empty()
        || job.input_artifacts.len() > 16
        || job.resources.len() > 4
    {
        return Err(error("invalid adapter host envelope"));
    }
    if !job.output_directory.is_absolute()
        || job.output_directory.canonicalize().map_err(error)? != out
    {
        return Err(error("output directory does not match host envelope"));
    }
    let mut ids = BTreeSet::new();
    for item in &job.input_artifacts {
        if !uuid(&item.artifact_id)
            || !hash(&item.sha256)
            || item.name.is_empty()
            || item.name.len() > 800
            || item
                .name
                .chars()
                .any(|c| c.is_control() || "/\\:".contains(c))
            || !ids.insert(&item.artifact_id)
        {
            return Err(error("invalid/duplicate selected artifact metadata"));
        }
    }
    let valid = match input {
        Input::Retrieve {
            artifact_id,
            query,
            top_k,
            before,
            after,
            ..
        } => {
            uuid(artifact_id)
                && !query.trim().is_empty()
                && query.len() <= 4096
                && (1..=8).contains(top_k)
                && *before <= 4096
                && *after <= 4096
        }
        Input::Expand {
            artifact_id,
            pack_artifact_id,
            reference,
            before,
            after,
            ..
        } => {
            uuid(artifact_id)
                && uuid(pack_artifact_id)
                && artifact_id != pack_artifact_id
                && hash(reference)
                && *before <= 4096
                && *after <= 4096
        }
    };
    if !valid {
        return Err(error("invalid retrieval adapter input"));
    }
    if out.join("result.json").exists()
        || out.join("retrieval.json").exists()
        || out.join("pack.json").exists()
    {
        return Err(error("job output already exists; reconciliation required"));
    }
    let inputs = out
        .parent()
        .ok_or_else(|| error("job root unavailable"))?
        .join("inputs")
        .canonicalize()
        .map_err(|_| error("host-selected inputs directory unavailable"))?;
    Ok(inputs)
}
#[cfg(feature = "cpu-onnx")]
fn model_directory(job: &Job) -> Result<PathBuf, Error> {
    let candidates: Vec<_> = job
        .resources
        .iter()
        .filter(|r| r["resourceId"] == "retrieval-e5-small-int8")
        .collect();
    if candidates.len() != 1 {
        return Err(error("one explicit E5 model resource grant required"));
    }
    let grant = candidates[0];
    if grant["apiVersion"] != 1
        || grant["workspaceId"] != job.project_id
        || grant["permission"] != "resource.read"
        || grant["manifest"]["id"] != "retrieval-e5-small-int8"
        || grant["manifest"]["apiVersion"] != 1
    {
        return Err(error("model resource grant does not match host scope"));
    }
    for key in ["grantId", "installationId", "registrationId"] {
        if !grant[key].as_str().is_some_and(uuid) {
            return Err(error("invalid model grant identity"));
        }
    }
    for key in ["resourceFingerprint", "pluginFingerprint"] {
        if !grant[key].as_str().is_some_and(hash) {
            return Err(error("invalid model grant fingerprint"));
        }
    }
    let path = PathBuf::from(
        grant["path"]
            .as_str()
            .ok_or_else(|| error("model resource path missing"))?,
    );
    if !path.is_absolute() {
        return Err(error(
            "model resource path must be host-owned absolute path",
        ));
    }
    Ok(path)
}
fn write_artifact(out: &Path, name: &str, value: &Value, kind: &str) -> Result<Value, Error> {
    let bytes = serde_json::to_vec(value).map_err(error)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(error("adapter artifact exceeds output budget"));
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join(name))
        .map_err(|_| error("artifact exists or is inaccessible"))?;
    f.write_all(&bytes)
        .and_then(|_| f.sync_all())
        .map_err(error)?;
    Ok(
        json!({"kind":kind,"path":name,"mimeType":"application/json","sha256":sha(&bytes),"bytes":bytes.len()}),
    )
}
fn execute(job: Job, out: &Path, phase: &mut &'static str) -> Result<(), Error> {
    let input: Input = serde_json::from_value(job.input.clone())
        .map_err(|_| error("adapter input contains unsupported fields"))?;
    let inputs = validate(&job, &input, out)?;
    *phase = "selected_input_binding";
    let artifact_id = match &input {
        Input::Retrieve { artifact_id, .. } | Input::Expand { artifact_id, .. } => artifact_id,
    };
    let item = selected(&job, artifact_id, &inputs)?;
    // Validate the requested selected pack before allocating a parser task.
    let previous = if let Input::Expand {
        pack_artifact_id, ..
    } = &input
    {
        let pack_item = selected(&job, pack_artifact_id, &inputs)?;
        let bytes = verified_bytes(pack_item, 4 * 1024 * 1024)?;
        let pack: EvidencePack =
            serde_json::from_slice(&bytes).map_err(|_| error("invalid selected evidence pack"))?;
        pack.verify_integrity()?;
        Some(pack)
    } else {
        None
    };
    let control = RequestControl::new(Duration::from_secs(60))?;
    *phase = "guarded_source_extraction";
    let (doc, isolation) = source(&job, item, out, &control)?;
    *phase = "retrieval_or_restore";
    let limits = EvidenceLimits {
        max_context_chars: 8192,
        max_pack_entries: 8,
        max_pack_chars: 65536,
        ..Default::default()
    };
    let (entries, pack, search) = match input {
        Input::Retrieve {
            query,
            mode,
            top_k,
            before,
            after,
            unit,
            ..
        } => {
            let mut session = EvidenceSession::new(vec![doc.clone()], limits)?;
            let (hits, search) = if mode == SearchMode::Literal {
                (
                    doc.find(&query, top_k)?,
                    json!({"mode":"literal","case_sensitive":true,"coverage_scope":"extracted_text"}),
                )
            } else {
                let index = LexicalIndex::build(
                    vec![doc.clone()],
                    ChunkConfig::default(),
                    IndexLimits::default(),
                )?;
                if mode == SearchMode::Bm25 {
                    (
                        index
                            .search(&query, top_k)?
                            .into_iter()
                            .map(|h| h.evidence)
                            .collect(),
                        json!({"mode":"bm25","index_stats":index.stats()}),
                    )
                } else {
                    #[cfg(feature = "cpu-onnx")]
                    {
                        *phase = "model_grant";
                        let model_dir = model_directory(&job)?;
                        *phase = "model_load";
                        let mut model =
                            crate::E5Embedder::load(model_dir, crate::CpuConfig::default())?;
                        let mut options = crate::HybridOptions {
                            top_k,
                            ..Default::default()
                        };
                        if mode == SearchMode::HybridPrefilter {
                            options.coverage = crate::SemanticCoverage::LexicalPrefilter;
                            options.lexical_candidates = 16;
                            options.semantic_candidates = 16;
                            options.budget.max_semantic_chunks = 16;
                        }
                        *phase = "model_inference";
                        let response = crate::hybrid_search_with_control(
                            &index, &mut model, &query, &options, None, &control,
                        )?;
                        (
                            response.hits.into_iter().map(|h| h.evidence).collect(),
                            json!({"mode":mode,"model":response.model,"cost":response.cost,"coverage":response.coverage}),
                        )
                    }
                    #[cfg(not(feature = "cpu-onnx"))]
                    {
                        return Err(error(
                            "CPU ONNX feature unavailable in selected native adapter",
                        ));
                    }
                }
            };
            let mut entries = if hits.is_empty() {
                Vec::new()
            } else {
                session.register_many(&query, &hits)?
            };
            if before != 0 || after != 0 {
                entries = entries
                    .iter()
                    .map(|v| session.expand(&v.reference, before, after, unit))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            let pack = if entries.is_empty() {
                None
            } else {
                Some(
                    session.export(
                        &entries
                            .iter()
                            .map(|v| v.reference.clone())
                            .collect::<Vec<_>>(),
                    )?,
                )
            };
            (entries, pack, search)
        }
        Input::Expand {
            reference,
            before,
            after,
            unit,
            ..
        } => {
            let previous = previous
                .as_ref()
                .ok_or_else(|| error("selected previous pack missing"))?;
            let mut session = EvidenceSession::restore(previous, vec![doc.clone()], limits)?;
            let view = session.expand(&reference, before, after, unit)?;
            let pack = session.export(std::slice::from_ref(&view.reference))?;
            (
                vec![view],
                Some(pack),
                json!({"mode":"offline_expansion","acquisition_repeated":false,"previous_pack_sha256":previous.sha256}),
            )
        }
    };
    *phase = "pack_verification";
    control.checkpoint()?;
    if let Some(pack) = &pack {
        pack.verify_against(&[doc.as_ref()])?;
    }
    *phase = "artifact_write";
    let mut artifacts = Vec::new();
    if let Some(pack) = &pack {
        artifacts.push(write_artifact(
            out,
            "pack.json",
            &serde_json::to_value(pack).map_err(error)?,
            "evidence",
        )?);
    }
    let report = json!({"schema_version":1,"operation":"retrieval","selected_source":{"artifact_id":item.artifact_id,"sha256":item.sha256,"bytes":item.bytes,"name":item.name,"host_public_http_metadata":item.public_http,"host_origin_metadata":item.origin},"provenance":doc.provenance(),"pages":doc.pages(),"warnings":doc.warnings(),"isolation":isolation,"entries":entries,"pack_sha256":pack.as_ref().map(|p|&p.sha256),"search":search,"offset_unit":"unicode_scalar","http_requests_by_adapter":0,"large_llm_calls":0});
    artifacts.push(write_artifact(out, "retrieval.json", &report, "source")?);
    let result = json!({"apiVersion":1,"jobId":job.job_id,"workflowId":job.workflow_id,"status":"completed","artifacts":artifacts,"evidence":[{"name":"retrieval","path":"retrieval.json"}],"limitations":["Trusted local code; process supervision is not an OS permission sandbox.","Selected bytes only. HTTP acquisition provenance, authorization and resource grants are owned by the host.","Scores are rankings, not confidence or legal/medical validation.","Stateless workflow; later expansion requires reselecting the same source artifact and pack. No automatic GET or model sharing across separate jobs."]});
    let staged = write_artifact(out, "result.pending.json", &result, "evidence")?;
    let _ = staged;
    control.checkpoint()?;
    control.commit(|| Ok(()))?;
    fs::hard_link(out.join("result.pending.json"), out.join("result.json"))
        .map_err(|_| error("result publication failed; reconciliation required"))?;
    fs::remove_file(out.join("result.pending.json")).map_err(error)?;
    Ok(())
}
/// Invoked only by the package's Node shim or an explicitly trusted local host.
/// This is an envelope consumer, not an authenticated service or grant issuer.
pub fn run_job(job_file: &Path, output: &Path) -> Result<(), Error> {
    let run = || {
        let job_path = regular(job_file)?;
        let value = read_json(&job_path, 512 * 1024)?;
        let job: Job =
            serde_json::from_value(value).map_err(|_| error("invalid host envelope fields"))?;
        let out = output
            .canonicalize()
            .map_err(|_| error("host output directory unavailable"))?;
        if !out.is_dir() {
            return Err(error("host output must be a directory"));
        }
        let mut phase = "validate";
        let result = execute(job, &out, &mut phase);
        if let Err(failure) = &result {
            if phase != "validate" {
                // No selected content, input paths, resource paths or diagnostics
                // from parsers/runtimes enter the failed-job observation.
                let value = json!({"schema_version":1,"status":"failed_unconfirmed","phase":phase,"error_fingerprint":sha(failure.0.as_bytes())});
                let _ = write_artifact(&out, "failure.json", &value, "evidence");
            }
        }
        result
    };
    // Do not print selected text, paths, untrusted parser diagnostics or input.
    run().map_err(|_| error("retrieval adapter did not confirm job completion"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_input_cannot_supply_paths_or_grants() {
        for key in [
            "path",
            "model_dir",
            "resources",
            "inputArtifacts",
            "outputDirectory",
        ] {
            let mut v = json!({"operation":"retrieve","artifact_id":"11111111-1111-4111-8111-111111111111","query":"prova","mode":"literal","unit":"words"});
            v[key] = json!("untrusted");
            assert!(serde_json::from_value::<Input>(v).is_err());
        }
        assert!(uuid("11111111-1111-4111-8111-111111111111"));
        assert!(!uuid("../not-a-grant"));
    }
}
