#![cfg(feature = "cpu-onnx")]
use retrieval_engine::{
    benchmark_model::{BenchmarkEncoder, BenchmarkProfile},
    InputKind, RequestControl,
};
use std::time::{Duration, Instant};
#[test]
fn missing_resources_and_invalid_cpu_limits_fail_before_native_load() {
    assert!(BenchmarkEncoder::load("missing-assets", BenchmarkProfile::MiniLmInt8, 2).is_err());
    assert!(BenchmarkEncoder::load("assets", BenchmarkProfile::E5Int8, 0).is_err());
    assert!(BenchmarkEncoder::load("assets", BenchmarkProfile::E5Fp32, 9).is_err());
}
#[test]
#[ignore = "requires pinned E5 CPU assets; real request cancellation and subsequent session reuse"]
fn actual_benchmark_request_cancel_discards_vector_and_preserves_encoder() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let mut encoder = BenchmarkEncoder::load(root, BenchmarkProfile::E5Int8, 2).unwrap();
    let control = RequestControl::new(Duration::from_secs(5)).unwrap();
    let cancel = control.clone();
    let observer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if cancel.interrupt_armed() {
                return cancel.cancel();
            }
            std::thread::yield_now();
        }
        panic!("benchmark never installed scoped interrupt");
    });
    let text = "responsabilidade civil sobre dados pessoais ".repeat(65);
    assert!(encoder.token_count(InputKind::Passage, &text).unwrap() <= 512);
    let result = encoder.embed(InputKind::Passage, &text, &control);
    assert_eq!(
        observer.join().unwrap(),
        retrieval_engine::CancelResult::Requested
    );
    assert!(result.is_err());
    assert_eq!(encoder.info().inference_runs, 0);
    let next = RequestControl::new(Duration::from_secs(5)).unwrap();
    let vector = encoder
        .embed(
            InputKind::Query,
            "responsabilidade sobre dados pessoais",
            &next,
        )
        .unwrap();
    assert_eq!(vector.vector.len(), 384);
    assert_eq!(encoder.info().inference_runs, 1);
    let already_cancelled = RequestControl::new(Duration::from_secs(5)).unwrap();
    already_cancelled.cancel();
    assert!(encoder
        .embed(InputKind::Query, "consulta", &already_cancelled)
        .is_err());
    assert_eq!(encoder.info().inference_runs, 1);
}
