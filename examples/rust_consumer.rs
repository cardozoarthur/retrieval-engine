//! Actual Rust host -> resident worker -> verified pack -> dedicated SQLite.
//! The command-line paths here are trusted host configuration, not model input.
#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use retrieval_engine::{
        evidence_store::{EvidenceStore, SqliteEvidenceStore, StoreLimits},
        process_client::{ClientConfig, ProcessClient},
        SearchMode, SourceSpec, Unit,
    };
    use std::{path::PathBuf, time::Duration};
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(4..=5).contains(&args.len()) {
        return Err(
            "usage: rust_consumer WORKER SOURCE QUERY NEW_EVIDENCE_DB [TRUSTED_MODEL_DIR]".into(),
        );
    }
    let mut config = ClientConfig {
        program: PathBuf::from(&args[0]),
        model_dir: args.get(4).map(PathBuf::from),
        ..Default::default()
    };
    let source = if args[1].starts_with("https://") {
        SourceSpec::Remote {
            url: args[1].clone(),
        }
    } else {
        let path = PathBuf::from(&args[1]).canonicalize()?;
        config
            .local_roots
            .push(path.parent().ok_or("source parent unavailable")?.into());
        SourceSpec::Local {
            path: path.to_string_lossy().into(),
        }
    };
    let mode = if config.model_dir.is_some() {
        SearchMode::Hybrid
    } else {
        SearchMode::Bm25
    };
    let client = ProcessClient::spawn(config)?;
    let session = client.open(source, Duration::from_secs(30))?;
    let found = session.search(&args[2], mode, 3, Duration::from_secs(30))?;
    if found.entries.is_empty() {
        return Err("no evidence found; no evidence database created".into());
    }
    let mut references = Vec::new();
    for entry in &found.entries {
        references.push(
            session
                .expand(&entry.reference, 8, 16, Unit::Words, Duration::from_secs(5))?
                .reference,
        );
    }
    let evidence = session.verified_pack(&references, Duration::from_secs(5))?;
    let mut store = SqliteEvidenceStore::create(&args[3], "host-example", StoreLimits::default())?;
    let receipt = store.put("selected-evidence-1", &evidence)?;
    session.close(Duration::from_secs(5))?;
    let shutdown = client.shutdown();
    if !shutdown.exit_observed || !shutdown.tree_exit_observed {
        return Err("owned worker tree termination was not observed".into());
    }
    drop(store);
    let mut reopened = SqliteEvidenceStore::open(&args[3], "host-example", StoreLimits::default())?;
    let (stored, pack) = reopened
        .get("selected-evidence-1")?
        .ok_or("stored evidence missing")?;
    stored.verify(&pack)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"schema_version":1,"scope":"actual Rust host/worker/SQLite; not external Foundry admission","worker_stopped":shutdown.exit_observed&&shutdown.tree_exit_observed,"shutdown":shutdown,"receipt":receipt,"reopened_receipt":stored,"entries":pack.payload.entries.len(),"pack_sha256":pack.sha256})
        )?
    );
    Ok(())
}
fn main() {
    #[cfg(windows)]
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("Resident process client currently qualified on Windows only");
        std::process::exit(1);
    }
}
