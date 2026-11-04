use retrieval_engine::{CancelResult, RequestControl};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    },
    time::Duration,
};

#[test]
fn cancellation_and_commit_have_one_linearization_point() {
    for _ in 0..32 {
        let control = RequestControl::new(Duration::from_secs(5)).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let writes = AtomicUsize::new(0);
        let cancel = control.clone();
        let ready = barrier.clone();
        let thread = std::thread::spawn(move || {
            ready.wait();
            cancel.cancel()
        });
        barrier.wait();
        let committed = control.commit(|| {
            writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let result = thread.join().unwrap();
        if committed.is_ok() {
            assert_eq!(result, CancelResult::TooLate);
            assert_eq!(writes.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(result, CancelResult::Requested);
            assert_eq!(writes.load(Ordering::SeqCst), 0);
        }
        assert!(control.commit(|| Ok(())).is_err());
    }
}

#[test]
fn cancelled_or_timed_out_work_cannot_commit() {
    let control = RequestControl::new(Duration::from_secs(2)).unwrap();
    assert_eq!(control.cancel(), CancelResult::Requested);
    assert_eq!(control.cancel(), CancelResult::AlreadyRequested);
    assert!(control
        .checkpoint()
        .unwrap_err()
        .to_string()
        .contains("request cancelled"));
    assert!(control
        .commit::<()>(|| panic!("cancelled transaction executed"))
        .is_err());
    let control = RequestControl::new(Duration::from_millis(5)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert!(control
        .checkpoint()
        .unwrap_err()
        .to_string()
        .contains("request deadline"));
    assert!(control
        .commit::<()>(|| panic!("expired transaction executed"))
        .is_err());
    assert!(RequestControl::new(Duration::ZERO).is_err());
}
