//! Snapshot-bound evidence references. Hashes provide integrity, not signatures.
use crate::document::{error, sha};
use crate::{Document, Error, Evidence, Provenance, Unit};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_REFERENCES: usize = 512;
const MAX_CONTEXT: usize = 65_536;
const MAX_PACK_CHARS: usize = 262_144;
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Compact UTF-8 JSON, recursively sorted object keys. This payload has only
/// integer numbers. Hashes must not depend on serde_json's preserve_order feature.
pub(crate) fn canonical_hash<T: Serialize>(value: &T) -> Result<String, Error> {
    fn ordered(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => {
                let sorted: BTreeMap<_, _> =
                    object.into_iter().map(|(k, v)| (k, ordered(v))).collect();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(array) => {
                serde_json::Value::Array(array.into_iter().map(ordered).collect())
            }
            other => other,
        }
    }
    Ok(sha(&serde_json::to_vec(&ordered(
        serde_json::to_value(value).map_err(error)?,
    ))
    .map_err(error)?))
}
fn hash_shape(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn source_bounds(source: &Provenance) -> bool {
    source.schema_version == 1
        && !source.final_source.is_empty()
        && source.final_source.len() <= 8192
        && source.requested_source.len() <= 8192
        && source.media_type.len() <= 512
        && source.extractor.len() <= 128
        && source.charset.as_ref().is_none_or(|v| v.len() <= 64)
        && source.raw_bytes > 0
        && source.raw_bytes <= 64 * 1024 * 1024
        && hash_shape(&source.raw_sha256)
        && hash_shape(&source.text_sha256)
        && hash_shape(&source.snapshot_id)
}

#[derive(Clone, Copy, Debug)]
pub struct EvidenceLimits {
    pub max_sources: usize,
    pub max_retained_chars: usize,
    pub max_references: usize,
    pub max_context_chars: usize,
    pub max_pack_entries: usize,
    pub max_pack_chars: usize,
}
impl Default for EvidenceLimits {
    fn default() -> Self {
        Self {
            max_sources: 16,
            max_retained_chars: 8_000_000,
            max_references: 256,
            max_context_chars: 16_384,
            max_pack_entries: 32,
            max_pack_chars: 65_536,
        }
    }
}
impl EvidenceLimits {
    pub(crate) fn validate(self) -> Result<(), Error> {
        if !(1..=64).contains(&self.max_sources)
            || !(1..=64_000_000).contains(&self.max_retained_chars)
            || !(1..=MAX_REFERENCES).contains(&self.max_references)
            || !(1..=MAX_CONTEXT).contains(&self.max_context_chars)
            || !(1..=32).contains(&self.max_pack_entries)
            || !(1..=MAX_PACK_CHARS).contains(&self.max_pack_chars)
        {
            return Err(error("invalid evidence session limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expansion {
    pub before: usize,
    pub after: usize,
    pub unit: Unit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceRecord {
    pub schema_version: u32,
    pub session_id: String,
    pub ordinal: u64,
    pub document_index: usize,
    pub snapshot_id: String,
    pub query_sha256: String,
    pub anchor_start: usize,
    pub anchor_end: usize,
    pub start: usize,
    pub end: usize,
    pub quote_sha256: String,
    pub parent_reference: Option<String>,
    pub expansion: Option<Expansion>,
}
impl ReferenceRecord {
    pub fn reference_id(&self) -> Result<String, Error> {
        canonical_hash(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceView {
    pub reference: String,
    pub record: ReferenceRecord,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackPayload {
    pub session_id: String,
    pub documents: Vec<Provenance>,
    pub entries: Vec<EvidenceView>,
    /// Records only; ancestor text is not duplicated in the exported audit trail.
    pub ancestors: Vec<ReferenceRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidencePack {
    pub schema_version: u32,
    pub payload: PackPayload,
    pub sha256: String,
}

pub struct EvidenceSession {
    id: String,
    documents: Vec<Arc<Document>>,
    references: BTreeMap<String, ReferenceRecord>,
    next_ordinal: u64,
    limits: EvidenceLimits,
}
impl EvidenceSession {
    /// Explicit offline continuation, not a grant and not resurrection of a
    /// worker handle. Caller supplies the same source snapshots in pack order;
    /// every quote/page/ancestor/expansion is replayed before references exist.
    /// Different branches are separate artifacts; no concurrent merge is implied.
    pub fn restore(
        pack: &EvidencePack,
        documents: Vec<Arc<Document>>,
        limits: EvidenceLimits,
    ) -> Result<Self, Error> {
        let mut session = Self::new(documents, limits)?;
        pack.verify_against(
            &session
                .documents
                .iter()
                .map(Arc::as_ref)
                .collect::<Vec<_>>(),
        )?;
        if session.documents.len() != pack.payload.documents.len()
            || session
                .documents
                .iter()
                .zip(&pack.payload.documents)
                .any(|(doc, expected)| doc.provenance().snapshot_id != expected.snapshot_id)
            || pack.payload.entries.len() > limits.max_pack_entries
            || pack
                .payload
                .entries
                .iter()
                .map(|v| v.evidence.text.chars().count())
                .sum::<usize>()
                > limits.max_pack_chars
        {
            return Err(error("restored pack source order or export budget differs"));
        }
        let records = pack.records()?;
        if records.len() > limits.max_references
            || records
                .values()
                .any(|r| r.end - r.start > limits.max_context_chars)
        {
            return Err(error("restored reference budget exceeded"));
        }
        let next = records
            .values()
            .map(|r| r.ordinal)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| error("restored ordinal exhausted"))?;
        session.id = pack.payload.session_id.clone();
        session.references = records.into_iter().map(|(id, r)| (id, r.clone())).collect();
        session.next_ordinal = next;
        Ok(session)
    }
    pub fn new(documents: Vec<Arc<Document>>, limits: EvidenceLimits) -> Result<Self, Error> {
        limits.validate()?;
        if documents.is_empty() || documents.len() > limits.max_sources {
            return Err(error("evidence source budget exceeded or empty"));
        }
        let mut chars = 0usize;
        let mut identities = BTreeSet::new();
        for doc in &documents {
            chars = chars
                .checked_add(doc.text().chars().count())
                .ok_or_else(|| error("source character overflow"))?;
            if chars > limits.max_retained_chars || !source_bounds(doc.provenance()) {
                return Err(error("evidence retained-source budget exceeded"));
            }
            if !identities.insert(doc.provenance().snapshot_id.clone()) {
                return Err(error("duplicate evidence source snapshot"));
            }
        }
        let sequence = SESSION_SEQUENCE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| x.checked_add(1))
            .map_err(|_| error("session sequence exhausted"))?;
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_nanos()
            .to_string();
        let id = format!(
            "session-{}",
            canonical_hash(&(std::process::id(), sequence, time, identities))?
        );
        Ok(Self {
            id,
            documents,
            references: BTreeMap::new(),
            next_ordinal: 1,
            limits,
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn documents(&self) -> &[Arc<Document>] {
        &self.documents
    }
    pub fn reference_count(&self) -> usize {
        self.references.len()
    }

    pub fn register(&mut self, query: &str, evidence: &Evidence) -> Result<EvidenceView, Error> {
        self.register_many(query, std::slice::from_ref(evidence))?
            .pop()
            .ok_or_else(|| error("missing registered reference"))
    }
    /// Atomic admission: no partial references if any quote or budget is invalid.
    pub fn register_many(
        &mut self,
        query: &str,
        evidence: &[Evidence],
    ) -> Result<Vec<EvidenceView>, Error> {
        if query.trim().is_empty() || query.len() > 4096 {
            return Err(error("invalid evidence query"));
        }
        self.check_capacity(evidence.len())?;
        let mut views = Vec::new();
        for (position, hit) in evidence.iter().enumerate() {
            let document_index = self
                .documents
                .iter()
                .position(|d| d.provenance().snapshot_id == hit.snapshot_id)
                .ok_or_else(|| error("evidence belongs to another session source"))?;
            let checked = self.documents[document_index].expand(
                hit,
                0,
                0,
                Unit::Characters,
                self.limits.max_context_chars,
            )?;
            let record = ReferenceRecord {
                schema_version: 1,
                session_id: self.id.clone(),
                ordinal: self.next_ordinal + position as u64,
                document_index,
                snapshot_id: checked.snapshot_id.clone(),
                query_sha256: sha(query.as_bytes()),
                anchor_start: checked.start,
                anchor_end: checked.end,
                start: checked.start,
                end: checked.end,
                quote_sha256: sha(checked.text.as_bytes()),
                parent_reference: None,
                expansion: None,
            };
            views.push(EvidenceView {
                reference: record.reference_id()?,
                record,
                evidence: checked,
            });
        }
        self.commit(&views);
        Ok(views)
    }
    fn check_capacity(&self, count: usize) -> Result<(), Error> {
        if count == 0
            || count
                > self
                    .limits
                    .max_references
                    .saturating_sub(self.references.len())
            || self.next_ordinal.checked_add(count as u64).is_none()
        {
            return Err(error("evidence reference budget exceeded"));
        }
        Ok(())
    }
    fn commit(&mut self, views: &[EvidenceView]) {
        for view in views {
            self.references
                .insert(view.reference.clone(), view.record.clone());
        }
        self.next_ordinal += views.len() as u64;
    }
    pub fn resolve(&self, reference: &str) -> Result<EvidenceView, Error> {
        let record = self
            .references
            .get(reference)
            .ok_or_else(|| error("unknown evidence reference in this session"))?;
        let evidence = self.documents[record.document_index].read_span(
            record.start,
            record.end,
            self.limits.max_context_chars,
        )?;
        if sha(evidence.text.as_bytes()) != record.quote_sha256
            || record.reference_id()? != reference
        {
            return Err(error("evidence reference integrity failed"));
        }
        Ok(EvidenceView {
            reference: reference.into(),
            record: record.clone(),
            evidence,
        })
    }
    pub fn expand(
        &mut self,
        reference: &str,
        before: usize,
        after: usize,
        unit: Unit,
    ) -> Result<EvidenceView, Error> {
        self.check_capacity(1)?;
        if before > 65_536 || after > 65_536 {
            return Err(error("expansion count budget exceeded"));
        }
        let parent = self.resolve(reference)?;
        let evidence = self.documents[parent.record.document_index].expand(
            &parent.evidence,
            before,
            after,
            unit,
            self.limits.max_context_chars,
        )?;
        let record = ReferenceRecord {
            ordinal: self.next_ordinal,
            start: evidence.start,
            end: evidence.end,
            quote_sha256: sha(evidence.text.as_bytes()),
            parent_reference: Some(reference.into()),
            expansion: Some(Expansion {
                before,
                after,
                unit,
            }),
            ..parent.record
        };
        let view = EvidenceView {
            reference: record.reference_id()?,
            record,
            evidence,
        };
        self.commit(std::slice::from_ref(&view));
        Ok(view)
    }
    pub fn export(&self, references: &[String]) -> Result<EvidencePack, Error> {
        if references.is_empty() || references.len() > self.limits.max_pack_entries {
            return Err(error("pack entry budget exceeded"));
        }
        let selected: BTreeSet<_> = references.iter().cloned().collect();
        if selected.len() != references.len() {
            return Err(error("duplicate pack reference"));
        }
        let mut total = 0usize;
        let mut entries = Vec::new();
        let mut ancestors = BTreeMap::new();
        for reference in references {
            let record = self
                .references
                .get(reference)
                .ok_or_else(|| error("unknown pack reference"))?;
            total += record.end - record.start;
            if total > self.limits.max_pack_chars {
                return Err(error("pack quote budget exceeded"));
            }
            entries.push(self.resolve(reference)?);
            let mut parent = record.parent_reference.clone();
            while let Some(id) = parent {
                let record = self
                    .references
                    .get(&id)
                    .ok_or_else(|| error("missing reference predecessor"))?;
                parent = record.parent_reference.clone();
                if !selected.contains(&id) {
                    ancestors.insert(id, record.clone());
                }
            }
        }
        let payload = PackPayload {
            session_id: self.id.clone(),
            documents: self
                .documents
                .iter()
                .map(|d| d.provenance().clone())
                .collect(),
            entries,
            ancestors: ancestors.into_values().collect(),
        };
        let mut pack = EvidencePack {
            schema_version: 1,
            payload,
            sha256: String::new(),
        };
        pack.sha256 = pack.calculate_sha256()?;
        pack.verify_integrity()?;
        Ok(pack)
    }
}

impl EvidencePack {
    pub fn calculate_sha256(&self) -> Result<String, Error> {
        canonical_hash(&(self.schema_version, &self.payload))
    }
    fn records(&self) -> Result<BTreeMap<String, &ReferenceRecord>, Error> {
        let mut all = BTreeMap::new();
        for record in self
            .payload
            .entries
            .iter()
            .map(|e| &e.record)
            .chain(&self.payload.ancestors)
        {
            if all.insert(record.reference_id()?, record).is_some() {
                return Err(error("duplicate pack audit record"));
            }
        }
        Ok(all)
    }
    pub fn verify_integrity(&self) -> Result<(), Error> {
        let payload = &self.payload;
        if self.schema_version != 1
            || payload.session_id.is_empty()
            || payload.session_id.len() > 128
            || payload.documents.is_empty()
            || payload.documents.len() > 64
            || payload.entries.is_empty()
            || payload.entries.len() > 32
            || payload.ancestors.len() > MAX_REFERENCES
            || !hash_shape(&self.sha256)
        {
            return Err(error("invalid pack schema or bounds"));
        }
        if payload
            .entries
            .iter()
            .map(|e| e.evidence.text.chars().count())
            .sum::<usize>()
            > MAX_PACK_CHARS
        {
            return Err(error("pack quote budget exceeded"));
        }
        let mut sources = BTreeSet::new();
        for source in &payload.documents {
            if !source_bounds(source)
                || !sources.insert(&source.snapshot_id)
                || sha(&serde_json::to_vec(&(
                    &source.extractor,
                    &source.final_source,
                    &source.raw_sha256,
                    &source.text_sha256,
                ))
                .map_err(error)?)
                    != source.snapshot_id
            {
                return Err(error("pack source identity or metadata bounds mismatch"));
            }
        }
        for record in payload
            .entries
            .iter()
            .map(|e| &e.record)
            .chain(&payload.ancestors)
        {
            if record.session_id.len() > 128
                || !hash_shape(&record.snapshot_id)
                || !hash_shape(&record.query_sha256)
                || !hash_shape(&record.quote_sha256)
                || record
                    .parent_reference
                    .as_ref()
                    .is_some_and(|v| !hash_shape(v))
            {
                return Err(error("pack record metadata budget exceeded"));
            }
        }
        if payload.entries.iter().any(|entry| {
            !hash_shape(&entry.reference)
                || entry.evidence.source.len() > 8192
                || !hash_shape(&entry.evidence.snapshot_id)
                || entry.evidence.pages.len() > 5000
        }) {
            return Err(error("pack entry metadata budget exceeded"));
        }
        if self.calculate_sha256()? != self.sha256 {
            return Err(error("pack SHA256 mismatch"));
        }
        let all = self.records()?;
        if all.len() > MAX_REFERENCES {
            return Err(error("pack audit budget exceeded"));
        }
        let mut ordinals = BTreeSet::new();
        for (id, record) in &all {
            if record.schema_version != 1
                || record.session_id != payload.session_id
                || record.ordinal == 0
                || !ordinals.insert(record.ordinal)
                || !hash_shape(&record.query_sha256)
                || !hash_shape(&record.quote_sha256)
                || record.anchor_start >= record.anchor_end
                || record.start > record.anchor_start
                || record.end < record.anchor_end
                || record.end - record.start > MAX_CONTEXT
            {
                return Err(error("invalid pack reference record"));
            }
            let source = payload
                .documents
                .get(record.document_index)
                .ok_or_else(|| error("unknown pack document"))?;
            if record.snapshot_id != source.snapshot_id {
                return Err(error("pack source identity mismatch"));
            }
            match (&record.parent_reference, &record.expansion) {
                (None, None)
                    if record.start == record.anchor_start && record.end == record.anchor_end => {}
                (Some(parent), Some(operation)) => {
                    let previous = all
                        .get(parent)
                        .ok_or_else(|| error("pack predecessor missing"))?;
                    if parent == id
                        || previous.ordinal >= record.ordinal
                        || previous.document_index != record.document_index
                        || previous.query_sha256 != record.query_sha256
                        || previous.anchor_start != record.anchor_start
                        || previous.anchor_end != record.anchor_end
                        || record.start > previous.start
                        || record.end < previous.end
                        || operation.before > 65_536
                        || operation.after > 65_536
                    {
                        return Err(error("invalid expansion lineage"));
                    }
                }
                _ => return Err(error("invalid reference origin or expansion")),
            }
        }
        for entry in &payload.entries {
            let record = &entry.record;
            let source = &payload.documents[record.document_index];
            if entry.reference != record.reference_id()?
                || entry.evidence.snapshot_id != record.snapshot_id
                || entry.evidence.source != source.final_source
                || entry.evidence.start != record.start
                || entry.evidence.end != record.end
                || entry.evidence.text.chars().count() != record.end - record.start
                || sha(entry.evidence.text.as_bytes()) != record.quote_sha256
                || entry.evidence.pages.contains(&0)
                || entry.evidence.pages.windows(2).any(|p| p[0] >= p[1])
            {
                return Err(error("pack quote integrity mismatch"));
            }
        }
        let mut reachable = BTreeSet::new();
        for entry in &payload.entries {
            let mut reference = Some(entry.reference.clone());
            while let Some(id) = reference {
                if !reachable.insert(id.clone()) {
                    break;
                }
                reference = all
                    .get(&id)
                    .ok_or_else(|| error("missing audit record"))?
                    .parent_reference
                    .clone();
            }
        }
        if reachable.len() != all.len() {
            return Err(error("pack contains detached audit records"));
        }
        Ok(())
    }
    /// Integrity alone is not authentication: compare every selected/ancestor
    /// quote and replay every expansion against the same source snapshot.
    pub fn verify_against(&self, documents: &[&Document]) -> Result<(), Error> {
        self.verify_integrity()?;
        let all = self.records()?;
        let mut actual = BTreeMap::new();
        for (id, record) in &all {
            let source = &self.payload.documents[record.document_index];
            let doc = documents
                .iter()
                .find(|d| d.provenance().snapshot_id == record.snapshot_id)
                .ok_or_else(|| error("matching source snapshot required for pack verification"))?;
            let provenance = doc.provenance();
            if provenance.raw_sha256 != source.raw_sha256
                || provenance.text_sha256 != source.text_sha256
                || provenance.raw_bytes != source.raw_bytes
                || provenance.format != source.format
                || provenance.extractor != source.extractor
                || provenance.final_source != source.final_source
            {
                return Err(error("pack metadata differs from source"));
            }
            let quote = doc.read_span(record.start, record.end, MAX_CONTEXT)?;
            if sha(quote.text.as_bytes()) != record.quote_sha256 {
                return Err(error("pack quote differs from source snapshot"));
            }
            actual.insert(id.clone(), quote);
        }
        for (id, record) in &all {
            let doc = documents
                .iter()
                .find(|d| d.provenance().snapshot_id == record.snapshot_id)
                .ok_or_else(|| error("missing source snapshot"))?;
            if let (Some(parent), Some(operation)) = (&record.parent_reference, &record.expansion) {
                let replay = doc.expand(
                    &actual[parent],
                    operation.before,
                    operation.after,
                    operation.unit,
                    MAX_CONTEXT,
                )?;
                if replay.start != record.start || replay.end != record.end {
                    return Err(error("expansion does not match source replay"));
                }
            }
            if let Some(entry) = self.payload.entries.iter().find(|e| e.reference == *id) {
                if entry.evidence.pages != actual[id].pages {
                    return Err(error("pack page reference differs from source"));
                }
            }
        }
        Ok(())
    }
}
