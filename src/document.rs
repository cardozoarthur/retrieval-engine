use crate::{acquisition, Snapshot, Unit};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const EXTRACTOR: &str = "retrieval-text/1";

#[derive(Debug)]
pub struct Error(pub String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
pub(crate) fn error(value: impl fmt::Display) -> Error {
    Error(value.to_string())
}
pub(crate) fn sha(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Source {
    Local(PathBuf),
    Remote(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_text_chars: usize,
    pub max_pages: usize,
    pub network_timeout: Duration,
    pub max_redirects: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 8 * 1024 * 1024,
            max_text_chars: 2_000_000,
            max_pages: 500,
            network_timeout: Duration::from_secs(30),
            max_redirects: 5,
        }
    }
}
impl Limits {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.max_bytes == 0
            || self.max_bytes > 64 * 1024 * 1024
            || self.max_text_chars == 0
            || self.max_text_chars > 16_000_000
            || self.max_pages == 0
            || self.max_pages > 5000
            || self.network_timeout.is_zero()
            || self.network_timeout > Duration::from_secs(300)
            || self.max_redirects > 10
        {
            return Err(error("invalid limits: bytes<=64MiB, chars<=16M, pages<=5000, timeout<=300s, redirects<=10"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Text,
    Html,
    Pdf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub schema_version: u32,
    pub requested_source: String,
    pub final_source: String,
    pub fetched_unix_seconds: u64,
    pub media_type: String,
    pub format: Format,
    pub raw_bytes: usize,
    pub raw_sha256: String,
    pub text_sha256: String,
    pub snapshot_id: String,
    pub extractor: String,
    pub charset: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRange {
    pub number: u32,
    pub start: usize,
    pub end: usize,
    pub has_text: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub snapshot_id: String,
    pub source: String,
    pub start: usize,
    pub end: usize,
    pub pages: Vec<u32>,
    pub text: String,
}

pub struct Document {
    snapshot: Snapshot,
    provenance: Provenance,
    pages: Vec<PageRange>,
    warnings: Vec<String>,
}

/// Wall times for source acquisition and the subsequent extraction/snapshot
/// construction, including hashes. Not CPU time or pure parser-only timing.
#[derive(Clone, Debug, Serialize)]
pub struct LoadTimings {
    pub acquisition_us: u128,
    pub extraction_snapshot_us: u128,
}

/// Provisional page/segment callback. No reference is final until extraction succeeds.
/// In-process observers do not isolate native parser allocations or blocked I/O.
pub type PageObserver<'a> = dyn FnMut(&str, Option<&PageRange>, usize) -> Result<(), Error> + 'a;

impl Document {
    #[cfg(windows)]
    pub(crate) fn from_extracted_parts(
        text: String,
        provenance: Provenance,
        pages: Vec<PageRange>,
        warnings: Vec<String>,
    ) -> Self {
        Self {
            snapshot: Snapshot::new(provenance.final_source.clone(), text),
            provenance,
            pages,
            warnings,
        }
    }
    pub fn load(source: Source, limits: &Limits) -> Result<Self, Error> {
        limits.validate()?;
        let acquired = acquisition::acquire(source, limits)?;
        Self::build(acquired, limits)
    }
    pub fn load_measured(source: Source, limits: &Limits) -> Result<(Self, LoadTimings), Error> {
        limits.validate()?;
        let started = Instant::now();
        let acquired = acquisition::acquire(source, limits)?;
        let acquisition_us = started.elapsed().as_micros();
        let started = Instant::now();
        let doc = Self::build(acquired, limits)?;
        Ok((
            doc,
            LoadTimings {
                acquisition_us,
                extraction_snapshot_us: started.elapsed().as_micros(),
            },
        ))
    }
    pub fn load_observed(
        source: Source,
        limits: &Limits,
        observer: &mut PageObserver<'_>,
    ) -> Result<Self, Error> {
        limits.validate()?;
        let acquired = acquisition::acquire(source, limits)?;
        Self::build_observed(acquired, limits, observer)
    }
    /// Parses caller-provided bytes without network or filesystem effects.
    pub fn from_bytes(
        source: &str,
        bytes: &[u8],
        mime: &str,
        limits: &Limits,
    ) -> Result<Self, Error> {
        limits.validate()?;
        if bytes.len() > limits.max_bytes {
            return Err(error("source byte budget exceeded"));
        }
        Self::build(
            acquisition::Acquired {
                requested: source.into(),
                final_source: source.into(),
                mime: mime.into(),
                bytes: bytes.to_vec(),
            },
            limits,
        )
    }
    fn build(acquired: acquisition::Acquired, limits: &Limits) -> Result<Self, Error> {
        Self::build_observed(acquired, limits, &mut |_, _, _| Ok(()))
    }
    pub(crate) fn build_observed(
        acquired: acquisition::Acquired,
        limits: &Limits,
        observer: &mut PageObserver<'_>,
    ) -> Result<Self, Error> {
        if acquired.bytes.len() > limits.max_bytes {
            return Err(error("source byte budget exceeded"));
        }
        let extracted = acquisition::extract(&acquired.bytes, &acquired.mime, limits, observer)?;
        let raw_sha256 = sha(&acquired.bytes);
        let text_sha256 = sha(extracted.text.as_bytes());
        // Length-delimited JSON avoids ambiguous concatenation. Source and extractor
        // are part of identity; a different origin never inherits evidence.
        let identity =
            serde_json::to_vec(&(EXTRACTOR, &acquired.final_source, &raw_sha256, &text_sha256))
                .map_err(error)?;
        let provenance = Provenance {
            schema_version: 1,
            requested_source: acquired.requested,
            final_source: acquired.final_source.clone(),
            fetched_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(error)?
                .as_secs(),
            media_type: acquired.mime,
            format: extracted.format,
            raw_bytes: acquired.bytes.len(),
            raw_sha256,
            text_sha256,
            snapshot_id: sha(&identity),
            extractor: EXTRACTOR.into(),
            charset: extracted.charset,
        };
        Ok(Self {
            snapshot: Snapshot::new(acquired.final_source, extracted.text),
            provenance,
            pages: extracted.pages,
            warnings: extracted.warnings,
        })
    }
    pub fn text(&self) -> &str {
        self.snapshot.text()
    }
    /// Adapter-only logical origin for an explicitly selected artifact. A URN
    /// identifies project+artifact, never claims HTTP acquisition or authority.
    /// Call only after guarded extraction and verification of the selected bytes.
    #[cfg(windows)]
    pub(crate) fn selected_origin(mut self, origin: String) -> Result<Self, Error> {
        if !origin.starts_with("urn:retrieval:selected:")
            || origin.len() > 512
            || !origin.is_ascii()
            || origin.chars().any(char::is_control)
        {
            return Err(error("invalid selected artifact identity"));
        }
        self.provenance.requested_source = origin.clone();
        self.provenance.final_source = origin.clone();
        self.provenance.snapshot_id = sha(&serde_json::to_vec(&(
            EXTRACTOR,
            &origin,
            &self.provenance.raw_sha256,
            &self.provenance.text_sha256,
        ))
        .map_err(error)?);
        self.snapshot.source = origin;
        Ok(self)
    }
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }
    pub fn pages(&self) -> &[PageRange] {
        &self.pages
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn read_span(&self, start: usize, end: usize, max_chars: usize) -> Result<Evidence, Error> {
        let span = self
            .snapshot
            .expand((start, end), 0, 0, Unit::Characters, max_chars)
            .map_err(error)?;
        Ok(self.evidence(span.start, span.end, span.text))
    }
    pub fn find(&self, query: &str, max_hits: usize) -> Result<Vec<Evidence>, Error> {
        if max_hits == 0 || max_hits > 1000 {
            return Err(error("max_hits must be 1..=1000"));
        }
        if query.is_empty() {
            return Err(error("empty query"));
        }
        // Limit before collecting; large repeated documents must not allocate an
        // unbounded vector of matches. R00's lower-level API remains compatible.
        let mut scalar_start = 0;
        let mut byte_start = 0;
        self.text()
            .match_indices(query)
            .take(max_hits)
            .map(|(byte, text)| {
                scalar_start += self.text()[byte_start..byte].chars().count();
                let start = scalar_start;
                let end = start + text.chars().count();
                byte_start = byte;
                Ok(self.evidence(start, end, text.into()))
            })
            .collect()
    }
    pub(crate) fn evidence(&self, start: usize, end: usize, text: String) -> Evidence {
        Evidence {
            snapshot_id: self.provenance.snapshot_id.clone(),
            source: self.provenance.final_source.clone(),
            start,
            end,
            pages: self
                .pages
                .iter()
                .filter(|p| p.start < end && p.end > start)
                .map(|p| p.number)
                .collect(),
            text,
        }
    }
    pub fn expand(
        &self,
        hit: &Evidence,
        before: usize,
        after: usize,
        unit: Unit,
        max_chars: usize,
    ) -> Result<Evidence, Error> {
        if hit.snapshot_id != self.provenance.snapshot_id
            || hit.source != self.provenance.final_source
        {
            return Err(error("evidence belongs to another snapshot"));
        }
        let actual = self
            .snapshot
            .expand((hit.start, hit.end), 0, 0, Unit::Characters, max_chars)
            .map_err(error)?;
        if actual.text != hit.text
            || self.evidence(hit.start, hit.end, String::new()).pages != hit.pages
        {
            return Err(error("evidence does not match snapshot"));
        }
        let context = self
            .snapshot
            .expand((hit.start, hit.end), before, after, unit, max_chars)
            .map_err(error)?;
        Ok(self.evidence(context.start, context.end, context.text))
    }
}
