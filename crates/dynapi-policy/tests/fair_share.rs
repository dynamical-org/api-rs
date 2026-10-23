use dynapi_policy::{AdmissionRefusal, Clock, ComputeGate, FairShare, caller_key};
use std::sync::{Arc, Barrier, Mutex};
struct Fixed;
impl Clock for Fixed {
    fn now(&self) -> f64 {
        10.0
    }
}
fn gate() -> ComputeGate<Fixed> {
    ComputeGate::new(6, 5, Fixed)
        .unwrap()
        .with_fair_share(FairShare::from_settings(6, true, None, None, 7).unwrap())
        .unwrap()
}
#[test]
fn every_boundary_and_unidentified_caller() {
    for in_use in 0..=6 {
        for held in 0..=in_use {
            // Establish arbitrary occupancy without invoking the rule during setup.
            let gate = ComputeGate::new(6, 5, Fixed).unwrap();
            let mut permits = Vec::new();
            for _ in 0..held {
                permits.push(gate.acquire(Some("a")).unwrap());
            }
            for _ in held..in_use {
                permits.push(gate.acquire(Some("other")).unwrap());
            }
            let gate = gate
                .with_fair_share(FairShare::from_settings(6, true, None, None, 7).unwrap())
                .unwrap();
            let result = gate.acquire(Some("a"));
            let expected = if in_use >= 4 && held >= 2 {
                Some(AdmissionRefusal::FairShare { retry_after: 7 })
            } else if in_use == 6 {
                Some(AdmissionRefusal::Capacity)
            } else {
                None
            };
            assert_eq!(result.err(), expected, "in_use={in_use}, held={held}");
        }
    }
}
#[test]
fn defaults_validation_and_small_capacity() {
    for capacity in 1..=6 {
        let share = FairShare::from_settings(capacity, true, None, None, 5).unwrap();
        assert_eq!(share.enabled, capacity >= 4);
        assert_eq!(
            share.busy_slots,
            if capacity < 4 { capacity } else { capacity - 2 }
        );
    }
    for (capacity, busy, caller) in [
        (6, Some(0), None),
        (6, Some(7), None),
        (6, None, Some(0)),
        (2, None, Some(1)),
    ] {
        assert!(FairShare::from_settings(capacity, true, busy, caller, 5).is_err());
    }
    assert!(FairShare::from_settings(6, false, Some(0), None, 5).is_err());
    assert!(FairShare::from_settings(6, true, None, None, 0).is_err());
    assert!(FairShare::from_settings(6, true, Some(1), Some(8), 5).is_ok());
    assert!(
        FairShare::from_settings(2, true, Some(2), Some(1), 5)
            .unwrap()
            .enabled
    );
    assert!(
        FairShare::from_settings(3, true, None, Some(1), 5)
            .unwrap()
            .enabled
    );
}
#[test]
fn identity_is_the_rate_key_with_explicit_identification() {
    for (peer, auth, ip, worker, value, identified) in [
        (
            Some("proxy"),
            true,
            Some("203.0.113.9"),
            None,
            "203.0.113.9",
            true,
        ),
        (
            Some("proxy"),
            true,
            Some("203.0.113.9"),
            Some("w.example"),
            "203.0.113.9|w.example",
            true,
        ),
        (Some("proxy"), true, None, None, "proxy", false),
        (Some("proxy"), true, Some(""), None, "", false),
        (
            None,
            true,
            None,
            Some("w.example"),
            "unknown|w.example",
            false,
        ),
        (
            Some("peer"),
            false,
            Some("forged"),
            Some("forged"),
            "peer",
            true,
        ),
        (None, false, None, None, "unknown", false),
    ] {
        let key = caller_key(peer, auth, ip, worker);
        assert_eq!((key.value.as_str(), key.identified), (value, identified));
    }
}
#[test]
fn admission_is_atomic_and_drop_restores_counts() {
    let gate = Arc::new(gate());
    let others: Vec<_> = (0..2)
        .map(|_| gate.acquire(Some("others")).unwrap())
        .collect();
    let barrier = Arc::new(Barrier::new(10));
    let permits = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..10)
        .map(|_| {
            let gate = gate.clone();
            let barrier = barrier.clone();
            let permits = permits.clone();
            std::thread::spawn(move || {
                barrier.wait();
                match gate.acquire(Some("a")) {
                    Ok(permit) => permits.lock().unwrap().push(permit),
                    Err(reason) => {
                        assert_eq!(reason, AdmissionRefusal::FairShare { retry_after: 7 })
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(permits.lock().unwrap().len(), 2);
    assert_eq!(gate.snapshot().fair_share_refusals, 8);
    assert_eq!(gate.snapshot().max_caller_slots, 2);
    drop(others);
    permits.lock().unwrap().clear();
    assert_eq!(gate.snapshot().in_use, 0);
    assert_eq!(gate.snapshot().max_caller_slots, 0);
}
#[test]
fn off_and_unidentified_still_count_capacity() {
    for identified in [false, true] {
        let gate = ComputeGate::new(6, 5, Fixed)
            .unwrap()
            .with_fair_share(FairShare::from_settings(6, false, None, None, 5).unwrap())
            .unwrap();
        let caller = identified.then_some("a");
        let permits: Vec<_> = (0..6).map(|_| gate.acquire(caller).unwrap()).collect();
        assert_eq!(gate.acquire(caller).err(), Some(AdmissionRefusal::Capacity));
        let snap = gate.snapshot();
        assert_eq!(snap.max_caller_slots, if identified { 6 } else { 0 });
        assert_eq!(snap.fair_share_refusals, 0);
        assert_eq!(snap.capacity_refusals, 1);
        drop(permits);
        assert_eq!(gate.snapshot().max_caller_slots, 0);
    }
    let gate = gate();
    let permits: Vec<_> = (0..6).map(|_| gate.acquire(None).unwrap()).collect();
    assert_eq!(gate.acquire(None).err(), Some(AdmissionRefusal::Capacity));
    assert_eq!(gate.snapshot().max_caller_slots, 0);
    drop(permits);
}
#[test]
fn telemetry_is_current_occupancy_and_cumulative_decisions_with_conservative_precedence() {
    let plain = ComputeGate::new(6, 5, Fixed).unwrap();
    let mut a: Vec<_> = (0..3).map(|_| plain.acquire(Some("a")).unwrap()).collect();
    let b = plain.acquire(Some("b")).unwrap();
    let gate = plain
        .with_fair_share(FairShare::from_settings(6, true, None, None, 5).unwrap())
        .unwrap();
    assert_eq!(
        gate.acquire(Some("a")).err(),
        Some(AdmissionRefusal::FairShare { retry_after: 5 })
    );
    let c = gate.acquire(Some("c")).unwrap();
    assert_eq!(
        gate.acquire(Some("a")).err(),
        Some(AdmissionRefusal::FairShare { retry_after: 5 })
    );
    let d = gate.acquire(Some("d")).unwrap();
    assert_eq!(
        gate.acquire(Some("e")).err(),
        Some(AdmissionRefusal::Capacity)
    );
    let before = gate.snapshot();
    assert_eq!(
        (
            before.fair_share_refusals,
            before.fair_share_would_refuse_conservative,
            before.capacity_refusals,
            before.max_caller_slots
        ),
        (2, 1, 1, 3)
    );
    a.pop();
    assert_eq!(gate.snapshot().max_caller_slots, 2);
    drop((a, b, c, d));
    let after = gate.snapshot();
    assert_eq!(
        (
            after.fair_share_refusals,
            after.fair_share_would_refuse_conservative,
            after.capacity_refusals,
            after.max_caller_slots
        ),
        (2, 1, 1, 0)
    );
    let plain = ComputeGate::new(6, 5, Fixed).unwrap();
    let held: Vec<_> = (0..6).map(|_| plain.acquire(Some("a")).unwrap()).collect();
    let gate = plain
        .with_fair_share(FairShare::from_settings(6, true, Some(6), None, 5).unwrap())
        .unwrap();
    assert_eq!(
        gate.acquire(Some("a")).err(),
        Some(AdmissionRefusal::FairShare { retry_after: 5 })
    );
    assert_eq!(gate.snapshot().fair_share_would_refuse_conservative, 0);
    assert_eq!(gate.snapshot().capacity_refusals, 0);
    drop(held);
}
#[test]
fn keyed_permit_survives_caller_cancellation_and_unwind_without_leaking_keys() {
    let gate = Arc::new(gate());
    let (release, work) = std::sync::mpsc::channel::<()>();
    let permit = gate.acquire(Some("a")).unwrap();
    let worker = std::thread::spawn(move || {
        let _permit = permit;
        let _ = work.recv();
    });
    assert_eq!(gate.snapshot().max_caller_slots, 1);
    drop(release);
    worker.join().unwrap();
    assert_eq!(gate.snapshot().max_caller_slots, 0);
    let _ = std::panic::catch_unwind(|| {
        let _permit = gate.acquire(Some("a")).unwrap();
        panic!("fixture worker fails");
    });
    assert_eq!(gate.snapshot().max_caller_slots, 0);
    for i in 0..1000 {
        drop(gate.acquire(Some(&format!("caller-{i}"))).unwrap());
    }
    assert_eq!(gate.snapshot().in_use, 0);
    assert_eq!(gate.snapshot().max_caller_slots, 0);
}

#[test]
fn explicit_thresholds_activate_without_clamping_and_large_caller_limits_are_safe() {
    for (capacity, busy, caller, limit) in [
        (2, Some(2), None, 2),
        (6, Some(1), Some(3), 3),
        (6, None, Some(8), 6),
        (6, None, Some(usize::MAX), 6),
    ] {
        let share = FairShare::from_settings(capacity, true, busy, caller, 5).unwrap();
        assert!(share.enabled);
        let gate = ComputeGate::new(capacity, 5, Fixed)
            .unwrap()
            .with_fair_share(share)
            .unwrap();
        let permits: Vec<_> = (0..limit)
            .map(|_| gate.acquire(Some("a")).unwrap())
            .collect();
        let expected = if limit >= share.caller_slots {
            AdmissionRefusal::FairShare { retry_after: 5 }
        } else {
            AdmissionRefusal::Capacity
        };
        assert_eq!(gate.acquire(Some("a")).err(), Some(expected));
        assert_eq!(gate.snapshot().fair_share_would_refuse_conservative, 0);
        drop(permits);
    }
}
