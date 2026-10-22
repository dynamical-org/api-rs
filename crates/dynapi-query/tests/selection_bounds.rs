//! This separate binary isolates the process-wide allocation meter.
use dynapi_query::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;
struct Observer;
impl PhaseObserver for Observer {}
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn add(n: usize) {
    let live = LIVE.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(live, Relaxed);
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(p, layout, size) };
        if !out.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            add(size);
        }
        out
    }
}
#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
fn multiple_runs_admit_label_copies_before_allocation() {
    // Ten runs and one value per run fit the value budgets. Test both a single
    // oversized label and labels that fit per run but exceed the aggregate cap.
    for label_bytes in [8 * 1024 * 1024, 512 * 1024] {
        let name = "x".repeat(label_bytes);
        let dims = ["init_time", "lead_time", "latitude", "longitude"].map(String::from);
        let view = DatasetView {
            dimensions: dims.iter().cloned().zip([10, 1, 1, 1]).collect(),
            coordinates: BTreeMap::from([
                (
                    "init_time".into(),
                    Coordinate::Time((0..10).map(|n| n * HOUR).collect()),
                ),
                ("lead_time".into(), Coordinate::Duration(vec![0])),
                ("latitude".into(), Coordinate::Numeric(vec![0.])),
                ("longitude".into(), Coordinate::Numeric(vec![0.])),
            ]),
            variables: BTreeMap::from([(
                name.clone(),
                Variable {
                    dimensions: dims.to_vec(),
                    chunks: Some(vec![Some(1); 4]),
                    ..Variable::default()
                },
            )]),
            ..DatasetView::default()
        };
        let collection = Collection {
            id: "synthetic".into(),
            kind: Kind::Forecast,
            version: None,
            catalog_url: None,
            variables: BTreeMap::from([(
                name.clone(),
                CatalogVariable {
                    dimensions: dims.to_vec(),
                    unit: None,
                },
            )]),
            point_query_reason: None,
        };
        let input = QueryInput {
            dataset: &view,
            collection: &collection,
            snapshot_id: "synthetic-snapshot",
            query_id: None,
            latitude: 0.,
            longitude: 0.,
            variables: vec![name],
            init_times: InitTimeSelection {
                after: Some(RequestDatetime::parse_str("1969-12-31T00:00:00Z").unwrap()),
                before: Some(RequestDatetime::parse_str("1970-01-02T00:00:00Z").unwrap()),
                ..InitTimeSelection::default()
            },
            max_lead_time_hours: None,
            start: None,
            end: None,
        };
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        let mut builder = BatchBuilder::new(Kind::Forecast);
        let mut budget = QueryBudget::default();
        let result = builder.push(&input, &mut budget, &mut Observer);
        let peak = PEAK.load(Relaxed).saturating_sub(baseline);
        eprintln!(
            "label={label_bytes} peak_new_bytes={peak} retained_runs={}",
            builder.selections().len()
        );
        // Independent of the 64 MiB cap: at most four half-MiB option copies
        // plus normalization and small selector bookkeeping can be retained.
        assert!(peak < 4 * 1024 * 1024, "label={label_bytes} peak={peak}");
        let error = result.expect_err("aggregate response bytes must be admitted during push");
        assert_eq!(error.status, 422);
        assert_eq!(
            error.detail,
            "Query exceeds the 67108864-byte response materialization budget"
        );
        if label_bytes == 512 * 1024 {
            assert!(!builder.selections().is_empty(), "at least one run fits");
            assert!(
                builder.selections().len() < 10,
                "aggregate admission stops later runs"
            );
        }
    }
    selected_vectors_admit_before_growth();
}

fn selected_vectors_admit_before_growth() {
    const LIMIT: u128 = 1024 * 1024;
    for kind in [Kind::Forecast, Kind::Analysis] {
        let time_name = if kind == Kind::Forecast {
            "lead_time"
        } else {
            "time"
        };
        let mut dims = vec![time_name.to_owned(), "latitude".into(), "longitude".into()];
        let mut dimensions = BTreeMap::from([
            (time_name.into(), 20_000),
            ("latitude".into(), 1),
            ("longitude".into(), 1),
        ]);
        let times = (0..20_000).map(|n| n * 1_000_000_000).collect();
        let mut coordinates = BTreeMap::from([
            (
                time_name.into(),
                if kind == Kind::Forecast {
                    Coordinate::Duration(times)
                } else {
                    Coordinate::Time(times)
                },
            ),
            ("latitude".into(), Coordinate::Numeric(vec![0.])),
            ("longitude".into(), Coordinate::Numeric(vec![0.])),
        ]);
        if kind == Kind::Forecast {
            dims.insert(0, "init_time".into());
            dimensions.insert("init_time".into(), 1);
            coordinates.insert("init_time".into(), Coordinate::Time(vec![0]));
        }
        let view = DatasetView {
            dimensions,
            coordinates,
            variables: BTreeMap::from([(
                "value".into(),
                Variable {
                    dimensions: dims.clone(),
                    chunks: Some(vec![Some(1); dims.len()]),
                    ..Variable::default()
                },
            )]),
            ..DatasetView::default()
        };
        let collection = Collection {
            id: "synthetic".into(),
            kind,
            version: None,
            catalog_url: None,
            variables: BTreeMap::from([(
                "value".into(),
                CatalogVariable {
                    dimensions: dims,
                    unit: None,
                },
            )]),
            point_query_reason: None,
        };
        let epoch = RequestDatetime::parse_str("1970-01-01T00:00:00Z").unwrap();
        let options = QueryOptions {
            variables: vec!["value".into()],
            run: Some(epoch),
            start: Some(epoch),
            ..QueryOptions::default()
        };
        let point = select_nearest_point(&view, 0., 0.).unwrap();
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        let result = select_query_bounded(
            &view,
            &collection,
            point,
            &options,
            "synthetic-snapshot",
            Some(&mut QueryBudget::default()),
            &mut MaterializationBudget::new(LIMIT),
        );
        let peak = PEAK.load(Relaxed).saturating_sub(baseline);
        eprintln!("kind={kind:?} limit={LIMIT} peak_new_bytes={peak}");
        assert!(
            peak < 256 * 1024,
            "full-axis temporaries/selectors must not precede admission: {peak}"
        );
        assert_eq!(
            result.unwrap_err().detail,
            "Query exceeds the 1048576-byte response materialization budget"
        );
    }
}
