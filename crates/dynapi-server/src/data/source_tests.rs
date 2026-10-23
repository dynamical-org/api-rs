//! Rust-owned stored-coordinate fixtures, with provider routing metadata only.
use super::*;
use dynapi_reader::{StoreDescriptor, VirtualSourceDeclaration};
use icechunk::{
    ObjectStoreConfig, Repository, RepositoryConfig, config::S3Options,
    virtual_chunks::VirtualChunkContainer,
};
use std::{collections::HashMap, path::Path};
fn copy(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}
#[test]
fn fresh_source_capture_precedes_pinned_and_branch_cache_hits() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fixture = tempfile::tempdir().unwrap();
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../dynapi-dataset/tests/fixtures/forecast/repository"),
        fixture.path(),
    );
    let prefix = "s3://example-chunks/data/";
    let save = |region: &str, name: &str| {
        runtime.block_on(async {
            let storage = icechunk::new_local_filesystem_storage(fixture.path())
                .await
                .unwrap();
            let container = VirtualChunkContainer::new_named(
                name.into(),
                prefix.into(),
                ObjectStoreConfig::S3(S3Options::default().with_region(region)),
            )
            .unwrap();
            let mut config = RepositoryConfig {
                virtual_chunk_containers: Some(HashMap::from([(prefix.into(), container)])),
                ..Default::default()
            };
            config.manifest = Some(icechunk::config::ManifestConfig {
                preload: Some(icechunk::config::ManifestPreloadConfig {
                    max_total_refs: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            });
            Repository::open(Some(config), storage, HashMap::new())
                .await
                .unwrap()
                .save_config()
                .await
                .unwrap();
        })
    };
    let config = Config::from_values(&BTreeMap::new()).unwrap();
    let catalog = StacCatalog::new(
        "https://catalog.example.test/catalog.json",
        dynapi_catalog::refresh::CatalogConfig::default(),
    )
    .unwrap();
    let data = Arc::new(
        DataService::new(&config, Arc::new(crate::Monotonic::default()), catalog).unwrap(),
    );
    let product =
        super::tests::recipe_product("https://data.dynamical.org/example/repository/", "s3");
    let snapshot: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../dynapi-dataset/tests/fixtures/forecast/input.json"),
        )
        .unwrap(),
    )
    .unwrap();
    for pinned in [None, snapshot["snapshot_id"].as_str()] {
        let key = DatasetKey::new(&product, pinned, Profile::Default);
        let recipe = || dynapi_catalog::ReaderSourceConfig {
            store: StoreDescriptor::Local(fixture.path().to_str().unwrap().into()),
            sources: vec![VirtualSourceDeclaration::AnonymousS3 {
                url_prefix: prefix.into(),
                name: None,
            }],
        };
        save("us-east-1", "first");
        let (first, status) = data
            .open_source_recipe(key.clone(), recipe(), runtime.handle())
            .unwrap();
        assert_eq!(status, dynapi_catalog::cache::CacheStatus::Miss);
        let (same, status) = data
            .open_source_recipe(key.clone(), recipe(), runtime.handle())
            .unwrap();
        assert_eq!(status, dynapi_catalog::cache::CacheStatus::Hit);
        assert!(Arc::ptr_eq(&first, &same));
        save("eu-central-1", "first");
        let (changed, status) = data
            .open_source_recipe(key.clone(), recipe(), runtime.handle())
            .unwrap();
        assert_eq!(status, dynapi_catalog::cache::CacheStatus::Miss);
        assert!(!Arc::ptr_eq(&first, &changed));
        save("eu-central-1", "second");
        let (_, status) = data
            .open_source_recipe(key.clone(), recipe(), runtime.handle())
            .unwrap();
        assert_eq!(status, dynapi_catalog::cache::CacheStatus::Miss);
        save("us-west-2", "first");
        assert!(
            matches!(data.open_source_recipe(key.clone(),recipe(),runtime.handle()),Err(SourceOpenError::Query(error)) if error.status==500)
        );
        save("us-east-1", "first");
        let (original, status) = data
            .open_source_recipe(key, recipe(), runtime.handle())
            .unwrap();
        assert_eq!(status, dynapi_catalog::cache::CacheStatus::Hit);
        assert!(Arc::ptr_eq(&first, &original));
    }
    same_owner_across_apps_and_workers(
        &runtime,
        &config,
        data,
        DatasetKey::new(&product, None, Profile::Default),
        dynapi_catalog::ReaderSourceConfig {
            store: StoreDescriptor::Local(fixture.path().to_str().unwrap().into()),
            sources: vec![VirtualSourceDeclaration::AnonymousS3 {
                url_prefix: prefix.into(),
                name: None,
            }],
        },
    );
}

#[test]
fn preparation_saturation_keeps_the_configured_busy_http_response() {
    let clock: SharedClock = Arc::new(crate::Monotonic::default());
    let gate = ComputeGate::new(6, 17, clock).unwrap();
    let error = dynapi_reader::ReaderError {
        kind: dynapi_reader::ErrorKind::Storage,
        message: "Source preparation unavailable".into(),
        virtual_chunk_fetch: false,
        virtual_failure: Some(dynapi_reader::VirtualFailureKind::PreparationUnavailable),
        exception_class: None,
    };
    let response = SourceOpenError::reader(error).response(&gate);
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["retry-after"], "17");
    assert_eq!(
        response.extensions().get::<ErrorKind>(),
        Some(&ErrorKind::ServerBusy)
    );
}

fn same_owner_across_apps_and_workers(
    runtime: &tokio::runtime::Runtime,
    config: &Config,
    data: Arc<DataService>,
    key: DatasetKey,
    recipe: dynapi_catalog::ReaderSourceConfig,
) {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Notify,
        task::JoinSet,
    };
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let released = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let (url, origin) = runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn({
            let entered = entered.clone();
            let release = release.clone();
            let released = released.clone();
            let requests = requests.clone();
            async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (mut socket, _) = accepted.unwrap();
                            let entered = entered.clone();
                            let release = release.clone();
                            let released = released.clone();
                            let requests = requests.clone();
                            connections.spawn(async move {
                                let mut request = Vec::new();
                                loop {
                                    let mut bytes = [0u8; 1024];
                                    let n = socket.read(&mut bytes).await.unwrap();
                                    if n == 0 {
                                        return;
                                    }
                                    request.extend_from_slice(&bytes[..n]);
                                    assert!(request.len() < 16384);
                                    if request.windows(4).any(|s| s == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                                requests.fetch_add(1, Ordering::SeqCst);
                                entered.notify_one();
                                loop {
                                    let wake = release.notified();
                                    tokio::pin!(wake);
                                    wake.as_mut().enable();
                                    if released.load(Ordering::SeqCst) {
                                        break;
                                    }
                                    wake.await;
                                }
                                let _ = socket.write_all(
                                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                ).await;
                            });
                        }
                        Some(result) = connections.join_next() => {
                            result.unwrap();
                        }
                    }
                }
            }
        });
        (url, task)
    });
    let active = runtime.spawn(async move {
        dynapi_reader::prepare_sources(&StoreDescriptor::Http { url }, &[]).await
    });
    runtime.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while requests.load(Ordering::SeqCst) < 3 {
                entered.notified().await;
            }
        })
        .await
        .unwrap();
        active.abort();
        let _ = active.await;
    });
    assert_eq!(
        dynapi_reader::preparation_snapshot().state,
        dynapi_reader::PreparationState::Aftermath
    );
    let before = requests.load(Ordering::SeqCst);
    let other = Arc::new(
        DataService::new(
            config,
            Arc::new(crate::Monotonic::default()),
            data.catalog.clone(),
        )
        .unwrap(),
    );
    let clock: SharedClock = Arc::new(crate::Monotonic::default());
    let gate = ComputeGate::new(6, 5, clock).unwrap();
    for service in [data, other] {
        let key = key.clone();
        let recipe = dynapi_catalog::ReaderSourceConfig {
            store: recipe.store.clone(),
            sources: recipe.sources.clone(),
        };
        runtime
            .block_on(run_with_permit(
                gate.try_acquire().unwrap(),
                move |handle| {
                    assert!(matches!(
                        service.open_source_recipe(key, recipe, &handle),
                        Err(SourceOpenError::Busy)
                    ));
                    Ok(())
                },
            ))
            .unwrap();
        assert_eq!(gate.snapshot().in_use, 0);
        assert!(dynapi_reader::preparation_snapshot().occupied);
    }
    assert_eq!(requests.load(Ordering::SeqCst), before);
    // A normal numeric/empty-authorization dataset open still succeeds.
    runtime
        .block_on(OpenedDataset::open(
            &recipe.store,
            DatasetVersion::Branch("main"),
            &[],
        ))
        .unwrap();
    runtime.block_on(async {
        let app = Arc::new(
            crate::App::new(
                config.clone(),
                Arc::new(crate::Monotonic::default()),
                Arc::new(crate::telemetry::RecordingSink::default()),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(Notify::new());
        let server = tokio::spawn({
            let stop = stop.clone();
            async move {
                let mut server = crate::transport::Server::new(listener, app);
                tokio::select! {result=server.accept()=>{result.unwrap();},_=stop.notified()=>{}}
                assert!(
                    server
                        .drain_until(
                            tokio::time::Instant::now() + std::time::Duration::from_secs(1)
                        )
                        .await
                );
            }
        });
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200"));
        assert!(dynapi_reader::preparation_snapshot().occupied);
        stop.notify_one();
        server.await.unwrap();
        released.store(true, Ordering::SeqCst);
        release.notify_waiters();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while dynapi_reader::preparation_snapshot().occupied {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        origin.abort();
        let _ = origin.await;
    });
    assert_eq!(
        dynapi_reader::preparation_snapshot().state,
        dynapi_reader::PreparationState::Available
    );
}
