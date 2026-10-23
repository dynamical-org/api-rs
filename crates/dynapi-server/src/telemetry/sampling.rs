//! Weighted analytics decisions; independent of request admission and stdout sampling.
use dynapi_policy::Clock;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_CALLERS: usize = 10_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub threshold: u64,
    pub window_seconds: u64,
    pub weight: u64,
}
impl Settings {
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.threshold == 0 || self.window_seconds == 0 || self.weight == 0 {
            return Err("Analytics sampling settings must be positive");
        }
        Ok(self)
    }
}

pub trait Sampling: Send + Sync {
    /// Some(weight) sends the event; None omits only its analytics capture.
    fn sample(&self, identity: &str, status: u16) -> Option<u64>;
}
/// A sampler failure preserves the eligible event with unit weight.
pub fn weight(sampler: &dyn Sampling, identity: &str, status: u16) -> Option<u64> {
    super::best_effort(|| sampler.sample(identity, status)).unwrap_or(Some(1))
}
#[derive(Default)]
struct State {
    window: Option<f64>,
    serial: u128,
    counts: BTreeMap<String, (u128, u128)>,
    recency: BTreeMap<u128, String>,
}
pub struct AnalyticsSampler {
    settings: Settings,
    max_callers: usize,
    clock: Arc<dyn Clock>,
    draw: Arc<dyn Fn() -> f64 + Send + Sync>,
    state: Mutex<State>,
}
impl AnalyticsSampler {
    pub fn new(
        settings: Settings,
        max_callers: usize,
        clock: Arc<dyn Clock>,
        draw: Arc<dyn Fn() -> f64 + Send + Sync>,
    ) -> Result<Self, &'static str> {
        let settings = settings.validate()?;
        if max_callers == 0 {
            return Err("Analytics sampling caller limit must be positive");
        }
        Ok(Self {
            settings,
            max_callers,
            clock,
            draw,
            state: Mutex::new(State::default()),
        })
    }
    pub(crate) fn production(settings: Settings) -> Result<Self, &'static str> {
        let seed = u64::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[8..].try_into().unwrap());
        let random = SeededRandom(AtomicU64::new(seed));
        Self::new(
            settings,
            MAX_CALLERS,
            Arc::new(NativeMonotonic),
            Arc::new(move || random.draw()),
        )
    }
}
impl Sampling for AnalyticsSampler {
    fn sample(&self, identity: &str, status: u16) -> Option<u64> {
        if !self.settings.enabled || identity == "anonymous" {
            return Some(1);
        }
        let count = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let window = (self.clock.now() / self.settings.window_seconds as f64).floor();
            assert!(window.is_finite(), "Sampling clock must be finite");
            if state.window != Some(window) {
                state.window = Some(window);
                state.counts.clear();
                state.recency.clear();
                state.serial = 0;
            }
            let count = if let Some((count, touched)) = state.counts.remove(identity) {
                state.recency.remove(&touched);
                count
            } else {
                0
            };
            let count = (count + 1).min(u128::from(self.settings.threshold) + 1);
            state.serial += 1;
            let serial = state.serial;
            state.counts.insert(identity.into(), (count, serial));
            state.recency.insert(serial, identity.into());
            while state.counts.len() > self.max_callers {
                if let Some((_, identity)) = state.recency.pop_first() {
                    state.counts.remove(&identity);
                }
            }
            count
        };
        if count <= u128::from(self.settings.threshold) || !(200..300).contains(&status) {
            return Some(1);
        }
        ((self.draw)() < 1.0 / self.settings.weight as f64).then_some(self.settings.weight)
    }
}
// Fixed windows use the OS monotonic epoch, independently of cache-relative clocks.
struct NativeMonotonic;
impl Clock for NativeMonotonic {
    fn now(&self) -> f64 {
        #[cfg(unix)]
        {
            let mut value = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: clock_gettime writes one valid, exclusively borrowed timespec.
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) },
                0,
                "Sampling clock unavailable"
            );
            value.tv_sec as f64 + value.tv_nsec as f64 / 1_000_000_000.0
        }
        #[cfg(not(unix))]
        panic!("Sampling clock unavailable")
    }
}
/// Per-instance SplitMix64 state, seeded once from OS entropy; no global draw stream.
struct SeededRandom(AtomicU64);
impl SeededRandom {
    fn draw(&self) -> f64 {
        let mut bits = self
            .0
            .fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed)
            .wrapping_add(0x9e3779b97f4a7c15);
        bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d049bb133111eb);
        bits ^= bits >> 31;
        (bits >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FakeClock(Mutex<f64>);
    impl Clock for FakeClock {
        fn now(&self) -> f64 {
            *self.0.lock().unwrap()
        }
    }
    fn fixture(max_callers: usize) -> (AnalyticsSampler, Arc<FakeClock>, Arc<Mutex<f64>>) {
        let clock = Arc::new(FakeClock(Mutex::new(0.0)));
        let draw = Arc::new(Mutex::new(0.99));
        let draws = draw.clone();
        (
            AnalyticsSampler::new(
                Settings {
                    enabled: true,
                    threshold: 3,
                    window_seconds: 60,
                    weight: 10,
                },
                max_callers,
                clock.clone(),
                Arc::new(move || *draws.lock().unwrap()),
            )
            .unwrap(),
            clock,
            draw,
        )
    }
    #[test]
    fn threshold_and_strict_probability_boundaries_count_every_eligible_status() {
        let (sampler, _, draw) = fixture(10);
        assert_eq!(
            [
                sampler.sample("a", 404),
                sampler.sample("a", 429),
                sampler.sample("a", 200)
            ],
            [Some(1); 3]
        );
        *draw.lock().unwrap() = 0.05;
        assert_eq!(sampler.sample("a", 200), Some(10));
        *draw.lock().unwrap() = 0.1;
        assert_eq!(sampler.sample("a", 200), None);
        *draw.lock().unwrap() = 0.0999;
        assert_eq!(sampler.sample("a", 204), Some(10));
        assert_eq!(sampler.sample("a", 299), Some(10));
        for status in [100, 199, 300, 301, 404, 422, 429, 451, 500, 503] {
            assert_eq!(sampler.sample("a", status), Some(1));
        }
        assert_eq!(sampler.sample("b", 200), Some(1));
    }
    #[test]
    fn fixed_window_rollover_and_clock_reversal_reset_counts() {
        let (sampler, clock, _) = fixture(10);
        for _ in 0..3 {
            assert_eq!(sampler.sample("a", 200), Some(1));
        }
        *clock.0.lock().unwrap() = 59.999;
        assert_eq!(sampler.sample("a", 200), None);
        *clock.0.lock().unwrap() = 60.0;
        for _ in 0..3 {
            assert_eq!(sampler.sample("a", 200), Some(1));
        }
        assert_eq!(sampler.sample("a", 200), None);
        *clock.0.lock().unwrap() = 0.0;
        assert_eq!(sampler.sample("a", 200), Some(1));
    }
    #[test]
    fn lru_and_saturated_counts_bound_both_indexes() {
        let (sampler, _, _) = fixture(2);
        for _ in 0..1000 {
            sampler.sample("a", 200);
        }
        assert_eq!(sampler.state.lock().unwrap().counts["a"].0, 4);
        sampler.sample("b", 404);
        sampler.sample("a", 200);
        sampler.sample("c", 200);
        {
            let state = sampler.state.lock().unwrap();
            assert_eq!(
                state.counts.keys().map(String::as_str).collect::<Vec<_>>(),
                vec!["a", "c"]
            );
            assert_eq!(state.recency.len(), 2);
        }
        assert_eq!(sampler.sample("b", 200), Some(1));
        for i in 0..1000 {
            sampler.sample(&format!("id-{i}"), 404);
            let state = sampler.state.lock().unwrap();
            assert!(state.counts.len() <= 2);
            assert_eq!(state.recency.len(), state.counts.len());
        }
    }
    #[test]
    fn anonymous_and_off_never_consult_clock_draw_or_state() {
        struct BrokenClock;
        impl Clock for BrokenClock {
            fn now(&self) -> f64 {
                panic!("not consulted")
            }
        }
        let (mut sampler, _, _) = fixture(2);
        sampler.clock = Arc::new(BrokenClock);
        sampler.draw = Arc::new(|| panic!("not consulted"));
        for _ in 0..100 {
            assert_eq!(sampler.sample("anonymous", 200), Some(1));
        }
        assert!(sampler.state.lock().unwrap().counts.is_empty());
        sampler.settings.enabled = false;
        for _ in 0..100 {
            assert_eq!(sampler.sample("a", 200), Some(1));
        }
        assert!(sampler.state.lock().unwrap().counts.is_empty());
    }
    #[test]
    fn concurrent_calls_keep_an_exact_prefix_and_capped_state() {
        let (sampler, _, _) = fixture(10);
        let sampler = Arc::new(sampler);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let sampler = sampler.clone();
                std::thread::spawn(move || {
                    (0..125)
                        .filter(|_| sampler.sample("a", 200) == Some(1))
                        .count()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .sum::<usize>(),
            3
        );
        assert_eq!(sampler.state.lock().unwrap().counts["a"].0, 4);
    }
    #[test]
    fn clock_and_draw_failures_send_unit_weight_and_allow_recovery() {
        let (mut sampler, clock, _) = fixture(2);
        *clock.0.lock().unwrap() = f64::NAN;
        assert_eq!(weight(&sampler, "a", 200), Some(1));
        *clock.0.lock().unwrap() = 0.0;
        for _ in 0..3 {
            assert_eq!(weight(&sampler, "a", 200), Some(1));
        }
        sampler.draw = Arc::new(|| panic!("synthetic draw failure"));
        assert_eq!(weight(&sampler, "a", 200), Some(1));
        sampler.draw = Arc::new(|| 0.0);
        assert_eq!(weight(&sampler, "a", 200), Some(10));
    }
    #[test]
    fn instance_seed_is_reproducible_and_draws_stay_in_unit_interval() {
        let a = SeededRandom(AtomicU64::new(17));
        let b = SeededRandom(AtomicU64::new(17));
        for _ in 0..1000 {
            let draw = a.draw();
            assert_eq!(draw, b.draw());
            assert!((0.0..1.0).contains(&draw));
        }
    }
    #[test]
    fn production_limit_retains_only_ten_thousand_caller_ids() {
        let mut sampler = AnalyticsSampler::production(Settings {
            enabled: true,
            threshold: 1,
            window_seconds: 60,
            weight: 10,
        })
        .unwrap();
        sampler.clock = Arc::new(FakeClock(Mutex::new(0.0)));
        sampler.draw = Arc::new(|| 0.5);
        for id in 0..=10_000 {
            assert_eq!(sampler.sample(&format!("{id:016x}"), 200), Some(1));
        }
        let state = sampler.state.lock().unwrap();
        assert_eq!(state.counts.len(), 10_000);
        assert_eq!(state.recency.len(), 10_000);
        assert!(!state.counts.contains_key("0000000000000000"));
        assert!(state.counts.contains_key("0000000000002710"));
    }
}
