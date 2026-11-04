use retrieval_engine::{StoredVector, VectorFormat};

#[test]
fn golden_wire_and_distances_are_explicit() {
    let input = [1.0, -1.0, 0.0];
    let fp = StoredVector::encode(&input, VectorFormat::Fp32).unwrap();
    assert_eq!(fp.payload_bytes(), 12);
    assert_eq!(&fp.to_bytes()[..9], b"RV01\x00\x03\x00\x00\x00");
    assert_eq!(fp.decode(), input);
    let int = StoredVector::encode(&input, VectorFormat::Int8).unwrap();
    assert_eq!(int.payload_bytes(), 7); // three codes plus per-vector f32 scale
    assert_eq!(&int.to_bytes()[13..], &[127, 129, 0]);
    let binary = StoredVector::encode(&input, VectorFormat::Binary).unwrap();
    assert_eq!(binary.payload_bytes(), 1);
    assert_eq!(binary.to_bytes()[9], 0b101); // zero has positive sign
    assert_eq!(binary.decode(), [1.0, -1.0, 1.0]);
    for v in [fp, int, binary] {
        let restored = StoredVector::from_bytes(&v.to_bytes()).unwrap();
        assert_eq!(restored.to_bytes(), v.to_bytes());
        assert!((v.similarity(&restored).unwrap() - 1.0).abs() < 1e-6);
    }
    let a = StoredVector::encode(&[1., 1., -1.], VectorFormat::Binary).unwrap();
    let b = StoredVector::encode(&[-1., 1., -1.], VectorFormat::Binary).unwrap();
    assert!((a.similarity(&b).unwrap() - 1. / 3.).abs() < 1e-6);
}

#[test]
fn int8_cosine_uses_decoded_norms_and_scale_is_not_global() {
    let a = StoredVector::encode(&[0.25, -0.6, 0.9, 0.1], VectorFormat::Int8).unwrap();
    let b = StoredVector::encode(&[3., 4., 5., 6.], VectorFormat::Int8).unwrap();
    let x = a.decode();
    let y = b.decode();
    let dot: f64 = x.iter().zip(&y).map(|(&a, &b)| a as f64 * b as f64).sum();
    let nx = x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>().sqrt();
    let ny = y.iter().map(|&v| (v as f64).powi(2)).sum::<f64>().sqrt();
    assert!((a.similarity(&b).unwrap() as f64 - dot / (nx * ny)).abs() < 1e-6);
    assert!(x
        .iter()
        .zip([0.25, -0.6, 0.9, 0.1])
        .all(|(a, b)| (a - b).abs() < 0.9 / 127.));
    let inverted = StoredVector::encode(&[-0.25, 0.6, -0.9, -0.1], VectorFormat::Int8).unwrap();
    assert!((a.similarity(&inverted).unwrap() + 1.).abs() < 1e-6);
}

#[test]
fn malformed_or_incompatible_vectors_fail_closed() {
    for input in [
        &[][..],
        &[0., 0.],
        &[f32::NAN, 1.],
        &[f32::INFINITY],
        &[f32::from_bits(1)],
    ] {
        for format in [VectorFormat::Fp32, VectorFormat::Int8, VectorFormat::Binary] {
            assert!(StoredVector::encode(input, format).is_err());
        }
    }
    let a = StoredVector::encode(&[1., 2.], VectorFormat::Fp32).unwrap();
    let b = StoredVector::encode(&[1., 2.], VectorFormat::Binary).unwrap();
    let c = StoredVector::encode(&[1.], VectorFormat::Fp32).unwrap();
    assert!(a.similarity(&b).is_err());
    assert!(a.similarity(&c).is_err());
    let bytes = a.to_bytes();
    for n in 0..bytes.len() {
        assert!(StoredVector::from_bytes(&bytes[..n]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(StoredVector::from_bytes(&trailing).is_err());
    for (pos, value) in [(0, b'X'), (4, 99), (8, 255)] {
        let mut malformed = bytes.clone();
        malformed[pos] = value;
        assert!(StoredVector::from_bytes(&malformed).is_err());
    }
    let mut invalid_padding = b.to_bytes();
    invalid_padding[9] |= 128;
    assert!(StoredVector::from_bytes(&invalid_padding).is_err());
    let q = StoredVector::encode(&[1., 2.], VectorFormat::Int8).unwrap();
    let mut invalid_scale = q.to_bytes();
    invalid_scale[9..13].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(StoredVector::from_bytes(&invalid_scale).is_err());
    let mut invalid_code = q.to_bytes();
    invalid_code[13] = 128;
    assert!(StoredVector::from_bytes(&invalid_code).is_err());
}

#[test]
fn e5_payload_accounting_includes_quantizer_but_not_unrelated_text() {
    let input = vec![0.5; 384];
    for (format, payload) in [
        (VectorFormat::Fp32, 1536),
        (VectorFormat::Int8, 388),
        (VectorFormat::Binary, 48),
    ] {
        let v = StoredVector::encode(&input, format).unwrap();
        assert_eq!(v.payload_bytes(), payload);
        assert_eq!(v.to_bytes().len(), 9 + payload);
    }
}

#[test]
fn every_successfully_encoded_extreme_is_readable_again() {
    for input in [
        [f32::MAX, 1.0],
        [f32::MIN_POSITIVE, 0.0],
        [-f32::MAX, f32::MAX],
    ] {
        for format in [VectorFormat::Fp32, VectorFormat::Int8, VectorFormat::Binary] {
            if let Ok(vector) = StoredVector::encode(&input, format) {
                assert!(
                    StoredVector::from_bytes(&vector.to_bytes()).is_ok(),
                    "{format:?} {input:?}"
                );
                assert!(vector.decode().iter().all(|v| v.is_finite()));
            }
        }
    }
}
