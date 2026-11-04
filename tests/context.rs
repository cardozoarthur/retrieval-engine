use retrieval_engine::{Snapshot, Unit};

#[test]
fn unicode_and_asymmetric_expansion() {
    let doc = Snapshot::new("source.txt", "A norma não permite divulgação pública. Fim.");
    let hit = doc.find("permite").unwrap().remove(0);
    let context = doc.expand(hit, 1, 2, Unit::Words, 100).unwrap();
    assert_eq!(context.text, "não permite divulgação pública.");
    assert_eq!(context.source, "source.txt");
    assert_eq!(
        doc.expand(hit, 999, 999, Unit::Characters, 100)
            .unwrap()
            .text,
        doc.text()
    );
}

#[test]
fn limits_and_invalid_input() {
    let doc = Snapshot::new("x", "ação ação");
    let hits = doc.find("ação").unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[1], (5, 9));
    assert!(doc.find("").is_err());
    assert!(doc.expand((3, 99), 0, 0, Unit::Words, 100).is_err());
    assert!(doc.expand((0, 4), 0, 0, Unit::Characters, 3).is_err());
    assert!(doc.expand((4, 2), 0, 0, Unit::Characters, 100).is_err());
}
