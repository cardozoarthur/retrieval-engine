//! Windows extraction protocol. Progress is provisional until EOF, exit and hashes agree.
use crate::{
    acquisition,
    control::InterruptGuard,
    document::{error, sha, EXTRACTOR},
    native_job::{verify_current_job, NativeJob},
    Document, Error, Limits, PageRange, Provenance, RequestControl, Source,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        mpsc::{self, Receiver},
        Arc,
    },
    time::Duration,
};

const CHUNK_CHARS: usize = 8192;
#[derive(Clone, Debug)]
pub struct IsolationConfig {
    pub memory_bytes: usize,
    pub max_output_bytes: usize,
    pub max_event_bytes: usize,
}
impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            memory_bytes: 256 * 1024 * 1024,
            max_output_bytes: 16 * 1024 * 1024,
            max_event_bytes: 256 * 1024,
        }
    }
}
impl IsolationConfig {
    pub fn validate(&self) -> Result<(), Error> {
        if !(32 * 1024 * 1024..=512 * 1024 * 1024).contains(&self.memory_bytes)
            || !(1024..=128 * 1024 * 1024).contains(&self.max_output_bytes)
            || !(1024..=1024 * 1024).contains(&self.max_event_bytes)
            || self.max_event_bytes > self.max_output_bytes
        {
            return Err(error("invalid extraction isolation budgets"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquiredSource {
    pub requested_source: String,
    pub final_source: String,
    pub media_type: String,
    pub raw_bytes: usize,
    pub raw_sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtractionEvent {
    Source {
        source: AcquiredSource,
    },
    Chunk {
        page: Option<PageRange>,
        total_pages: usize,
        start: usize,
        text: String,
        final_chunk: bool,
    },
    Complete {
        provenance: Provenance,
        pages: Vec<PageRange>,
        warnings: Vec<String>,
    },
    Failed {
        code: HelperFailure,
    },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperFailure {
    SourceUnavailable,
    OcrRequired,
    ResourceLimit,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperInput {
    schema_version: u32,
    source: Source,
    limits: Limits,
    memory_bytes: usize,
}

fn emit(output: &mut impl Write, event: &ExtractionEvent) -> Result<(), Error> {
    serde_json::to_writer(&mut *output, event).map_err(error)?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(error)
}
/// Internal binary entry point. It reads a bounded handshake before touching a source.
pub fn run_extraction_helper() -> Result<(), Error> {
    let mut bytes = Vec::new();
    let read = std::io::stdin()
        .lock()
        .take(32 * 1024 + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(error)?;
    if read == 0 || bytes.len() > 32 * 1024 {
        return Err(error("extraction helper handshake rejected"));
    }
    let input: HelperInput =
        serde_json::from_slice(&bytes).map_err(|_| error("extraction helper handshake invalid"))?;
    if input.schema_version != 1
        || !(32 * 1024 * 1024..=512 * 1024 * 1024).contains(&input.memory_bytes)
    {
        return Err(error("extraction helper configuration rejected"));
    }
    verify_current_job(input.memory_bytes)?;
    input.limits.validate()?;
    let mut output = std::io::stdout().lock();
    let result = (|| {
        let acquired = acquisition::acquire(input.source, &input.limits)?;
        emit(
            &mut output,
            &ExtractionEvent::Source {
                source: AcquiredSource {
                    requested_source: acquired.requested.clone(),
                    final_source: acquired.final_source.clone(),
                    media_type: acquired.mime.clone(),
                    raw_bytes: acquired.bytes.len(),
                    raw_sha256: sha(&acquired.bytes),
                },
            },
        )?;
        let mut global = 0;
        let document =
            Document::build_observed(acquired, &input.limits, &mut |text, page, total_pages| {
                let start = page.map_or(global, |range| range.start);
                let mut chars = text.chars().peekable();
                let mut offset = start;
                loop {
                    let chunk: String = chars.by_ref().take(CHUNK_CHARS).collect();
                    let length = chunk.chars().count();
                    let last = chars.peek().is_none();
                    emit(
                        &mut output,
                        &ExtractionEvent::Chunk {
                            page: page.cloned(),
                            total_pages,
                            start: offset,
                            text: chunk,
                            final_chunk: last,
                        },
                    )?;
                    offset += length;
                    if last {
                        break;
                    }
                }
                global = offset;
                Ok(())
            })?;
        emit(
            &mut output,
            &ExtractionEvent::Complete {
                provenance: document.provenance().clone(),
                pages: document.pages().to_vec(),
                warnings: document.warnings().to_vec(),
            },
        )
    })();
    if let Err(failure) = result {
        let message = failure.to_string();
        let code = if message.contains("OCR required") {
            HelperFailure::OcrRequired
        } else if message.contains("budget") || message.contains("limit") {
            HelperFailure::ResourceLimit
        } else {
            HelperFailure::SourceUnavailable
        };
        let _ = emit(&mut output, &ExtractionEvent::Failed { code });
        return Err(error("extraction helper rejected source"));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct IsolationReport {
    pub helper_pid: u32,
    pub process_commit_limit_bytes: usize,
    pub peak_process_commit_bytes: usize,
    pub output_bytes: usize,
    pub helper_exit_confirmed: bool,
    pub kill_on_close: bool,
}
struct Block {
    page: Option<PageRange>,
}
struct Frame {
    event: ExtractionEvent,
    bytes: usize,
}
pub struct ExtractionTask {
    child: Child,
    job: Arc<NativeJob>,
    control: RequestControl,
    interrupt: Option<InterruptGuard>,
    receiver: Option<Receiver<Result<Frame, Error>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    limits: Limits,
    expected_source: String,
    source: Option<AcquiredSource>,
    text: String,
    chars: usize,
    pages: Vec<PageRange>,
    block: Option<Block>,
    blocks: usize,
    total_pages: Option<usize>,
    complete: Option<(Provenance, Vec<String>)>,
    failed: bool,
    eof: bool,
    reaped: bool,
    output_bytes: usize,
}
impl ExtractionTask {
    /// `program` is a trusted host-owned retrieval-worker executable, never a query parameter.
    pub fn start(
        program: &Path,
        source: Source,
        limits: Limits,
        config: IsolationConfig,
        control: RequestControl,
    ) -> Result<Self, Error> {
        use std::os::windows::process::CommandExt;
        config.validate()?;
        limits.validate()?;
        control.checkpoint()?;
        let expected_source = match &source {
            Source::Local(path) => path.to_string_lossy().into_owned(),
            Source::Remote(url) => url.clone(),
        };
        if expected_source.len() > 4096 {
            return Err(error("extraction source budget exceeded"));
        }
        let input = serde_json::to_vec(&HelperInput {
            schema_version: 1,
            source,
            limits: limits.clone(),
            memory_bytes: config.memory_bytes,
        })
        .map_err(error)?;
        if input.len() + 1 > 32 * 1024 {
            return Err(error("extraction handshake budget exceeded"));
        }
        let job = Arc::new(NativeJob::new(config.memory_bytes)?);
        let mut child = Command::new(program)
            .arg("--extract-helper")
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| error("extraction helper could not start"))?;
        // Child cannot acquire bytes until the handshake. EOF if the parent dies here.
        if let Err(failure) = job.attach(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failure);
        }
        let (send, receiver) = mpsc::sync_channel(2);
        let mut task = Self {
            child,
            job: job.clone(),
            control,
            interrupt: None,
            receiver: Some(receiver),
            reader: None,
            limits,
            expected_source,
            source: None,
            text: String::new(),
            chars: 0,
            pages: Vec::new(),
            block: None,
            blocks: 0,
            total_pages: None,
            complete: None,
            failed: false,
            eof: false,
            reaped: false,
            output_bytes: 0,
        };
        task.interrupt = Some(task.control.interrupt(Arc::new(move || job.terminate()))?);
        let stdout = task
            .child
            .stdout
            .take()
            .ok_or_else(|| error("extraction output missing"))?;
        task.reader = Some(
            std::thread::Builder::new()
                .name("extraction-output".into())
                .spawn(move || {
                    let mut input = BufReader::new(stdout);
                    let mut total = 0usize;
                    loop {
                        let frame = (|| {
                            let mut bytes = Vec::new();
                            let length = Read::take(&mut input, config.max_event_bytes as u64 + 1)
                                .read_until(b'\n', &mut bytes)
                                .map_err(|_| error("extraction output failed"))?;
                            if length == 0 {
                                return Ok(None);
                            }
                            total = total
                                .checked_add(length)
                                .ok_or_else(|| error("extraction output budget exceeded"))?;
                            if length > config.max_event_bytes || total > config.max_output_bytes {
                                return Err(error("extraction output budget exceeded"));
                            }
                            let event = serde_json::from_slice(&bytes)
                                .map_err(|_| error("extraction output frame invalid"))?;
                            Ok(Some(Frame {
                                event,
                                bytes: length,
                            }))
                        })();
                        match frame {
                            Ok(None) => break,
                            Ok(Some(frame)) => {
                                if send.send(Ok(frame)).is_err() {
                                    break;
                                }
                            }
                            Err(failure) => {
                                let _ = send.send(Err(failure));
                                break;
                            }
                        }
                    }
                })
                .map_err(|_| error("extraction output reader unavailable"))?,
        );
        let mut stdin = task
            .child
            .stdin
            .take()
            .ok_or_else(|| error("extraction input missing"))?;
        stdin
            .write_all(&input)
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|_| error("extraction handshake failed"))?;
        Ok(task)
    }
    pub fn source(&self) -> Option<&AcquiredSource> {
        self.source.as_ref()
    }
    pub fn provisional_text(&self) -> &str {
        &self.text
    }
    pub fn completed_pages(&self) -> &[PageRange] {
        &self.pages
    }
    pub fn total_pages(&self) -> Option<usize> {
        self.total_pages
    }
    pub fn terminal_received(&self) -> bool {
        self.complete.is_some()
    }
    pub fn helper_pid(&self) -> u32 {
        self.child.id()
    }
    pub fn helper_is_running(&mut self) -> Result<bool, Error> {
        let exit = self
            .child
            .try_wait()
            .map_err(|_| error("extraction child state unavailable"))?;
        if exit.is_some() {
            self.reaped = true;
        }
        Ok(exit.is_none())
    }
    /// Timeout means no new event, not helper termination. Finalization must still succeed.
    pub fn next_event(&mut self, wait: Duration) -> Result<Option<ExtractionEvent>, Error> {
        if self.failed {
            return Err(error(
                "extraction task failed; provisional text invalidated",
            ));
        }
        let result = self.next_event_inner(wait);
        if result.is_err() {
            self.failed = true;
            self.text.clear();
            self.chars = 0;
            self.pages.clear();
            self.source = None;
            self.complete = None;
            self.job.terminate();
        }
        result
    }
    fn next_event_inner(&mut self, wait: Duration) -> Result<Option<ExtractionEvent>, Error> {
        self.control.checkpoint()?;
        if self.eof {
            return Ok(None);
        }
        let result = self
            .receiver
            .as_ref()
            .ok_or_else(|| error("extraction stream unavailable"))?
            .recv_timeout(wait);
        let frame = match result {
            Ok(frame) => frame?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.control.checkpoint()?;
                return Ok(None);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.control.checkpoint()?;
                self.eof = true;
                return Ok(None);
            }
        };
        self.control.checkpoint()?;
        self.output_bytes += frame.bytes;
        self.accept(&frame.event)?;
        Ok(Some(frame.event))
    }
    fn accept(&mut self, event: &ExtractionEvent) -> Result<(), Error> {
        if self.complete.is_some() {
            return Err(error("extraction data after terminal event"));
        }
        if let ExtractionEvent::Failed { code } = event {
            return Err(error(match code {
                HelperFailure::OcrRequired => "PDF has no extractable text; OCR required",
                HelperFailure::ResourceLimit => "extraction helper budget exceeded",
                HelperFailure::SourceUnavailable => "extraction helper source unavailable",
            }));
        }
        match event {
            ExtractionEvent::Source { source } => {
                if self.source.is_some()
                    || source.requested_source != self.expected_source
                    || source.final_source.len() > 4096
                    || source.media_type.len() > 512
                    || source.final_source.is_empty()
                    || source.raw_bytes == 0
                    || source.raw_bytes > self.limits.max_bytes
                    || !valid_sha(&source.raw_sha256)
                {
                    return Err(error("extraction source identity rejected"));
                }
                self.source = Some(source.clone());
            }
            ExtractionEvent::Chunk {
                page,
                total_pages,
                start,
                text,
                final_chunk,
            } => {
                let source = self
                    .source
                    .as_ref()
                    .ok_or_else(|| error("extraction chunk without source"))?;
                let pdf = source
                    .media_type
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/pdf");
                if page.is_some() != pdf
                    || *total_pages > self.limits.max_pages
                    || (pdf && *total_pages == 0)
                    || (!pdf && *total_pages != 0)
                    || text.chars().count() > CHUNK_CHARS
                {
                    return Err(error("extraction chunk metadata rejected"));
                }
                if self.total_pages.is_some_and(|old| old != *total_pages) {
                    return Err(error("extraction page count changed"));
                }
                self.total_pages = Some(*total_pages);
                if self.block.is_none() {
                    if !pdf && self.blocks != 0 {
                        return Err(error("extraction repeated non-PDF block"));
                    }
                    if let Some(range) = page {
                        let expected = self.chars + if self.pages.is_empty() { 0 } else { 2 };
                        if range.number as usize != self.pages.len() + 1
                            || range.number as usize > *total_pages
                            || range.start != expected
                            || range.end < range.start
                            || range.end > self.limits.max_text_chars
                            || range.has_text != (range.end > range.start)
                        {
                            return Err(error("extraction page range rejected"));
                        }
                        if !self.pages.is_empty() {
                            self.text.push_str("\n\n");
                            self.chars += 2;
                        }
                    }
                    self.block = Some(Block { page: page.clone() });
                }
                if self.block.as_ref().is_none_or(|b| b.page != *page) || *start != self.chars {
                    return Err(error("extraction chunk order rejected"));
                }
                self.chars += text.chars().count();
                if self.chars > self.limits.max_text_chars {
                    return Err(error("extraction text budget exceeded"));
                }
                self.text.push_str(text);
                if *final_chunk {
                    if let Some(range) = page {
                        if self.chars != range.end {
                            return Err(error("extraction page text length rejected"));
                        }
                        self.pages.push(range.clone());
                    }
                    self.blocks += 1;
                    self.block = None;
                }
            }
            ExtractionEvent::Complete {
                provenance,
                pages,
                warnings,
            } => {
                let source = self
                    .source
                    .as_ref()
                    .ok_or_else(|| error("extraction terminal without source"))?;
                if self.block.is_some()
                    || self.blocks == 0
                    || pages != &self.pages
                    || pages.len() != self.total_pages.unwrap_or(0)
                    || warnings.len() > self.limits.max_pages + 1
                    || warnings.iter().any(|w| w.len() > 1024)
                    || !matches_source(source, provenance)
                {
                    return Err(error("extraction terminal identity rejected"));
                }
                self.complete = Some((provenance.clone(), warnings.clone()));
            }
            ExtractionEvent::Failed { .. } => unreachable!(),
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<(Document, IsolationReport), Error> {
        if self.failed {
            return Err(error(
                "extraction task failed; provisional text invalidated",
            ));
        }
        while !self.eof {
            self.next_event(Duration::from_millis(50))?;
        }
        loop {
            self.control.checkpoint()?;
            if let Some(exit) = self
                .child
                .try_wait()
                .map_err(|_| error("extraction child state unavailable"))?
            {
                self.reaped = true;
                if !exit.success() {
                    return Err(error(
                        "extraction helper exited unsuccessfully; partial output rejected",
                    ));
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let (provenance, warnings) = self
            .complete
            .take()
            .ok_or_else(|| error("extraction missing successful terminal event"))?;
        let memory = self.job.memory()?;
        let report = IsolationReport {
            helper_pid: self.child.id(),
            process_commit_limit_bytes: memory.limit,
            peak_process_commit_bytes: memory.peak,
            output_bytes: self.output_bytes,
            helper_exit_confirmed: true,
            kill_on_close: true,
        };
        let document = Document::from_verified_extraction(
            std::mem::take(&mut self.text),
            provenance,
            std::mem::take(&mut self.pages),
            warnings,
            &self.limits,
        )?;
        self.control.checkpoint()?;
        Ok((document, report))
    }
}
impl Drop for ExtractionTask {
    fn drop(&mut self) {
        self.receiver.take();
        if !self.reaped {
            self.job.terminate();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.interrupt.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
fn valid_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn matches_source(source: &AcquiredSource, provenance: &Provenance) -> bool {
    provenance.requested_source == source.requested_source
        && provenance.final_source == source.final_source
        && provenance.media_type == source.media_type
        && provenance.raw_bytes == source.raw_bytes
        && provenance.raw_sha256 == source.raw_sha256
}

impl Document {
    fn from_verified_extraction(
        text: String,
        provenance: Provenance,
        pages: Vec<PageRange>,
        warnings: Vec<String>,
        limits: &Limits,
    ) -> Result<Self, Error> {
        if text.trim().is_empty()
            || text.chars().count() > limits.max_text_chars
            || provenance.schema_version != 1
            || provenance.extractor != EXTRACTOR
            || provenance.text_sha256 != sha(text.as_bytes())
            || !valid_sha(&provenance.raw_sha256)
            || provenance.charset.as_ref().is_some_and(|c| c.len() > 64)
        {
            return Err(error("extraction final text identity rejected"));
        }
        let identity = serde_json::to_vec(&(
            EXTRACTOR,
            &provenance.final_source,
            &provenance.raw_sha256,
            &provenance.text_sha256,
        ))
        .map_err(error)?;
        let media = provenance
            .media_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let expected = match media.as_str() {
            "application/pdf" => crate::Format::Pdf,
            "text/html" | "application/xhtml+xml" => crate::Format::Html,
            "text/plain" | "text/markdown" | "text/csv" => crate::Format::Text,
            _ => return Err(error("extraction final media rejected")),
        };
        if provenance.format != expected
            || provenance.snapshot_id != sha(&identity)
            || (provenance.format == crate::Format::Pdf) == pages.is_empty()
        {
            return Err(error("extraction final snapshot rejected"));
        }
        Ok(Document::from_extracted_parts(
            text, provenance, pages, warnings,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_checksums_and_acquired_version_are_separate_requirements() {
        let limits = Limits::default();
        let original =
            Document::from_bytes("synthetic", b"Source version one.", "text/plain", &limits)
                .unwrap();
        let base = original.provenance().clone();
        let source = AcquiredSource {
            requested_source: base.requested_source.clone(),
            final_source: base.final_source.clone(),
            media_type: base.media_type.clone(),
            raw_bytes: base.raw_bytes,
            raw_sha256: base.raw_sha256.clone(),
        };
        assert!(matches_source(&source, &base));
        assert!(Document::from_verified_extraction(
            "Changed text".into(),
            base.clone(),
            vec![],
            vec![],
            &limits
        )
        .is_err());
        let mut signed = base.clone();
        signed.raw_sha256 = "0".repeat(64);
        signed.snapshot_id = sha(&serde_json::to_vec(&(
            EXTRACTOR,
            &signed.final_source,
            &signed.raw_sha256,
            &signed.text_sha256,
        ))
        .unwrap());
        // A self-consistent re-signed hash cannot replace the raw version acquired
        // before the first provisional page. Hashes are not source authentication.
        assert!(Document::from_verified_extraction(
            original.text().into(),
            signed.clone(),
            vec![],
            vec![],
            &limits
        )
        .is_ok());
        assert!(!matches_source(&source, &signed));
        let mut forged = base.clone();
        forged.snapshot_id = "0".repeat(64);
        assert!(Document::from_verified_extraction(
            original.text().into(),
            forged,
            vec![],
            vec![],
            &limits
        )
        .is_err());
    }
}
