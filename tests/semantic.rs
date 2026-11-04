#![cfg(feature = "cpu-onnx")]
use retrieval_engine::{
    cosine_similarity, CancelResult, CpuConfig, E5Embedder, InputKind, RequestControl,
};

#[test]
fn invalid_vectors_and_missing_resources_fail() {
    assert!(cosine_similarity(&[], &[]).is_err());
    assert!(cosine_similarity(&[0.0], &[1.0]).is_err());
    assert!(cosine_similarity(&[f32::NAN], &[1.0]).is_err());
    assert!(cosine_similarity(&[1.0], &[1.0, 2.0]).is_err());
    assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]).unwrap() - 1.0).abs() < 1e-6);
    assert!(E5Embedder::load("missing-assets", CpuConfig::default()).is_err());
    assert!(E5Embedder::load(
        "missing-assets",
        CpuConfig {
            threads: 0,
            ..CpuConfig::default()
        }
    )
    .is_err());
}

#[test]
#[ignore = "requires explicitly installed pinned E5 ONNX/ORT resources; actual CPU inference"]
fn actual_e5_cpu_retrieves_portuguese_paraphrases_and_reuses_session() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/e5-small-int8");
    // This tests microbatch stability through actual inference, not cache reuse.
    let mut model = E5Embedder::load(
        root,
        CpuConfig {
            cache_max_bytes: 0,
            ..Default::default()
        },
    )
    .unwrap();
    let q = model
        .embed(
            InputKind::Query,
            &["Quem paga pelos prejuízos causados por vazamento de informações?".into()],
        )
        .unwrap();
    let p = model.embed(InputKind::Passage, &[
        "O agente responde pela reparação dos danos decorrentes do tratamento indevido de dados pessoais.".into(),
        "O transporte rodoviário de cargas depende do recolhimento de tributos e frete.".into(),
        "A receita de bolo inclui farinha, ovos e leite.".into(),
    ]).unwrap();
    let scores: Vec<_> = p
        .vectors
        .iter()
        .map(|v| cosine_similarity(&q.vectors[0], v).unwrap())
        .collect();
    assert!(scores[0] > scores[1] && scores[0] > scores[2], "{scores:?}");
    assert_eq!(q.vectors[0].len(), 384);
    assert!(q.truncated.iter().all(|x| !x));
    for v in &p.vectors {
        assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-4);
    }
    let repeated = model.embed(InputKind::Passage, &["O agente responde pela reparação dos danos decorrentes do tratamento indevido de dados pessoais.".into()]).unwrap();
    let batch_single_cosine = cosine_similarity(&p.vectors[0], &repeated.vectors[0]).unwrap();
    eprintln!(
        "batch/single cosine={batch_single_cosine}, batch token lengths={:?}, single={:?}",
        p.used_token_counts, repeated.used_token_counts
    );
    assert!(batch_single_cosine > 0.999);
    assert_eq!(model.info().completed_batches, 3);
    assert!(model.embed(InputKind::Query, &[]).is_err());
    assert!(model
        .embed(InputKind::Passage, &vec!["x".into(); 9])
        .is_err());
    assert!(model.embed(InputKind::Query, &[String::new()]).is_err());
    assert!(model
        .embed(InputKind::Query, &["x".repeat(16 * 1024 + 1)])
        .is_err());
    assert!(model
        .embed(InputKind::Passage, &["palavra ".repeat(1000)])
        .is_err());
}

#[test]
#[ignore = "requires pinned assets; actual CPU packed batch and explicit truncation"]
fn actual_packed_batch_reports_truncation_and_repeats_same_shape() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/e5-small-int8");
    let mut model = E5Embedder::load(
        root,
        CpuConfig {
            max_tokens: 32,
            allow_truncation: true,
            packed_batching: true,
            ..CpuConfig::default()
        },
    )
    .unwrap();
    let texts = vec![
        "A responsabilidade não dispensa prova. ".repeat(50),
        "Exceção prevista em lei.".into(),
    ];
    let first = model.embed(InputKind::Passage, &texts).unwrap();
    assert!(first.truncated[0]);
    assert!(!first.truncated[1]);
    assert!(first.original_token_counts[0] > 32);
    assert_eq!(first.used_token_counts[0], 32);
    let second = model.embed(InputKind::Passage, &texts).unwrap();
    for (a, b) in first.vectors.iter().zip(&second.vectors) {
        assert!(cosine_similarity(a, b).unwrap() > 0.999);
    }
    assert_eq!(model.info().inference_runs, 2);
    assert!(!model.info().cache.enabled);
    assert_eq!(second.cache_hits, 0);
    assert_eq!(model.info().cache.entries, 0);
}

#[test]
#[ignore = "requires pinned assets; real cache hits and role isolation"]
fn actual_cache_reuses_vectors_without_reexecuting_model() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/e5-small-int8");
    let mut model = E5Embedder::load(root, CpuConfig::default()).unwrap();
    let texts = vec!["A norma não dispensa a comprovação do dano.".into()];
    let first = model.embed(InputKind::Passage, &texts).unwrap();
    let runs = model.info().inference_runs;
    let mut second = model.embed(InputKind::Passage, &texts).unwrap();
    assert_eq!(second.cache_hits, 1);
    assert_eq!(second.executed_model_tokens, 0);
    assert_eq!(second.vectors, first.vectors);
    assert_eq!(model.info().inference_runs, runs);
    second.vectors[0][0] = 123.0;
    assert_eq!(
        model.embed(InputKind::Passage, &texts).unwrap().vectors,
        first.vectors
    );
    let query = model.embed(InputKind::Query, &texts).unwrap();
    assert_eq!(query.cache_hits, 0);
    assert_eq!(query.cache_misses, 1);
    assert_eq!(model.info().inference_runs, runs + 1);
    assert!(model.info().cache.accounted_bytes <= model.info().config.cache_max_bytes);
}

#[test]
#[ignore = "actual ORT native termination, deadline and session reuse; requires pinned assets"]
fn actual_native_cancellation_and_deadline_preserve_resident_session() {
    use std::time::{Duration, Instant};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/e5-small-int8");
    let model = E5Embedder::load(
        root,
        CpuConfig {
            threads: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut text = String::new();
    loop {
        let next = format!("{text}dados pessoais sem consentimento ");
        if model.token_count(InputKind::Passage, &next).unwrap() > 512 {
            break;
        }
        text = next;
    }
    assert!(model.token_count(InputKind::Passage, &text).unwrap() > 480);
    let mut model = model;
    for deadline in [false, true] {
        let control = RequestControl::new(Duration::from_secs(10)).unwrap();
        let running = control.clone();
        let input = text.clone();
        let work = std::thread::spawn(move || {
            let result = model.embed_with_control(InputKind::Passage, &[input], &running);
            (model, result)
        });
        let end = Instant::now() + Duration::from_secs(5);
        while !control.interrupt_armed() && !work.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            control.interrupt_armed(),
            "native hook must become observable"
        );
        if deadline {
            control
                .limit_deadline(Instant::now() + Duration::from_millis(10))
                .unwrap();
        } else {
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(control.cancel(), CancelResult::Requested);
        }
        let (resident, result) = work.join().unwrap();
        model = resident;
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains(if deadline {
                "request deadline"
            } else {
                "request cancelled"
            }),
            "{message}"
        );
        assert_eq!(model.info().interrupted_runs, if deadline { 2 } else { 1 });
        assert_eq!(model.info().inference_runs, 0);
        assert_eq!(
            model.info().cache.entries,
            0,
            "interrupted native result cannot enter cache"
        );
        assert!(!control.interrupt_armed());
        assert!(control
            .commit::<()>(|| panic!("must not admit references"))
            .is_err());
        control.finish();
    }
    assert_eq!(model.info().inference_attempts, 2);
    let after = model
        .embed(InputKind::Passage, &["A exceção exige prova.".into()])
        .unwrap();
    assert_eq!(after.vectors[0].len(), 384);
    assert_eq!(model.info().inference_runs, 1);
    assert_eq!(model.info().cache.entries, 1);
}
