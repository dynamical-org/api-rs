//! Cumulative open/load observations. No coordinates or query payloads enter
//! this registry. Snapshot/session identities are retained only for coldness.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Mutex,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenStatus {
    Hit,
    Miss,
    Revalidated,
    Failed,
}
impl OpenStatus {
    fn name(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Revalidated => "revalidated",
            Self::Failed => "failed",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LoadClass {
    Product,
    Snapshot,
    Session,
    Variable,
    Warm,
}
impl LoadClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Product => "product",
            Self::Snapshot => "snapshot",
            Self::Session => "session",
            Self::Variable => "variable",
            Self::Warm => "warm",
        }
    }
}
#[derive(Clone)]
pub struct LoadIdentity {
    pub product: String,
    pub snapshot: String,
    pub session: u64,
    pub variables: Vec<String>,
}
pub trait LoadObserver: Send + Sync {
    fn opened(&self, _product: &str, _status: OpenStatus, _elapsed_ms: u64) {}
    fn classify(&self, _load: &LoadIdentity) -> LoadClass {
        LoadClass::Warm
    }
    fn loaded(&self, _load: &LoadIdentity, _class: LoadClass, _elapsed_ms: u64, _failed: bool) {}
    fn snapshot(&self) -> Value {
        json!({})
    }
}
pub struct NoopLoadObserver;
impl LoadObserver for NoopLoadObserver {}

#[derive(Default)]
struct Total {
    count: u64,
    ms: u64,
    failures: u64,
}
impl Total {
    fn add(&mut self, elapsed: u64, failed: bool) {
        self.count = self.count.saturating_add(1);
        self.ms = self.ms.saturating_add(elapsed);
        self.failures = self.failures.saturating_add(u64::from(failed));
    }
}
#[derive(Default)]
struct Snapshot {
    sessions: VecDeque<(u64, BTreeSet<String>)>,
}
#[derive(Default)]
struct Registry {
    loaded: BTreeMap<String, VecDeque<(String, Snapshot)>>,
    opens: BTreeMap<String, BTreeMap<OpenStatus, Total>>,
    loads: BTreeMap<String, BTreeMap<LoadClass, Total>>,
}
#[derive(Default)]
pub struct LoadRegistry(Mutex<Registry>);
impl LoadObserver for LoadRegistry {
    fn opened(&self, product: &str, status: OpenStatus, elapsed_ms: u64) {
        self.0
            .lock()
            .unwrap()
            .opens
            .entry(product.to_owned())
            .or_default()
            .entry(status)
            .or_default()
            .add(elapsed_ms, false);
    }
    fn classify(&self, load: &LoadIdentity) -> LoadClass {
        let state = self.0.lock().unwrap();
        let Some(snapshots) = state.loaded.get(&load.product) else {
            return LoadClass::Product;
        };
        let Some((_, snapshot)) = snapshots.iter().find(|(id, _)| *id == load.snapshot) else {
            return LoadClass::Snapshot;
        };
        let Some((_, variables)) = snapshot.sessions.iter().find(|(id, _)| *id == load.session)
        else {
            return LoadClass::Session;
        };
        if load
            .variables
            .iter()
            .all(|variable| variables.contains(variable))
        {
            LoadClass::Warm
        } else {
            LoadClass::Variable
        }
    }
    fn loaded(&self, load: &LoadIdentity, class: LoadClass, elapsed_ms: u64, failed: bool) {
        let mut state = self.0.lock().unwrap();
        state
            .loads
            .entry(load.product.clone())
            .or_default()
            .entry(class)
            .or_default()
            .add(elapsed_ms, failed);
        if failed {
            return;
        }
        let snapshots = state.loaded.entry(load.product.clone()).or_default();
        let index = snapshots.iter().position(|(id, _)| *id == load.snapshot);
        let (id, mut snapshot) = index
            .and_then(|index| snapshots.remove(index))
            .unwrap_or_else(|| (load.snapshot.clone(), Snapshot::default()));
        let index = snapshot
            .sessions
            .iter()
            .position(|(id, _)| *id == load.session);
        let (_, mut variables) = index
            .and_then(|index| snapshot.sessions.remove(index))
            .unwrap_or_else(|| (load.session, BTreeSet::new()));
        variables.extend(load.variables.iter().cloned());
        snapshot.sessions.push_back((load.session, variables));
        while snapshot.sessions.len() > 8 {
            snapshot.sessions.pop_front();
        }
        snapshots.push_back((id, snapshot));
        while snapshots.len() > 8 {
            snapshots.pop_front();
        }
    }
    fn snapshot(&self) -> Value {
        let state = self.0.lock().unwrap();
        let opens = state
            .opens
            .iter()
            .map(|(product, statuses)| {
                (
                    product.clone(),
                    Value::Object(
                        statuses
                            .iter()
                            .map(|(status, total)| {
                                (
                                    status.name().to_owned(),
                                    json!({"count":total.count,"ms":total.ms}),
                                )
                            })
                            .collect(),
                    ),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let loads = state.loads.iter().map(|(product, classes)| (product.clone(),Value::Object(classes.iter().map(|(class,total)| (class.name().to_owned(),json!({"count":total.count,"ms":total.ms,"failures":total.failures}))).collect()))).collect::<serde_json::Map<_,_>>();
        json!({"loads":loads,"opens":opens,"productsLoaded":state.loaded.len()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn load(snapshot: &str, session: u64, variable: &str) -> LoadIdentity {
        LoadIdentity {
            product: "example-forecast".into(),
            snapshot: snapshot.into(),
            session,
            variables: vec![variable.into()],
        }
    }
    #[test]
    fn coldness_progresses_only_on_success_and_counts_failures_cumulatively() {
        let registry = LoadRegistry::default();
        let first = load("snapshot-a", 1, "temperature");
        assert_eq!(registry.classify(&first), LoadClass::Product);
        registry.loaded(&first, LoadClass::Product, 3, true);
        assert_eq!(registry.classify(&first), LoadClass::Product);
        registry.loaded(&first, LoadClass::Product, 4, false);
        assert_eq!(registry.classify(&first), LoadClass::Warm);
        assert_eq!(
            registry.classify(&load("snapshot-b", 1, "temperature")),
            LoadClass::Snapshot
        );
        assert_eq!(
            registry.classify(&load("snapshot-a", 2, "temperature")),
            LoadClass::Session
        );
        assert_eq!(
            registry.classify(&load("snapshot-a", 1, "precipitation")),
            LoadClass::Variable
        );
        registry.opened(&first.product, OpenStatus::Miss, 2);
        registry.opened(&first.product, OpenStatus::Hit, 1);
        registry.opened(&first.product, OpenStatus::Failed, 5);
        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot,
            json!({"productsLoaded":1,"opens":{"example-forecast":{"miss":{"count":1,"ms":2},"hit":{"count":1,"ms":1},"failed":{"count":1,"ms":5}}},"loads":{"example-forecast":{"product":{"count":2,"ms":7,"failures":1}}}})
        );
        let encoded = snapshot.to_string();
        for internal in [
            "snapshot-a",
            "temperature",
            "session",
            "latitude",
            "longitude",
        ] {
            assert!(!encoded.contains(internal));
        }
    }
    #[test]
    fn snapshot_and_session_lru_caps_keep_the_last_eight_successful_loads() {
        let registry = LoadRegistry::default();
        for i in 0..9 {
            let identity = load(&format!("snapshot-{i}"), 1, "temperature");
            registry.loaded(&identity, registry.classify(&identity), 1, false);
        }
        assert_eq!(
            registry.classify(&load("snapshot-0", 1, "temperature")),
            LoadClass::Snapshot
        );
        assert_eq!(
            registry.classify(&load("snapshot-1", 1, "temperature")),
            LoadClass::Warm
        );
        for session in 2..=9 {
            let identity = load("snapshot-8", session, "temperature");
            registry.loaded(&identity, registry.classify(&identity), 1, false);
        }
        assert_eq!(
            registry.classify(&load("snapshot-8", 1, "temperature")),
            LoadClass::Session
        );
        assert_eq!(
            registry.classify(&load("snapshot-8", 2, "temperature")),
            LoadClass::Warm
        );
    }
    #[test]
    fn no_op_observer_does_not_invent_registry_measurements() {
        let observer = NoopLoadObserver;
        observer.opened("example-forecast", OpenStatus::Miss, 1);
        observer.loaded(
            &load("snapshot-a", 1, "temperature"),
            LoadClass::Product,
            1,
            false,
        );
        assert_eq!(observer.snapshot(), json!({}));
    }
}
