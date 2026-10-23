//! Process measurements and an explicitly owned heartbeat task.
use super::loads::{LoadObserver, NoopLoadObserver};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Debug, Default)]
pub struct Resources {
    pub cpu_seconds: Option<f64>,
    pub native_threads: Option<u64>,
    pub nr_throttled: Option<u64>,
    pub throttled_usec: Option<u64>,
}

pub trait ResourceSource: Send + Sync {
    fn snapshot(&self) -> Resources;
}
pub struct ProcessResources;
impl ResourceSource for ProcessResources {
    fn snapshot(&self) -> Resources {
        let stat = bounded_read("/sys/fs/cgroup/cpu.stat").unwrap_or_default();
        let counter = |key: &str| {
            stat.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next()? == key)
                    .then(|| fields.next()?.parse().ok())
                    .flatten()
            })
        };
        let native_threads = bounded_read("/proc/self/status").and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Threads:")?.trim().parse().ok())
        });
        Resources {
            cpu_seconds: process_cpu_seconds(),
            native_threads,
            nr_throttled: counter("nr_throttled"),
            throttled_usec: counter("throttled_usec"),
        }
    }
}

fn bounded_read(path: &str) -> Option<String> {
    let mut contents = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(16 * 1024)
        .read_to_string(&mut contents)
        .ok()?;
    Some(contents)
}

#[cfg(unix)]
fn process_cpu_seconds() -> Option<f64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // getrusage initializes the entire output on success. No pointers escape.
    let usage = unsafe {
        if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) != 0 {
            return None;
        }
        usage.assume_init()
    };
    Some(
        usage.ru_utime.tv_sec as f64
            + usage.ru_stime.tv_sec as f64
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0,
    )
}
#[cfg(not(unix))]
fn process_cpu_seconds() -> Option<f64> {
    None
}

pub struct Heartbeat {
    admitted: AtomicUsize,
    resources: Arc<dyn ResourceSource>,
    loads: Arc<dyn LoadObserver>,
    previous: Mutex<(std::time::Instant, Resources)>,
}
impl Default for Heartbeat {
    fn default() -> Self {
        Self::new(Arc::new(ProcessResources), Arc::new(NoopLoadObserver))
    }
}
impl Heartbeat {
    pub fn new(resources: Arc<dyn ResourceSource>, loads: Arc<dyn LoadObserver>) -> Self {
        let previous = Mutex::new((std::time::Instant::now(), resources.snapshot()));
        Self {
            admitted: AtomicUsize::new(0),
            resources,
            loads,
            previous,
        }
    }
    pub fn admit(&self) -> Admission<'_> {
        self.admitted.fetch_add(1, Ordering::Relaxed);
        Admission(self)
    }
    pub fn record(&self, container: &str, gate: dynapi_policy::GateSnapshot) -> Value {
        let now = std::time::Instant::now();
        let current = self.resources.snapshot();
        let previous = {
            let mut previous = self.previous.lock().unwrap();
            std::mem::replace(&mut *previous, (now, current.clone()))
        };
        let elapsed = now.duration_since(previous.0).as_secs_f64();
        self.record_delta(container, gate, &previous.1, &current, elapsed)
    }
    fn record_delta(
        &self,
        container: &str,
        gate: dynapi_policy::GateSnapshot,
        previous: &Resources,
        current: &Resources,
        elapsed: f64,
    ) -> Value {
        let mut record = json!({
            "event": "api_container", "container": container,
            "inflightAdmitted": self.admitted.load(Ordering::Relaxed),
            "computeSlotsInUse": gate.in_use, "computeSlots": gate.capacity,
            "oldestSlotAgeSeconds": gate.oldest_age_seconds,
            "maxCallerSlots": gate.max_caller_slots,
            "fairShareRefusals": gate.fair_share_refusals,
            "fairShareWouldRefuseConservative": gate.fair_share_would_refuse_conservative,
            "capacityRefusals": gate.capacity_refusals,
        });
        if let (Some(old), Some(new)) = (previous.cpu_seconds, current.cpu_seconds) {
            let delta = (new - old).max(0.0);
            record["cpuSecondsDelta"] = json!(delta);
            record["cpuUtilization"] = json!(if elapsed > 0.0 { delta / elapsed } else { 0.0 });
        }
        if let Some(threads) = current.native_threads {
            record["nativeThreads"] = threads.into();
        }
        for (key, old, new) in [
            (
                "nrThrottledDelta",
                previous.nr_throttled,
                current.nr_throttled,
            ),
            (
                "throttledUsecDelta",
                previous.throttled_usec,
                current.throttled_usec,
            ),
        ] {
            if let (Some(old), Some(new)) = (old, new) {
                record[key] = new.saturating_sub(old).into();
            }
        }
        if let Some(loads) = self.loads.snapshot().as_object() {
            for key in ["loads", "opens", "productsLoaded"] {
                if let Some(value) = loads.get(key) {
                    record[key] = value.clone();
                }
            }
        }
        record
    }
}

pub struct Admission<'a>(&'a Heartbeat);
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.admitted.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct HeartbeatTask(Option<tokio::task::JoinHandle<()>>);
impl HeartbeatTask {
    pub fn start(app: Weak<crate::App>, seconds: Option<f64>) -> Self {
        let interval = seconds
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
            .filter(|d| !d.is_zero())
            .and_then(|d| {
                tokio::time::Instant::now()
                    .checked_add(d)
                    .map(|start| (d, start))
            });
        let task = interval.map(|(period, start)| {
            tokio::spawn(async move {
                let mut ticks = tokio::time::interval_at(start, period);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticks.tick().await;
                    let Some(app) = app.upgrade() else {
                        break;
                    };
                    // A broken measurement or sink must not stop future beats.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        app.emit(
                            crate::telemetry::Stream::Terminal,
                            app.heartbeat
                                .record(&app.config.container, app.gate.snapshot()),
                        );
                    }));
                }
            })
        });
        Self(task)
    }
    pub async fn stop(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
impl Drop for HeartbeatTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gate() -> dynapi_policy::GateSnapshot {
        dynapi_policy::GateSnapshot {
            in_use: 1,
            capacity: 6,
            oldest_age_seconds: Some(2.5),
            max_caller_slots: 1,
            fair_share_refusals: 2,
            fair_share_would_refuse_conservative: 1,
            capacity_refusals: 3,
        }
    }
    #[test]
    fn heartbeat_deltas_and_optional_measurements_are_explicit() {
        let heartbeat = Heartbeat::default();
        let admitted = heartbeat.admit();
        let old = Resources {
            cpu_seconds: Some(3.0),
            nr_throttled: Some(8),
            throttled_usec: Some(100),
            ..Resources::default()
        };
        let new = Resources {
            cpu_seconds: Some(4.0),
            native_threads: Some(7),
            nr_throttled: Some(10),
            throttled_usec: Some(150),
        };
        let record = heartbeat.record_delta("fake-container", gate(), &old, &new, 2.0);
        assert_eq!(
            record,
            json!({"event":"api_container", "container":"fake-container", "inflightAdmitted":1, "computeSlotsInUse":1,"computeSlots":6,"oldestSlotAgeSeconds":2.5,"maxCallerSlots":1,"fairShareRefusals":2,"fairShareWouldRefuseConservative":1,"capacityRefusals":3,"cpuSecondsDelta":1.0,"cpuUtilization":0.5,"nativeThreads":7,"nrThrottledDelta":2,"throttledUsecDelta":50})
        );
        drop(admitted);
        let reset = heartbeat.record_delta("local", gate(), &new, &old, 0.0);
        assert_eq!(reset["inflightAdmitted"], 0);
        assert_eq!(reset["cpuSecondsDelta"], 0.0);
        assert_eq!(reset["cpuUtilization"], 0.0);
        assert_eq!(reset["nrThrottledDelta"], 0);
        assert_eq!(reset["throttledUsecDelta"], 0);
        let missing = heartbeat.record_delta("local", gate(), &old, &Resources::default(), 1.0);
        for key in [
            "nativeThreads",
            "cpuSecondsDelta",
            "cpuUtilization",
            "nrThrottledDelta",
            "throttledUsecDelta",
            "loads",
            "opens",
            "productsLoaded",
            "pythonThreads",
        ] {
            assert!(missing.get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn injected_resources_and_load_observer_supply_measured_fields_only() {
        struct FakeResources(Mutex<Resources>);
        impl ResourceSource for FakeResources {
            fn snapshot(&self) -> Resources {
                self.0.lock().unwrap().clone()
            }
        }
        struct FakeLoads;
        impl LoadObserver for FakeLoads {
            fn snapshot(&self) -> Value {
                json!({"loads":{"example-forecast":{"warm":{"count":1,"ms":2,"failures":0}}},"opens":{"example-forecast":{"hit":{"count":1,"ms":1}}},"productsLoaded":1,"unexpected":"fake-payload"})
            }
        }
        let resources = Arc::new(FakeResources(Mutex::new(Resources {
            cpu_seconds: Some(3.0),
            ..Resources::default()
        })));
        let heartbeat = Heartbeat::new(resources.clone(), Arc::new(FakeLoads));
        resources.0.lock().unwrap().cpu_seconds = Some(4.0);
        let record = heartbeat.record("fake-container", gate());
        assert_eq!(record["cpuSecondsDelta"], 1.0);
        assert_eq!(record["productsLoaded"], 1);
        assert_eq!(record["loads"]["example-forecast"]["warm"]["count"], 1);
        assert!(record.get("nativeThreads").is_none());
        assert!(record.get("unexpected").is_none());
        assert!(!record.to_string().contains("fake-payload"));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_waits_one_interval_stops_and_does_not_own_the_app() {
        let sink = Arc::new(crate::telemetry::RecordingSink::default());
        let app = Arc::new(
            crate::App::new(
                crate::config::Config::from_values(&Default::default()).unwrap(),
                Arc::new(crate::Monotonic::default()),
                sink.clone(),
            )
            .unwrap(),
        );
        let task = HeartbeatTask::start(Arc::downgrade(&app), Some(2.0));
        tokio::task::yield_now().await;
        assert!(sink.0.lock().unwrap().is_empty());
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        assert_eq!(sink.0.lock().unwrap()[0].1["event"], "api_container");
        task.stop().await;
        tokio::time::advance(Duration::from_secs(8)).await;
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        for interval in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::MAX),
        ] {
            let disabled = HeartbeatTask::start(Arc::downgrade(&app), interval);
            assert!(disabled.0.is_none());
        }
        let task = HeartbeatTask::start(Arc::downgrade(&app), Some(1.0));
        let weak = Arc::downgrade(&app);
        drop(app);
        assert!(weak.upgrade().is_none());
        tokio::time::advance(Duration::from_secs(1)).await;
        task.stop().await;
    }
}
