use dynapi_catalog::Clock;
use std::sync::atomic::{AtomicU64, Ordering};
#[derive(Default)]
pub struct FakeClock(AtomicU64);
impl FakeClock {
    pub fn set(&self, now: f64) {
        self.0.store(now.to_bits(), Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::SeqCst))
    }
}
