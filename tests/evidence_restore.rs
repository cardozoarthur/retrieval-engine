use retrieval_engine::{Document, EvidenceLimits, EvidenceSession, Limits, Unit};
use std::sync::Arc;
fn source(name: &str, text: &str) -> Arc<Document> {
    Arc::new(Document::from_bytes(name, text.as_bytes(), "text/plain", &Limits::default()).unwrap())
}
#[test]
fn offline_pack_continues_expansion_after_original_session_is_gone() {
    let doc = source(
        "urn:selected:artifact-one",
        "Antes: avó e café. O responsável não responde sem culpa. Depois: exceção requer prova.",
    );
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let hit = doc.find("não responde", 1).unwrap().remove(0);
    let found = session.register("responsabilidade", &hit).unwrap();
    let context = session.expand(&found.reference, 1, 2, Unit::Words).unwrap();
    let pack = session
        .export(std::slice::from_ref(&context.reference))
        .unwrap();
    drop(session);
    let recreated = source("urn:selected:artifact-one", doc.text());
    let mut resumed =
        EvidenceSession::restore(&pack, vec![recreated.clone()], EvidenceLimits::default())
            .unwrap();
    assert_eq!(resumed.id(), pack.payload.session_id);
    assert_eq!(resumed.reference_count(), 2);
    let expanded = resumed
        .expand(&context.reference, 3, 5, Unit::Characters)
        .unwrap();
    assert!(expanded.record.ordinal > context.record.ordinal);
    assert_eq!(expanded.record.anchor_start, found.record.anchor_start);
    assert_eq!(expanded.record.anchor_end, found.record.anchor_end);
    let final_pack = resumed.export(&[expanded.reference]).unwrap();
    final_pack.verify_against(&[recreated.as_ref()]).unwrap();
    assert_eq!(final_pack.payload.ancestors.len(), 2);
}
#[test]
fn restored_quotes_origin_version_pages_and_limits_are_revalidated() {
    let doc = source("origin-a", "Não se presume culpa; existe uma exceção.");
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let found = session
        .register("culpa", &doc.find("culpa", 1).unwrap()[0])
        .unwrap();
    let expanded = session.expand(&found.reference, 2, 2, Unit::Words).unwrap();
    let pack = session.export(&[expanded.reference]).unwrap();
    assert!(EvidenceSession::restore(
        &pack,
        vec![source("origin-b", doc.text())],
        EvidenceLimits::default()
    )
    .is_err());
    assert!(EvidenceSession::restore(
        &pack,
        vec![source("origin-a", "Não se presume culpa; mudou.")],
        EvidenceLimits::default()
    )
    .is_err());
    assert!(EvidenceSession::restore(
        &pack,
        vec![doc.clone()],
        EvidenceLimits {
            max_references: 1,
            ..Default::default()
        }
    )
    .is_err());
    let mut bad = pack.clone();
    bad.payload.entries[0].evidence.pages = vec![1];
    bad.sha256 = bad.calculate_sha256().unwrap();
    assert!(EvidenceSession::restore(&bad, vec![doc.clone()], EvidenceLimits::default()).is_err());
    let mut bad = pack;
    bad.payload.entries[0].evidence.text = "adulterado".into();
    bad.sha256 = bad.calculate_sha256().unwrap();
    assert!(EvidenceSession::restore(&bad, vec![doc], EvidenceLimits::default()).is_err());
}
