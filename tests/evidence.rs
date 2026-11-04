use retrieval_engine::{Document, EvidenceLimits, EvidencePack, EvidenceSession, Limits, Unit};
use std::sync::Arc;

fn document(text: &str) -> Arc<Document> {
    Arc::new(
        Document::from_bytes(
            "synthetic.txt",
            text.as_bytes(),
            "text/plain",
            &Limits::default(),
        )
        .unwrap(),
    )
}

#[test]
fn references_expand_across_calls_without_replacing_original() {
    let doc = document("Antes. Não é permitido divulgar dados pessoais. Depois há exceções.");
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let hit = doc.find("divulgar", 1).unwrap().remove(0);
    let first = session.register("Quem pode divulgar?", &hit).unwrap();
    let context = session.expand(&first.reference, 4, 2, Unit::Words).unwrap();
    assert!(context
        .evidence
        .text
        .contains("Não é permitido divulgar dados pessoais."));
    let more = session
        .expand(&context.reference, 8, 8, Unit::Characters)
        .unwrap();
    assert!(
        more.evidence.start <= context.evidence.start && more.evidence.end >= context.evidence.end
    );
    assert_eq!(more.record.anchor_start, hit.start);
    assert_eq!(more.record.anchor_end, hit.end);
    assert_eq!(
        more.record.parent_reference.as_deref(),
        Some(context.reference.as_str())
    );
    assert_eq!(
        session.resolve(&first.reference).unwrap().evidence.text,
        "divulgar"
    );
    assert_ne!(first.reference, context.reference);
    let mut other = EvidenceSession::new(vec![doc], EvidenceLimits::default()).unwrap();
    assert!(other.expand(&first.reference, 1, 1, Unit::Words).is_err());
}

#[test]
fn evidence_pack_round_trip_validates_against_source_and_lineage() {
    let doc = document("A norma não permite divulgação. Exceção exige prova, artigo 42.");
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let first = session
        .register("divulgação", &doc.find("divulgação", 1).unwrap()[0])
        .unwrap();
    let second = session.expand(&first.reference, 3, 3, Unit::Words).unwrap();
    let pack = session
        .export(std::slice::from_ref(&second.reference))
        .unwrap();
    assert_eq!(pack.payload.ancestors.len(), 1);
    pack.verify_integrity().unwrap();
    pack.verify_against(&[doc.as_ref()]).unwrap();
    let encoded = serde_json::to_vec(&pack).unwrap();
    let mut decoded: EvidencePack = serde_json::from_slice(&encoded).unwrap();
    decoded.verify_against(&[doc.as_ref()]).unwrap();
    decoded.payload.entries[0]
        .evidence
        .text
        .push_str(" adulterado");
    assert!(decoded.verify_integrity().is_err());
    assert!(pack
        .verify_against(&[document("Fonte alterada.").as_ref()])
        .is_err());
}

#[test]
fn recalculated_self_hash_is_not_source_authentication() {
    let doc = document("A norma não permite.");
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let hit = session
        .register("negação", &doc.find("não", 1).unwrap()[0])
        .unwrap();
    let mut forged = session.export(&[hit.reference]).unwrap();
    let entry = &mut forged.payload.entries[0];
    entry.evidence.text = "sim".into(); // Same number of Unicode scalars.
    use sha2::{Digest, Sha256};
    entry.record.quote_sha256 = format!("{:x}", Sha256::digest(b"sim"));
    entry.reference = entry.record.reference_id().unwrap();
    forged.sha256 = forged.calculate_sha256().unwrap();
    forged.verify_integrity().unwrap(); // Self-consistent data, not a signature.
    assert!(forged.verify_against(&[doc.as_ref()]).is_err());
}

#[test]
fn registration_and_pack_budgets_fail_without_partial_refs() {
    let doc = document("um dois três quatro cinco");
    let mut session = EvidenceSession::new(
        vec![doc.clone()],
        EvidenceLimits {
            max_references: 1,
            ..EvidenceLimits::default()
        },
    )
    .unwrap();
    let mut hit = doc.find("dois", 1).unwrap().remove(0);
    hit.text = "fake".into();
    assert!(session.register("q", &hit).is_err());
    assert_eq!(session.reference_count(), 0);
    let good = session
        .register("q", &doc.find("dois", 1).unwrap()[0])
        .unwrap();
    assert!(session.expand(&good.reference, 1, 1, Unit::Words).is_err());
    assert_eq!(session.reference_count(), 1);
    assert!(session
        .export(&[good.reference.clone(), good.reference])
        .is_err());
    let mut small = EvidenceSession::new(
        vec![doc.clone()],
        EvidenceLimits {
            max_pack_chars: 2,
            ..EvidenceLimits::default()
        },
    )
    .unwrap();
    let good = small
        .register("q", &doc.find("dois", 1).unwrap()[0])
        .unwrap();
    assert!(small.export(&[good.reference]).is_err());
}

#[test]
fn expansion_reaches_another_pdf_page_and_preserves_snapshot() {
    let doc = Arc::new(
        Document::from_bytes(
            "fixture.pdf",
            include_bytes!("fixtures/two-pages.pdf"),
            "application/pdf",
            &Limits::default(),
        )
        .unwrap(),
    );
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let first = session
        .register(
            "responsabilidade",
            &doc.find("responsabilidade", 1).unwrap()[0],
        )
        .unwrap();
    assert_eq!(first.evidence.pages, vec![2]);
    let full = session
        .expand(&first.reference, 1000, 1000, Unit::Characters)
        .unwrap();
    assert_eq!(full.evidence.pages, vec![1, 2]);
    let pack = session.export(&[full.reference]).unwrap();
    pack.verify_against(&[doc.as_ref()]).unwrap();
}

#[test]
fn source_replay_rejects_resigned_false_expansion_and_detached_audit() {
    let doc = document("Antes não permite divulgar dados. Depois há exceções.");
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let root = session
        .register("q", &doc.find("divulgar", 1).unwrap()[0])
        .unwrap();
    let child = session.expand(&root.reference, 2, 2, Unit::Words).unwrap();
    let pack = session
        .export(std::slice::from_ref(&child.reference))
        .unwrap();
    let mut fake = pack.clone();
    fake.payload.entries[0]
        .record
        .expansion
        .as_mut()
        .unwrap()
        .before = 0;
    fake.payload.entries[0]
        .record
        .expansion
        .as_mut()
        .unwrap()
        .after = 0;
    fake.payload.entries[0]
        .record
        .expansion
        .as_mut()
        .unwrap()
        .unit = Unit::Characters;
    fake.payload.entries[0].reference = fake.payload.entries[0].record.reference_id().unwrap();
    fake.sha256 = fake.calculate_sha256().unwrap();
    fake.verify_integrity().unwrap();
    assert!(fake.verify_against(&[doc.as_ref()]).is_err());
    let detached = session
        .register("other", &doc.find("Antes", 1).unwrap()[0])
        .unwrap();
    let mut fake = pack;
    fake.payload.ancestors.push(detached.record);
    fake.sha256 = fake.calculate_sha256().unwrap();
    assert!(fake.verify_integrity().is_err());
}

#[test]
fn register_many_admits_all_or_none() {
    let doc = document("um dois três quatro");
    let mut session = EvidenceSession::new(vec![doc.clone()], EvidenceLimits::default()).unwrap();
    let good = doc.find("dois", 1).unwrap().remove(0);
    let mut bad = doc.find("três", 1).unwrap().remove(0);
    bad.text = "fake".into();
    assert!(session.register_many("q", &[good.clone(), bad]).is_err());
    assert_eq!(session.reference_count(), 0);
    assert_eq!(session.register("q", &good).unwrap().record.ordinal, 1);
}
