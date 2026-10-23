use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn settled(owner: &Owner, state: PreparationState) {
    for _ in 0..1000 {
        if owner.snapshot().state == state {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "unexpected state {:?}, expected {state:?}",
        owner.snapshot()
    );
}
#[tokio::test(start_paused = true)]
async fn cumulative_deadline_retains_late_stage_and_refuses_replacements() {
    let owner = Arc::new(Owner::default());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    struct Retained(Arc<AtomicUsize>);
    impl Drop for Retained {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let deadline = Instant::now() + OPEN_TIMEOUT;
    let job = tokio::spawn({
        let owner = owner.clone();
        let release = release.clone();
        let calls = calls.clone();
        let drops = drops.clone();
        async move {
            owner
                .run(deadline, move |ctx| async move {
                    ctx.stage(PreparationStage::Bootstrap, async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        Ok(())
                    })
                    .await?;
                    ctx.stage(PreparationStage::Capture, async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        Ok(Retained(drops))
                    })
                    .await?;
                    ctx.stage(PreparationStage::Session, async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                })
                .await
        }
    });
    settled(&owner, PreparationState::Active).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        job.await.unwrap().unwrap_err().message,
        "Reader timed out after 90 seconds"
    );
    assert_eq!(owner.snapshot().stage, Some(PreparationStage::Capture));
    settled(&owner, PreparationState::Aftermath).await;
    for _ in 0..20 {
        assert!(
            owner
                .run(Instant::now() + OPEN_TIMEOUT, |_| async {
                    panic!("replacement polled");
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .await
                .is_err()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(owner.snapshot().refusals, 20);
    tokio::time::advance(OVERDUE).await;
    assert_eq!(owner.snapshot().state, PreparationState::Overdue);
    assert_eq!(owner.snapshot().aftermath_age_seconds, Some(90.0));
    assert_eq!(owner.snapshot().timeouts, 1);
    release.notify_one();
    settled(&owner, PreparationState::Available).await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        owner
            .run(Instant::now() + OPEN_TIMEOUT, |_| async { Ok(()) })
            .await
            .is_ok()
    );
}
#[tokio::test(start_paused = true)]
async fn cancellation_does_not_drop_joined_work_and_shutdown_reports_it() {
    let owner = Arc::new(Owner::default());
    for _ in 0..8 {
        let spec = Arc::new(Notify::new());
        let config = Arc::new(Notify::new());
        let entered = Arc::new(Notify::new());
        let job = tokio::spawn({
            let owner = owner.clone();
            let spec = spec.clone();
            let config = config.clone();
            let entered = entered.clone();
            async move {
                owner
                    .run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                        ctx.stage(PreparationStage::Bootstrap, async {
                            entered.notify_one();
                            let spec = tokio::spawn(async move {
                                spec.notified().await;
                                Err::<(), _>(err(ErrorKind::Storage, "fixture"))
                            });
                            let (spec, _) = tokio::join!(spec, config.notified());
                            spec.unwrap()
                        })
                        .await
                    })
                    .await
            }
        });
        entered.notified().await;
        job.abort();
        let _ = job.await;
        settled(&owner, PreparationState::Aftermath).await;
        spec.notify_one();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(owner.snapshot().state, PreparationState::Aftermath);
        config.notify_one();
        settled(&owner, PreparationState::Available).await;
    }
    let release = Arc::new(Notify::new());
    let job = tokio::spawn({
        let owner = owner.clone();
        let release = release.clone();
        async move {
            owner
                .run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                    ctx.stage(PreparationStage::Session, async {
                        release.notified().await;
                        Ok(())
                    })
                    .await
                })
                .await
        }
    });
    settled(&owner, PreparationState::Active).await;
    job.abort();
    let _ = job.await;
    let snapshot = owner
        .shutdown_until(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(snapshot.occupied && snapshot.admission_closed);
    assert_eq!(snapshot.state, PreparationState::Aftermath);
    release.notify_one();
    settled(&owner, PreparationState::Closed).await;
    assert!(
        owner
            .run(Instant::now() + OPEN_TIMEOUT, |_| async { Ok(()) })
            .await
            .is_err()
    );
}
#[tokio::test]
async fn exceptional_supervisor_exit_faults_closed() {
    let owner = Arc::new(Owner::default());
    let result: Result<(), _> = owner
        .run(Instant::now() + OPEN_TIMEOUT, |ctx| async move {
            ctx.stage(PreparationStage::Bootstrap, async {
                panic!("fixture panic");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await
        })
        .await;
    assert!(result.is_err());
    settled(&owner, PreparationState::Faulted).await;
    assert!(owner.snapshot().occupied && owner.snapshot().admission_closed);
    assert!(
        owner
            .run(Instant::now() + OPEN_TIMEOUT, |_| async { Ok(()) })
            .await
            .is_err()
    );
}
#[tokio::test(start_paused = true)]
async fn expired_work_is_never_built_or_polled() {
    let owner = Arc::new(Owner::default());
    let result: Result<(), _> = owner
        .run(Instant::now(), |_| {
            panic!("expired builder");
            #[allow(unreachable_code)]
            async {
                Ok(())
            }
        })
        .await;
    assert!(result.is_err());
    assert_eq!(owner.snapshot().state, PreparationState::Available);
    assert_eq!(owner.snapshot().revision, 0);
}

mod upstream;

#[tokio::test]
async fn runtime_loss_and_explicit_abort_keep_unknown_work_faulted() {
    for runtime_loss in [false, true] {
        let owner = Arc::new(Owner::default());
        let shared = owner.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let entered = Arc::new(Notify::new());
                let call = tokio::spawn({
                    let owner = shared.clone();
                    let entered = entered.clone();
                    async move {
                        owner
                            .run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                                ctx.stage(PreparationStage::Bootstrap, async {
                                    entered.notify_one();
                                    std::future::pending::<Result<(), ReaderError>>().await
                                })
                                .await
                            })
                            .await
                    }
                });
                entered.notified().await;
                if !runtime_loss {
                    shared.lock().task.as_ref().unwrap().abort();
                    assert!(call.await.unwrap().is_err());
                    settled(&shared, PreparationState::Faulted).await;
                }
            });
            drop(runtime);
        })
        .join()
        .unwrap();
        assert_eq!(owner.snapshot().state, PreparationState::Faulted);
        assert!(owner.snapshot().occupied);
    }
}
#[tokio::test(start_paused = true)]
async fn cancellation_at_stage_transition_starts_no_later_work() {
    let owner = Arc::new(Owner::default());
    let entered = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let call = tokio::spawn({
        let owner = owner.clone();
        let entered = entered.clone();
        let proceed = proceed.clone();
        let calls = calls.clone();
        async move {
            owner
                .run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                    ctx.stage(PreparationStage::Bootstrap, async {
                        entered.notify_one();
                        proceed.notified().await;
                        Ok(())
                    })
                    .await?;
                    ctx.stage(PreparationStage::Capture, async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                })
                .await
        }
    });
    entered.notified().await;
    call.abort();
    let _ = call.await;
    proceed.notify_one();
    settled(&owner, PreparationState::Available).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn repeated_aftermath_retains_one_allocation_then_disposes_it() {
    let owner = Arc::new(Owner::default());
    let measurement = crate::allocation_tests::Measurement::start();
    let mut post_drain = Vec::new();
    for _ in 0..12 {
        let held = Arc::new(vec![3u8; 256 * 1024]);
        let weak = Arc::downgrade(&held);
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let call = tokio::spawn({
            let owner = owner.clone();
            let entered = entered.clone();
            let release = release.clone();
            async move {
                owner
                    .run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                        ctx.stage(PreparationStage::Capture, async move {
                            entered.notify_one();
                            release.notified().await;
                            Ok(held)
                        })
                        .await
                    })
                    .await
            }
        });
        entered.notified().await;
        call.abort();
        let _ = call.await;
        assert!(weak.upgrade().is_some());
        for _ in 0..10 {
            assert!(
                owner
                    .run(Instant::now() + OPEN_TIMEOUT, |_| async { Ok(()) })
                    .await
                    .is_err()
            );
        }
        release.notify_one();
        settled(&owner, PreparationState::Available).await;
        assert!(weak.upgrade().is_none());
        drop(weak);
        post_drain.push(measurement.counts().live);
    }
    let counts = measurement.counts();
    eprintln!(
        "preparation allocation peak={} live={} foreign={} post_drain={post_drain:?}",
        counts.peak, counts.live, counts.foreign_deallocation
    );
    assert!(counts.peak >= 256 * 1024);
    // Tokio may grow its bounded scheduler slab; released per-operation buffers
    // and tasks must not accumulate. This is Rust heap evidence, not an RSS cap.
    assert!(post_drain.last().unwrap().saturating_sub(post_drain[0]) < 16 * 1024);
}
