#![cfg(feature = "sqlite-store")]
use retrieval_engine::{
    evidence_store::{EvidenceStore, SqliteEvidenceStore, StoreLimits, VerifiedEvidence},
    Document, EvidenceLimits, EvidenceSession, Limits,
};
use std::sync::Arc;
fn verified() -> VerifiedEvidence {
    let doc = Arc::new(
        Document::from_bytes(
            "synthetic-store-source",
            "Não se presume culpa; a exceção exige prova.".as_bytes(),
            "text/plain",
            &Limits::default(),
        )
        .unwrap(),
    );
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let hit = doc.find("exceção", 1).unwrap().remove(0);
    let view = session.register("hipótese", &hit).unwrap();
    let pack = session.export(&[view.reference]).unwrap();
    VerifiedEvidence::against_snapshots(pack, &[doc.as_ref()]).unwrap()
}
#[test]
fn real_sqlite_roundtrip_idempotency_namespace_and_conflict() {
    let root = std::env::temp_dir().join(format!(
        "retrieval-store-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let evidence = verified();
    let mut store =
        SqliteEvidenceStore::create(&root, "project-a", StoreLimits::default()).unwrap();
    let first = store.put("write-1", &evidence).unwrap();
    assert_eq!(first.pack_sha256, evidence.pack().sha256);
    assert_eq!(
        store.put("write-1", &evidence).unwrap().receipt_sha256,
        first.receipt_sha256
    );
    let different = verified();
    assert!(store.put("write-1", &different).is_err());
    drop(store);
    let mut reopened =
        SqliteEvidenceStore::open(&root, "project-a", StoreLimits::default()).unwrap();
    let (receipt, pack) = reopened.get("write-1").unwrap().unwrap();
    assert_eq!(receipt.receipt_sha256, first.receipt_sha256);
    assert_eq!(pack.sha256, evidence.pack().sha256);
    let mut other = SqliteEvidenceStore::open(&root, "project-b", StoreLimits::default()).unwrap();
    assert!(other.get("write-1").unwrap().is_none());
    other.put("write-1", &different).unwrap();
    drop(other);
    drop(reopened);
    std::fs::remove_file(root).unwrap();
}
#[test]
fn unowned_database_budgets_and_corruption_fail() {
    let root = std::env::temp_dir().join(format!(
        "retrieval-store-negative-{}.sqlite",
        std::process::id()
    ));
    let canonical = rusqlite::Connection::open(&root).unwrap();
    canonical.execute_batch("CREATE TABLE canonical_records(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO canonical_records VALUES(1,'preserved');").unwrap();
    drop(canonical);
    let original = std::fs::read(&root).unwrap();
    assert!(SqliteEvidenceStore::open(&root, "p", StoreLimits::default()).is_err());
    assert!(SqliteEvidenceStore::create(&root, "p", StoreLimits::default()).is_err());
    assert_eq!(std::fs::read(&root).unwrap(), original);
    std::fs::remove_file(&root).unwrap();
    let mut store = SqliteEvidenceStore::create(
        &root,
        "p",
        StoreLimits {
            max_records: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let evidence = verified();
    store.put("one", &evidence).unwrap();
    assert!(store.put("two", &evidence).is_err());
    assert!(store.put("../path", &evidence).is_err());
    drop(store);
    let conn = rusqlite::Connection::open(&root).unwrap();
    conn.execute(
        "UPDATE retrieval_evidence SET payload=?1",
        [b"{}".as_slice()],
    )
    .unwrap();
    drop(conn);
    let mut reopened = SqliteEvidenceStore::open(&root, "p", StoreLimits::default()).unwrap();
    assert!(reopened.get("one").is_err());
    drop(reopened);
    std::fs::remove_file(root).unwrap();
}
