//! Reusable local JSONL worker: the parent broker can share this process.
use retrieval_engine::{CpuConfig, E5Embedder, InputKind};
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, Read, Write};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    id: String,
    kind: InputKind,
    texts: Vec<String>,
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty()
        || args.len() > 3
        || args
            .iter()
            .skip(1)
            .any(|a| !matches!(a.as_str(), "--allow-truncation" | "--packed-batching"))
    {
        return Err(
            "usage: retrieval-embeddings MODEL_DIR [--allow-truncation] [--packed-batching]".into(),
        );
    }
    let config = CpuConfig {
        allow_truncation: args.iter().any(|a| a == "--allow-truncation"),
        packed_batching: args.iter().any(|a| a == "--packed-batching"),
        ..CpuConfig::default()
    };
    let mut model = E5Embedder::load(&args[0], config)?;
    let mut reader = std::io::stdin().lock();
    let mut writer = std::io::stdout().lock();
    loop {
        let mut bytes = Vec::new();
        let n = Read::take(&mut reader, 128 * 1024 + 1).read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        if bytes.len() > 128 * 1024 {
            return Err("request line exceeds 128KiB; worker closed".into());
        }
        let response = match serde_json::from_slice::<Request>(&bytes) {
            Ok(req) if req.schema_version == 1 && !req.id.is_empty() && req.id.len() <= 128 => {
                match model.embed(req.kind, &req.texts) {
                    Ok(batch) => {
                        json!({"schema_version":1,"id":req.id,"ok":true,"result":batch,"model":model.info()})
                    }
                    Err(error) => {
                        json!({"schema_version":1,"id":req.id,"ok":false,"error":error.to_string()})
                    }
                }
            }
            _ => json!({"schema_version":1,"id":null,"ok":false,"error":"invalid request schema"}),
        };
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
