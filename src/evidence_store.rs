//! Derived evidence storage, independent of a consumer's canonical schema.
//! Namespace is selected by trusted host construction, never an LLM tool field.
//! Receipts are integrity records, not authorization, signatures or legal truth.
use crate::{document::error, evidence::canonical_hash, Document, Error, EvidencePack};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    SnapshotReplay,
    TrustedWorkerExport,
}
#[derive(Clone, Debug)]
pub struct VerifiedEvidence {
    pack: EvidencePack,
    method: VerificationMethod,
}
impl VerifiedEvidence {
    pub fn against_snapshots(pack: EvidencePack, documents: &[&Document]) -> Result<Self, Error> {
        pack.verify_against(documents)?;
        Ok(Self {
            pack,
            method: VerificationMethod::SnapshotReplay,
        })
    }
    #[cfg(windows)]
    pub(crate) fn from_worker(pack: EvidencePack) -> Result<Self, Error> {
        pack.verify_integrity()?;
        Ok(Self {
            pack,
            method: VerificationMethod::TrustedWorkerExport,
        })
    }
    pub fn pack(&self) -> &EvidencePack {
        &self.pack
    }
    pub fn method(&self) -> VerificationMethod {
        self.method
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReceipt {
    pub schema_version: u32,
    pub namespace: String,
    pub write_id: String,
    pub pack_sha256: String,
    pub payload_bytes: u64,
    pub verification: VerificationMethod,
    pub receipt_sha256: String,
}
fn identity(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b))
}
impl EvidenceReceipt {
    /// Constructor for adapters over an existing Rust bank. This only prepares
    /// the integrity receipt; the adapter must commit it with the exact payload.
    pub fn for_write(
        namespace: &str,
        write_id: &str,
        evidence: &VerifiedEvidence,
    ) -> Result<Self, Error> {
        if !identity(namespace) || !identity(write_id) {
            return Err(error("invalid host evidence namespace/write identity"));
        }
        evidence.pack.verify_integrity()?;
        let payload_bytes = serde_json::to_vec(&evidence.pack).map_err(error)?.len() as u64;
        if payload_bytes > 4 * 1024 * 1024 {
            return Err(error("evidence receipt payload exceeds version1 budget"));
        }
        let mut receipt = Self {
            schema_version: 1,
            namespace: namespace.into(),
            write_id: write_id.into(),
            pack_sha256: evidence.pack.sha256.clone(),
            payload_bytes,
            verification: evidence.method,
            receipt_sha256: String::new(),
        };
        receipt.receipt_sha256 = receipt.digest()?;
        Ok(receipt)
    }
    fn digest(&self) -> Result<String, Error> {
        canonical_hash(&(
            self.schema_version,
            &self.namespace,
            &self.write_id,
            &self.pack_sha256,
            self.payload_bytes,
            self.verification,
        ))
    }
    pub fn verify(&self, pack: &EvidencePack) -> Result<(), Error> {
        pack.verify_integrity()?;
        if self.schema_version != 1
            || !identity(&self.namespace)
            || !identity(&self.write_id)
            || self.pack_sha256 != pack.sha256
            || self.payload_bytes != serde_json::to_vec(pack).map_err(error)?.len() as u64
            || self.digest()? != self.receipt_sha256
        {
            return Err(error("evidence receipt integrity differs"));
        }
        Ok(())
    }
}
/// Consumers can implement their own transaction using their existing Rust bank.
/// put must bind the write ID to exact pack+verification, without canonical writes.
pub trait EvidenceStore {
    fn put(
        &mut self,
        write_id: &str,
        evidence: &VerifiedEvidence,
    ) -> Result<EvidenceReceipt, Error>;
    /// Replay verification is historical; reading checks bytes/integrity only.
    /// To expand after worker TTL, independently supply/revalidate the snapshot.
    fn get(&mut self, write_id: &str) -> Result<Option<(EvidenceReceipt, EvidencePack)>, Error>;
}

#[derive(Clone, Copy, Debug)]
pub struct StoreLimits {
    pub max_records: usize,
    pub max_payload_bytes: usize,
    pub max_total_payload_bytes: usize,
}
impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_records: 1024,
            max_payload_bytes: 4 * 1024 * 1024,
            max_total_payload_bytes: 64 * 1024 * 1024,
        }
    }
}
impl StoreLimits {
    pub fn validate(self) -> Result<(), Error> {
        if !(1..=65536).contains(&self.max_records)
            || !(1024..=4 * 1024 * 1024).contains(&self.max_payload_bytes)
            || self.max_total_payload_bytes < self.max_payload_bytes
            || self.max_total_payload_bytes > 1024 * 1024 * 1024
        {
            return Err(error("invalid evidence store budgets"));
        }
        Ok(())
    }
}

#[cfg(feature = "sqlite-store")]
mod sqlite {
    use super::*;
    use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
    use std::{fs::OpenOptions, path::Path, time::Duration};
    const APPLICATION_ID: i64 = 0x52455631;
    pub struct SqliteEvidenceStore {
        connection: Connection,
        namespace: String,
        limits: StoreLimits,
    }
    fn sql<T>(result: rusqlite::Result<T>) -> Result<T, Error> {
        result.map_err(|_| error("evidence SQLite operation failed"))
    }
    fn configuration(namespace: &str, limits: StoreLimits) -> Result<(), Error> {
        if !identity(namespace) {
            return Err(error("invalid host evidence namespace"));
        }
        limits.validate()
    }
    impl SqliteEvidenceStore {
        /// Creates a dedicated new DB. Refuses to initialize/modify any existing
        /// database, including a consumer's canonical bank.
        pub fn create(
            path: impl AsRef<Path>,
            namespace: &str,
            limits: StoreLimits,
        ) -> Result<Self, Error> {
            configuration(namespace, limits)?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path.as_ref())
                .map_err(|_| error("new evidence database already exists or is inaccessible"))?;
            let mut connection = sql(Connection::open(path))?;
            let transaction =
                sql(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
            sql(transaction.execute_batch(&format!("PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version=1; CREATE TABLE retrieval_evidence(namespace TEXT NOT NULL, write_id TEXT NOT NULL, pack_sha256 TEXT NOT NULL, payload BLOB NOT NULL, receipt BLOB NOT NULL, PRIMARY KEY(namespace,write_id));")))?;
            sql(transaction.commit())?;
            Self::configure(connection, namespace, limits)
        }
        /// Opens only this dedicated format. Namespace is a trusted host binding,
        /// not an authorization token or a browser-selected tenant.
        pub fn open(
            path: impl AsRef<Path>,
            namespace: &str,
            limits: StoreLimits,
        ) -> Result<Self, Error> {
            configuration(namespace, limits)?;
            let connection = sql(Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            ))?;
            let app: i64 = sql(connection.query_row("PRAGMA application_id", [], |r| r.get(0)))?;
            let version: i64 = sql(connection.query_row("PRAGMA user_version", [], |r| r.get(0)))?;
            if app != APPLICATION_ID || version != 1 {
                return Err(error("database is not the owned retrieval-evidence format"));
            }
            Self::configure(connection, namespace, limits)
        }
        fn configure(
            connection: Connection,
            namespace: &str,
            limits: StoreLimits,
        ) -> Result<Self, Error> {
            sql(connection.busy_timeout(Duration::from_secs(2)))?;
            sql(connection.pragma_update(None, "synchronous", "FULL"))?;
            Ok(Self {
                connection,
                namespace: namespace.into(),
                limits,
            })
        }
    }
    fn read(
        transaction: &Transaction<'_>,
        namespace: &str,
        id: &str,
        limits: StoreLimits,
    ) -> Result<Option<(EvidenceReceipt, EvidencePack)>, Error> {
        let lengths:Option<(i64,i64)>=sql(transaction.query_row("SELECT length(payload),length(receipt) FROM retrieval_evidence WHERE namespace=?1 AND write_id=?2",params![namespace,id],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
        let Some((payload_len, receipt_len)) = lengths else {
            return Ok(None);
        };
        if payload_len < 1
            || payload_len > limits.max_payload_bytes as i64
            || !(1..=8192).contains(&receipt_len)
        {
            return Err(error("stored evidence exceeds read budget"));
        }
        let (pack_sha,bytes,receipt):(String,Vec<u8>,Vec<u8>)=sql(transaction.query_row("SELECT pack_sha256,payload,receipt FROM retrieval_evidence WHERE namespace=?1 AND write_id=?2",params![namespace,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))))?;
        let pack: EvidencePack =
            serde_json::from_slice(&bytes).map_err(|_| error("invalid stored evidence pack"))?;
        let receipt: EvidenceReceipt = serde_json::from_slice(&receipt)
            .map_err(|_| error("invalid stored evidence receipt"))?;
        if receipt.namespace != namespace
            || receipt.write_id != id
            || pack.sha256 != pack_sha
            || receipt.payload_bytes != bytes.len() as u64
        {
            return Err(error("stored evidence binding differs"));
        }
        receipt.verify(&pack)?;
        Ok(Some((receipt, pack)))
    }
    impl EvidenceStore for SqliteEvidenceStore {
        fn put(
            &mut self,
            write_id: &str,
            evidence: &VerifiedEvidence,
        ) -> Result<EvidenceReceipt, Error> {
            if !identity(write_id) {
                return Err(error("invalid evidence write identity"));
            }
            evidence.pack.verify_integrity()?;
            let bytes = serde_json::to_vec(&evidence.pack).map_err(error)?;
            if bytes.len() > self.limits.max_payload_bytes {
                return Err(error("evidence payload exceeds storage budget"));
            }
            let transaction = sql(self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate))?;
            if let Some((old, _)) = read(&transaction, &self.namespace, write_id, self.limits)? {
                if old.pack_sha256 != evidence.pack.sha256 || old.verification != evidence.method {
                    return Err(error(
                        "evidence write identity conflicts with existing content",
                    ));
                }
                sql(transaction.commit())?;
                return Ok(old);
            }
            let (count, total): (i64, i64) = sql(transaction.query_row(
                "SELECT count(*),coalesce(sum(length(payload)),0) FROM retrieval_evidence",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ))?;
            if count >= self.limits.max_records as i64
                || total < 0
                || total as u64 + bytes.len() as u64 > self.limits.max_total_payload_bytes as u64
            {
                return Err(error("evidence database accounted payload budget exceeded"));
            }
            let receipt = EvidenceReceipt::for_write(&self.namespace, write_id, evidence)?;
            let receipt_bytes = serde_json::to_vec(&receipt).map_err(error)?;
            sql(transaction.execute("INSERT INTO retrieval_evidence(namespace,write_id,pack_sha256,payload,receipt) VALUES(?1,?2,?3,?4,?5)",params![self.namespace,write_id,evidence.pack.sha256,bytes,receipt_bytes]))?;
            sql(transaction.commit())?;
            Ok(receipt)
        }
        fn get(
            &mut self,
            write_id: &str,
        ) -> Result<Option<(EvidenceReceipt, EvidencePack)>, Error> {
            if !identity(write_id) {
                return Err(error("invalid evidence write identity"));
            }
            let transaction = sql(self.connection.transaction())?;
            let value = read(&transaction, &self.namespace, write_id, self.limits)?;
            sql(transaction.commit())?;
            Ok(value)
        }
    }
}
#[cfg(feature = "sqlite-store")]
pub use sqlite::SqliteEvidenceStore;
