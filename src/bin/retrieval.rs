use retrieval_engine::{serve_jsonl, JsonlConfig, RetrievalWorker, WorkerConfig};
use std::time::Duration;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--foundry-job") {
        #[cfg(windows)]
        {
            let args: Vec<_> = std::env::args().skip(2).collect();
            if args.len() != 4 || args[0] != "--job" || args[2] != "--out" {
                return Err("Expected --foundry-job --job FILE --out DIRECTORY".into());
            }
            retrieval_engine::foundry_adapter::run_job(args[1].as_ref(), args[3].as_ref())?;
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            return Err("Foundry guarded adapter currently requires Windows".into());
        }
    }
    if std::env::args().len() == 2 && std::env::args().nth(1).as_deref() == Some("--extract-helper")
    {
        #[cfg(windows)]
        {
            retrieval_engine::run_extraction_helper()?;
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            return Err("Isolated extraction helper unsupported on this platform".into());
        }
    }
    let mut config = WorkerConfig::default();
    #[cfg(windows)]
    {
        config.extraction_helper = Some(std::env::current_exe()?);
    }
    let mut transport = JsonlConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--model-dir" => {
                if config.model_dir.is_some() {
                    return Err("Only one model directory may be configured".into());
                }
                config.model_dir = Some(args.next().ok_or("Missing model directory")?.into());
            }
            "--local-root" => config
                .local_roots
                .push(args.next().ok_or("Missing permitted local root")?.into()),
            "--session-ttl-ms" => {
                config.session_ttl = Duration::from_millis(
                    args.next()
                        .ok_or("Missing session TTL")?
                        .parse()
                        .map_err(|_| "Invalid session TTL")?,
                );
            }
            "--help" => {
                println!("retrieval-worker [--model-dir DIR] [--local-root DIR ...] [--session-ttl-ms N] [--request-timeout-ms N] [--max-request-ids N] [--extraction-memory-mib N on Windows] [--max-open-tasks N] [--open-task-ttl-ms N] [--open-task-timeout-ms N]\nJSONL retrieval-worker/v1 on stdin/stdout; no daemon or network listener\nFoundry envelope on Windows: --foundry-job --job FILE --out DIRECTORY");
                return Ok(());
            }
            "--request-timeout-ms" => {
                transport.request_timeout = Duration::from_millis(
                    args.next()
                        .ok_or("Missing request timeout")?
                        .parse()
                        .map_err(|_| "Invalid request timeout")?,
                )
            }
            "--max-request-ids" => {
                transport.max_request_ids = args
                    .next()
                    .ok_or("Missing request ID limit")?
                    .parse()
                    .map_err(|_| "Invalid request ID limit")?
            }
            "--max-open-tasks" => {
                config.open_tasks.max_live = args
                    .next()
                    .ok_or("Missing open task limit")?
                    .parse()
                    .map_err(|_| "Invalid open task limit")?
            }
            "--open-task-ttl-ms" => {
                config.open_tasks.idle_ttl = Duration::from_millis(
                    args.next()
                        .ok_or("Missing open task TTL")?
                        .parse()
                        .map_err(|_| "Invalid open task TTL")?,
                )
            }
            "--open-task-timeout-ms" => {
                config.open_tasks.extraction_timeout = Duration::from_millis(
                    args.next()
                        .ok_or("Missing open task timeout")?
                        .parse()
                        .map_err(|_| "Invalid open task timeout")?,
                )
            }
            #[cfg(windows)]
            "--extraction-memory-mib" => {
                let mib: usize = args
                    .next()
                    .ok_or("Missing extraction memory limit")?
                    .parse()
                    .map_err(|_| "Invalid extraction memory limit")?;
                config.isolation.memory_bytes = mib
                    .checked_mul(1024 * 1024)
                    .ok_or("Invalid extraction memory limit")?;
            }
            _ => return Err("Unknown startup option; use --help".into()),
        }
    }
    serve_jsonl(RetrievalWorker::new(config)?, transport)?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
