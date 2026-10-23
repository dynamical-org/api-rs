//! One process-wide owner for source-aware metadata work, including late work.
use crate::{ErrorKind, ReaderError, err};
use serde::Serialize;
use std::{
    future::Future,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{Notify, oneshot},
    task::JoinHandle,
    time::Instant,
};

pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(90);
const OVERDUE: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationState {
    Available,
    Active,
    Aftermath,
    Overdue,
    Faulted,
    Closed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationStage {
    Acquired,
    Bootstrap,
    Capture,
    Validation,
    Session,
}
/// Bounded operational state; contains no repository, source or caller data.
#[derive(Clone, Debug, Serialize)]
pub struct PreparationSnapshot {
    pub state: PreparationState,
    pub stage: Option<PreparationStage>,
    pub operation_age_seconds: Option<f64>,
    pub aftermath_age_seconds: Option<f64>,
    pub refusals: u64,
    pub timeouts: u64,
    pub revision: u64,
    pub admission_closed: bool,
    pub occupied: bool,
}
struct Active {
    id: u64,
    started: Instant,
    stage: PreparationStage,
    polled: bool,
    aftermath: Option<Instant>,
    faulted: bool,
}
#[derive(Default)]
struct Inner {
    active: Option<Active>,
    // Retain explicit ownership even after the request disappears. Never abort
    // this handle to manufacture capacity; runtime loss faults the cell.
    task: Option<JoinHandle<()>>,
    closed: bool,
    revision: u64,
    refusals: u64,
    timeouts: u64,
}
#[derive(Default)]
pub(crate) struct Owner {
    inner: Mutex<Inner>,
    changed: Notify,
}
fn global() -> &'static Arc<Owner> {
    static OWNER: OnceLock<Arc<Owner>> = OnceLock::new();
    OWNER.get_or_init(|| Arc::new(Owner::default()))
}
pub fn preparation_snapshot() -> PreparationSnapshot {
    global().snapshot()
}
/// Stop new preparation admission permanently and report ownership honestly.
/// The caller supplies the existing process deadline; no new shutdown budget.
pub async fn shutdown_preparation_until(deadline: Instant) -> PreparationSnapshot {
    global().shutdown_until(deadline).await
}
fn timeout() -> ReaderError {
    err(ErrorKind::Storage, "Reader timed out after 90 seconds")
}
fn abandoned() -> ReaderError {
    err(ErrorKind::Storage, "Source preparation abandoned")
}
fn unavailable() -> ReaderError {
    err(ErrorKind::Storage, "Source preparation unavailable")
}

impl Owner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn snapshot(&self) -> PreparationSnapshot {
        let inner = self.lock();
        let now = Instant::now();
        let active = inner.active.as_ref();
        let aftermath_age = active
            .and_then(|a| a.aftermath)
            .map(|t| now.saturating_duration_since(t));
        let state = match active {
            Some(a) if a.faulted => PreparationState::Faulted,
            Some(_) if aftermath_age.is_some_and(|age| age >= OVERDUE) => PreparationState::Overdue,
            Some(_) if aftermath_age.is_some() => PreparationState::Aftermath,
            Some(_) => PreparationState::Active,
            None if inner.closed => PreparationState::Closed,
            None => PreparationState::Available,
        };
        PreparationSnapshot {
            state,
            stage: active.map(|a| a.stage),
            operation_age_seconds: active
                .map(|a| now.saturating_duration_since(a.started).as_secs_f64()),
            aftermath_age_seconds: aftermath_age.map(|d| d.as_secs_f64()),
            refusals: inner.refusals,
            timeouts: inner.timeouts,
            revision: inner.revision,
            admission_closed: inner.closed || active.is_some_and(|a| a.faulted),
            occupied: active.is_some(),
        }
    }
    fn abandon(&self, id: u64, timed_out: bool) {
        let mut inner = self.lock();
        let Some(active) = inner.active.as_mut().filter(|a| a.id == id) else {
            return;
        };
        if active.aftermath.is_none() {
            active.aftermath = Some(Instant::now());
            if timed_out {
                inner.timeouts = inner.timeouts.saturating_add(1);
            }
            inner.revision = inner.revision.saturating_add(1);
            self.changed.notify_waiters();
        }
    }
    pub(crate) async fn run<T, F, Fut>(
        self: &Arc<Self>,
        deadline: Instant,
        build: F,
    ) -> Result<T, ReaderError>
    where
        T: Send + 'static,
        F: FnOnce(Context) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, ReaderError>> + Send + 'static,
    {
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| unavailable())?;
        let (sender, receiver) = oneshot::channel();
        let id = {
            let mut inner = self.lock();
            if inner.closed || inner.active.is_some() {
                inner.refusals = inner.refusals.saturating_add(1);
                return Err(unavailable());
            }
            inner.revision = inner.revision.saturating_add(1);
            let id = inner.revision;
            inner.active = Some(Active {
                id,
                started: Instant::now(),
                stage: PreparationStage::Acquired,
                polled: false,
                aftermath: None,
                faulted: false,
            });
            let context = Context {
                owner: self.clone(),
                id,
                deadline,
            };
            let fault = FaultGuard {
                owner: self.clone(),
                id,
                completed: false,
            };
            // No user future is built or polled before acquiring the cell. The guard
            // lives in the owned future even if its runtime drops it before first poll.
            inner.task = Some(runtime.spawn(async move {
                let mut fault = fault;
                {
                    let mut state = context.owner.lock();
                    state.active.as_mut().unwrap().polled = true;
                }
                let result = if context.check().is_ok() {
                    build(context.clone()).await
                } else {
                    Err(abandoned())
                };
                // Transfer/dispose under the same lock as cancellation/deadline acceptance.
                // A late success never reaches a caller or a cache.
                let mut state = context.owner.lock();
                let active = state.active.as_ref().unwrap();
                if active.aftermath.is_some() || Instant::now() >= deadline || sender.is_closed() {
                    if Instant::now() >= deadline && active.aftermath.is_none() {
                        state.timeouts = state.timeouts.saturating_add(1);
                    }
                    drop(result);
                    let _ = sender.send(Err(timeout()));
                } else if let Err(late) = sender.send(result) {
                    drop(late);
                }
                state.active = None;
                state.task.take();
                state.revision = state.revision.saturating_add(1);
                fault.completed = true;
                context.owner.changed.notify_waiters();
            }));
            id
        };
        let mut caller = CallerGuard {
            owner: self.clone(),
            id,
            finished: false,
        };
        let result = tokio::select! { biased;
            _ = tokio::time::sleep_until(deadline) => {
                self.abandon(id, true);
                Err(timeout())
            }
            result = receiver => result.unwrap_or_else(|_| Err(unavailable())),
        };
        caller.finished = true;
        result
    }
    async fn shutdown_until(&self, deadline: Instant) -> PreparationSnapshot {
        {
            let mut state = self.lock();
            state.closed = true;
            state.revision = state.revision.saturating_add(1);
        }
        loop {
            let changed = self.changed.notified();
            // Register before checking, so completion cannot be lost.
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = self.snapshot();
            if !snapshot.occupied || snapshot.state == PreparationState::Faulted {
                return snapshot;
            }
            tokio::select! { biased;
                _ = tokio::time::sleep_until(deadline) => return self.snapshot(),
                _ = changed => {},
            }
        }
    }
}
#[derive(Clone)]
pub(crate) struct Context {
    owner: Arc<Owner>,
    id: u64,
    deadline: Instant,
}
impl Context {
    pub(crate) fn check(&self) -> Result<(), ReaderError> {
        let mut state = self.owner.lock();
        let active = state
            .active
            .as_mut()
            .filter(|a| a.id == self.id)
            .ok_or_else(abandoned)?;
        if active.aftermath.is_some() {
            return Err(abandoned());
        }
        if Instant::now() >= self.deadline {
            active.aftermath = Some(Instant::now());
            state.timeouts = state.timeouts.saturating_add(1);
            state.revision = state.revision.saturating_add(1);
            return Err(timeout());
        }
        Ok(())
    }
    /// Await an entire upstream stage even after abandonment. Check before and
    /// after it, so no later stage is polled and no late value escapes ownership.
    pub(crate) async fn stage<T>(
        &self,
        stage: PreparationStage,
        future: impl Future<Output = Result<T, ReaderError>>,
    ) -> Result<T, ReaderError> {
        tokio::pin!(future);
        let mut started = false;
        let result = futures::future::poll_fn(|cx| {
            if !started {
                // Atomic with acceptance of cancellation: either this first
                // upstream poll starts, or abandonment prevents it entirely.
                let mut state = self.owner.lock();
                let Some(active) = state.active.as_mut().filter(|a| a.id == self.id) else {
                    return std::task::Poll::Ready(Err(abandoned()));
                };
                if active.aftermath.is_some() {
                    return std::task::Poll::Ready(Err(abandoned()));
                }
                if Instant::now() >= self.deadline {
                    return std::task::Poll::Ready(Err(timeout()));
                }
                active.stage = stage;
                state.revision = state.revision.saturating_add(1);
                started = true;
                future.as_mut().poll(cx)
            } else {
                future.as_mut().poll(cx)
            }
        })
        .await;
        self.check()?;
        result
    }
}
struct CallerGuard {
    owner: Arc<Owner>,
    id: u64,
    finished: bool,
}
impl Drop for CallerGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.owner.abandon(self.id, false);
        }
    }
}
struct FaultGuard {
    owner: Arc<Owner>,
    id: u64,
    completed: bool,
}
impl Drop for FaultGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut state = self.owner.lock();
        if let Some(active) = state.active.as_mut().filter(|a| a.id == self.id) {
            if active.polled {
                active.faulted = true;
                state.closed = true;
            } else {
                state.active = None;
            }
            state.revision = state.revision.saturating_add(1);
            self.owner.changed.notify_waiters();
        }
    }
}
pub(crate) async fn run<T, F, Fut>(deadline: Instant, build: F) -> Result<T, ReaderError>
where
    T: Send + 'static,
    F: FnOnce(Context) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, ReaderError>> + Send + 'static,
{
    global().run(deadline, build).await
}

#[cfg(test)]
mod tests;

// Apply to every source-aware bootstrap/reopen config. Both fields matter: the
// upstream early return must precede its unreturned manifest-preload spawn.
pub(crate) fn disable_preload(config: &mut icechunk::RepositoryConfig) {
    let mut manifest = config.manifest.take().unwrap_or_default();
    manifest.preload = Some(icechunk::config::ManifestPreloadConfig {
        max_total_refs: Some(0),
        preload_if: Some(icechunk::config::ManifestPreloadCondition::False),
        ..Default::default()
    });
    config.manifest = Some(manifest);
}
