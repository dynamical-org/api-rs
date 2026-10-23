//! Actual Icechunk metadata requests against a Rust-owned loopback repository.
use super::*;
use icechunk::{
    Repository,
    config::{ManifestConfig, ManifestPreloadCondition, ManifestPreloadConfig},
    repository::VersionInfo,
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};

struct Fixture(PathBuf);
impl Fixture {
    async fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "dynapi-source-owner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fn copy(source: &Path, dest: &Path) {
            std::fs::create_dir_all(dest).unwrap();
            for entry in std::fs::read_dir(source).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    copy(&entry.path(), &dest.join(entry.file_name()));
                } else {
                    std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
                }
            }
        }
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http/repository"),
            &path,
        );
        let storage = icechunk::new_local_filesystem_storage(&path).await.unwrap();
        let repo = Repository::open(
            Some(icechunk::RepositoryConfig {
                manifest: Some(ManifestConfig {
                    preload: Some(ManifestPreloadConfig {
                        max_total_refs: Some(100_000),
                        preload_if: Some(ManifestPreloadCondition::True),
                        max_arrays_to_scan: Some(100),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            storage,
            Default::default(),
        )
        .await
        .unwrap();
        repo.save_config().await.unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
struct Gate {
    path: &'static str,
    occurrence: usize,
    released: AtomicBool,
    changed: Notify,
}
impl Gate {
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
struct Server {
    url: String,
    storage: Arc<dyn icechunk_storage::Storage + Send + Sync>,
    requests: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    gates: Vec<Arc<Gate>>,
    task: JoinHandle<()>,
}
impl Server {
    async fn new(fixture: &Fixture, paused: &[(&'static str, usize)]) -> Self {
        Self::with_fault(fixture, paused, None).await
    }
    async fn with_fault(
        fixture: &Fixture,
        paused: &[(&'static str, usize)],
        fault: Option<&'static str>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let gates: Vec<_> = paused
            .iter()
            .map(|(path, occurrence)| {
                Arc::new(Gate {
                    path,
                    occurrence: *occurrence,
                    released: AtomicBool::new(false),
                    changed: Notify::new(),
                })
            })
            .collect();
        let task = tokio::spawn({
            let root = fixture.0.clone();
            let requests = requests.clone();
            let active = active.clone();
            let gates = gates.clone();
            async move {
                let mut tasks = JoinSet::new();
                loop {
                    tokio::select! {
                        result = listener.accept() => {
                            let (mut socket, _) = result.unwrap(); let root = root.clone(); let requests = requests.clone(); let active = active.clone(); let gates = gates.clone();
                            tasks.spawn(async move {
                                let mut request = Vec::new();
                                loop { let mut chunk = [0; 1024]; let n = socket.read(&mut chunk).await.unwrap(); if n == 0 { return; }
                                    request.extend_from_slice(&chunk[..n]); assert!(request.len()<16384);
                                    if request.windows(4).any(|s|s==b"\r\n\r\n") { break; }
                                }
                                let request = std::str::from_utf8(&request).unwrap();
                                let path = request.split_whitespace().nth(1).unwrap().trim_start_matches('/').to_owned();
                                assert!(path.split('/').all(|s| !["", ".", ".."].contains(&s)));
                                let occurrence = { let mut records=requests.lock().unwrap(); records.push(path.clone()); records.iter().filter(|s|**s==path).count() };
                                active.fetch_add(1,Ordering::SeqCst);
                                for gate in &gates { if gate.path==path && gate.occurrence==occurrence {
                                    loop { let changed=gate.changed.notified(); tokio::pin!(changed); changed.as_mut().enable(); if gate.released.load(Ordering::SeqCst) { break; } changed.await; }
                                }}
                                let data=std::fs::read(root.join(&path));
                                let response=if fault==Some(path.as_str()){b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()}else{match data {
                                    Err(_) => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                                    Ok(data) => {
                                        let range=request.lines().find_map(|s|s.strip_prefix("range: bytes=")).map(|s|{
                                            let (start,end)=s.trim().split_once('-').unwrap(); start.parse::<usize>().unwrap()..end.parse::<usize>().unwrap()+1
                                        });
                                        let mut headers=format!("HTTP/1.1 {} OK\r\nContent-Length: {}\r\nETag: \"fixture\"\r\nLast-Modified: Wed, 21 Oct 2026 00:00:00 GMT\r\nConnection: close\r\n", if range.is_some(){206}else{200}, range.as_ref().map_or(data.len(),|r|r.len()));
                                        if let Some(range)=&range { headers.push_str(&format!("Content-Range: bytes {}-{}/{}\r\n",range.start,range.end-1,data.len())); }
                                        headers.push_str("\r\n"); let mut response=headers.into_bytes(); response.extend_from_slice(&data[range.unwrap_or(0..data.len())]); response
                                    }
                                }};
                                let _=socket.write_all(&response).await;
                                active.fetch_sub(1,Ordering::SeqCst);
                            });
                        }
                        Some(result)=tasks.join_next()=>{result.unwrap();}
                    }
                }
            }
        });
        Self {
            url: url.clone(),
            storage: Arc::new(crate::repository_http::BoundedRepositoryStorage::new(&url).unwrap()),
            requests,
            active,
            gates,
            task,
        }
    }
    async fn reached(&self, path: &str, occurrence: usize) {
        for _ in 0..100_000 {
            if self
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.as_str() == path)
                .count()
                >= occurrence
            {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "missing request {path} {occurrence}: {:?}",
            self.requests.lock().unwrap()
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn config() -> icechunk::RepositoryConfig {
    let mut config = icechunk::RepositoryConfig {
        virtual_chunk_containers: Some(Default::default()),
        ..Default::default()
    };
    super::super::disable_preload(&mut config);
    config
}
#[tokio::test(start_paused = true)]
async fn actual_bootstrap_joins_spec_and_config_after_expiry_or_cancel() {
    let fixture = Fixture::new().await;
    // Each optimistic request may finish first, or both can remain paused.
    for paused in [
        vec![("repo", 1)],
        vec![("config.yaml", 1)],
        vec![("repo", 1), ("config.yaml", 1)],
    ] {
        for expired in [false, true] {
            let server = Server::new(&fixture, &paused).await;
            let owner = Arc::new(Owner::default());
            let started = Arc::new(Notify::new());
            let proceed = Arc::new(Notify::new());
            let deadline = Instant::now() + OPEN_TIMEOUT;
            let job = tokio::spawn({
                let owner = owner.clone();
                let storage = server.storage.clone();
                let started = started.clone();
                let proceed = proceed.clone();
                async move {
                    owner
                        .run(deadline, move |ctx| async move {
                            // Keep transport deadlines independent of the absolute deadline.
                            started.notify_one();
                            proceed.notified().await;
                            ctx.stage(PreparationStage::Bootstrap, async move {
                                Repository::open(Some(config()), storage, Default::default())
                                    .await
                                    .map_err(|e| err(ErrorKind::Storage, e))
                            })
                            .await
                            .map(|_| ())
                        })
                        .await
                }
            });
            started.notified().await;
            if expired {
                tokio::time::advance(Duration::from_secs(80)).await;
            }
            proceed.notify_one();
            for (path, occurrence) in &paused {
                server.reached(path, *occurrence).await;
            }
            if expired {
                tokio::time::advance(Duration::from_secs(10)).await;
                assert!(job.await.unwrap().is_err());
            } else {
                job.abort();
                let _ = job.await;
            }
            settled(&owner, PreparationState::Aftermath).await;
            let requests = server.requests.lock().unwrap().len();
            let active = server.active.load(Ordering::SeqCst);
            assert!(active > 0);
            for _ in 0..8 {
                assert!(
                    owner
                        .run(Instant::now() + OPEN_TIMEOUT, |_| async {
                            panic!("replacement");
                            #[allow(unreachable_code)]
                            Ok(())
                        })
                        .await
                        .is_err()
                );
            }
            assert_eq!(server.requests.lock().unwrap().len(), requests);
            assert_eq!(server.active.load(Ordering::SeqCst), active);
            if server.gates.len() == 2 {
                server.gates[0].release();
                for _ in 0..30 {
                    tokio::task::yield_now().await;
                }
                assert!(owner.snapshot().occupied);
            }
            for gate in &server.gates {
                gate.release();
            }
            settled(&owner, PreparationState::Available).await;
            assert_eq!(server.active.load(Ordering::SeqCst), 0);
        }
    }
}
#[tokio::test]
async fn persisted_preload_is_disabled_through_reopen_and_sessions() {
    let fixture = Fixture::new().await;
    let server = Server::new(&fixture, &[]).await;
    let original = Repository::fetch_config(server.storage.clone())
        .await
        .unwrap()
        .unwrap()
        .0;
    assert!(matches!(
        original.manifest().preload().preload_if(),
        ManifestPreloadCondition::True
    ));
    let bootstrap = Repository::open(Some(config()), server.storage.clone(), Default::default())
        .await
        .unwrap();
    let reopened = bootstrap
        .reopen(Some(config()), Some(Default::default()))
        .await
        .unwrap();
    for repo in [&bootstrap, &reopened] {
        assert_eq!(repo.config().manifest().preload().max_total_refs(), 0);
        assert!(matches!(
            repo.config().manifest().preload().preload_if(),
            ManifestPreloadCondition::False
        ));
        for _ in 0..3 {
            let session = repo
                .readonly_session(&VersionInfo::BranchTipRef("main".into()))
                .await
                .unwrap();
            drop(session);
            for _ in 0..30 {
                tokio::task::yield_now().await;
            }
        }
    }
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("manifests/")),
        "{:?}",
        server.requests.lock().unwrap()
    );
    // Ordinary demanded metadata is still available.
    let session = reopened
        .readonly_session(&VersionInfo::BranchTipRef("main".into()))
        .await
        .unwrap();
    let nodes = session
        .list_nodes(&icechunk::format::Path::root())
        .await
        .unwrap();
    let nodes: Vec<_> = nodes.collect::<Result<_, _>>().unwrap();
    let array = nodes
        .into_iter()
        .find(|n| {
            matches!(
                n.node_data,
                icechunk::format::snapshot::NodeData::Array { .. }
            )
        })
        .unwrap();
    let rank = match array.node_data {
        icechunk::format::snapshot::NodeData::Array { shape, .. } => shape.len(),
        _ => unreachable!(),
    };
    session
        .get_chunk_ref(&array.path, &icechunk::format::ChunkIndices(vec![0; rank]))
        .await
        .unwrap();
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("manifests/"))
    );
}

#[tokio::test(start_paused = true)]
async fn capture_and_session_late_results_drain_without_delivery() {
    let fixture = Fixture::new().await;
    for stage in [PreparationStage::Capture, PreparationStage::Session] {
        for expired in [false, true] {
            let server = Server::new(&fixture, &[("repo", 2)]).await;
            let owner = Arc::new(Owner::default());
            let entered = Arc::new(Notify::new());
            let proceed = Arc::new(Notify::new());
            let deadline = Instant::now() + OPEN_TIMEOUT;
            let delivered = Arc::new(AtomicUsize::new(0));
            let retained = Arc::new(Mutex::new(std::sync::Weak::<Vec<u8>>::new()));
            let job = tokio::spawn({
                let owner = owner.clone();
                let storage = server.storage.clone();
                let entered = entered.clone();
                let proceed = proceed.clone();
                let delivered = delivered.clone();
                let retained = retained.clone();
                async move {
                    owner
                        .run(deadline, move |ctx| async move {
                            let repo = ctx
                                .stage(PreparationStage::Bootstrap, async {
                                    Repository::open(
                                        Some(config()),
                                        storage.clone(),
                                        Default::default(),
                                    )
                                    .await
                                    .map_err(|e| err(ErrorKind::Storage, e))
                                })
                                .await?;
                            entered.notify_one();
                            proceed.notified().await;
                            ctx.stage(stage, async {
                                let held = Arc::new(vec![7u8; 256 * 1024]);
                                *retained.lock().unwrap() = Arc::downgrade(&held);
                                match stage {
                                    PreparationStage::Capture => {
                                        Repository::fetch_config(storage)
                                            .await
                                            .map_err(|e| err(ErrorKind::Storage, e))?;
                                    }
                                    PreparationStage::Session => {
                                        repo.readonly_session(&VersionInfo::BranchTipRef(
                                            "main".into(),
                                        ))
                                        .await
                                        .map_err(|e| err(ErrorKind::Snapshot, e))?;
                                    }
                                    _ => unreachable!(),
                                }
                                Ok(held)
                            })
                            .await?;
                            delivered.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .await
                }
            });
            entered.notified().await;
            if expired {
                tokio::time::advance(Duration::from_secs(80)).await;
            }
            proceed.notify_one();
            server.reached("repo", 2).await;
            if expired {
                tokio::time::advance(Duration::from_secs(10)).await;
                assert!(job.await.unwrap().is_err());
            } else {
                job.abort();
                let _ = job.await;
            }
            settled(&owner, PreparationState::Aftermath).await;
            assert_eq!(owner.snapshot().stage, Some(stage));
            assert!(retained.lock().unwrap().upgrade().is_some());
            let requests = server.requests.lock().unwrap().len();
            for _ in 0..8 {
                assert!(
                    owner
                        .run(Instant::now() + OPEN_TIMEOUT, |_| async { Ok(()) })
                        .await
                        .is_err()
                );
            }
            assert_eq!(server.requests.lock().unwrap().len(), requests);
            server.gates[0].release();
            settled(&owner, PreparationState::Available).await;
            assert!(retained.lock().unwrap().upgrade().is_none());
            assert_eq!(delivered.load(Ordering::SeqCst), 0);
            assert_eq!(server.active.load(Ordering::SeqCst), 0);
            assert!(
                !server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|s| s.starts_with("manifests/"))
            );
        }
    }
}

#[tokio::test]
async fn source_aware_http_only_preparation_does_not_capture_routing() {
    let _serial = crate::preparation::SOURCE_TEST_LOCK.lock().await;
    let fixture = Fixture::new().await;
    for provider in [false, true] {
        let server = Server::new(&fixture, &[]).await;
        let declarations = if provider {
            vec![crate::VirtualSourceDeclaration::AnonymousS3 {
                url_prefix: "s3://example-bucket/".into(),
                name: None,
            }]
        } else {
            vec![crate::VirtualSourceDeclaration::Http {
                url_prefix: "https://example.test/granted/".into(),
                name: None,
            }]
        };
        let result = crate::prepare_sources(
            &crate::StoreDescriptor::Http {
                url: server.url.clone(),
            },
            &declarations,
        )
        .await;
        assert_eq!(result.is_ok(), !provider);
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.as_str() == "repo")
                .count(),
            if provider { 2 } else { 1 }
        );
        if let Ok(prepared) = result {
            // This exercises the production source recipe against persisted
            // True/nonzero preloading, not only the standalone override helper.
            let session =
                crate::ReadSession::open_prepared(&prepared, crate::DatasetVersion::Branch("main"))
                    .await
                    .unwrap();
            let actual = session.store.session();
            let guard = actual.read().await;
            assert_eq!(guard.config().manifest().preload().max_total_refs(), 0);
            assert!(matches!(
                guard.config().manifest().preload().preload_if(),
                ManifestPreloadCondition::False
            ));
            drop(guard);
            drop(session);
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert!(
                !server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|s| s.starts_with("manifests/"))
            );
        }
    }
}

#[tokio::test]
async fn bootstrap_error_does_not_release_the_other_joined_operation() {
    let fixture = Fixture::new().await;
    for (fault, paused) in [("repo", "config.yaml"), ("config.yaml", "repo")] {
        let server = Server::with_fault(&fixture, &[(paused, 1)], Some(fault)).await;
        let owner = Arc::new(Owner::default());
        let job = tokio::spawn({
            let storage = server.storage.clone();
            let owner = owner.clone();
            async move {
                owner
                    .run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                        ctx.stage(PreparationStage::Bootstrap, async {
                            Repository::open(Some(config()), storage, Default::default())
                                .await
                                .map_err(|e| err(ErrorKind::Storage, e))
                        })
                        .await
                        .map(|_| ())
                    })
                    .await
            }
        });
        server.reached(paused, 1).await;
        server.reached(fault, 1).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        job.abort();
        let _ = job.await;
        settled(&owner, PreparationState::Aftermath).await;
        assert!(server.active.load(Ordering::SeqCst) > 0);
        server.gates[0].release();
        settled(&owner, PreparationState::Available).await;
        assert_eq!(server.active.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn repeated_actual_bootstrap_cancellation_has_no_growing_owned_aftermath() {
    if std::env::var_os("DYNAPI_PREPARATION_ALLOCATION_CHILD").is_none() {
        let status=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","preparation::tests::upstream::repeated_actual_bootstrap_cancellation_has_no_growing_owned_aftermath","--nocapture"])
            .env("DYNAPI_PREPARATION_ALLOCATION_CHILD","1").status().unwrap();
        assert!(status.success());
        return;
    }
    let fixture = Fixture::new().await;
    let owner = Arc::new(Owner::default());
    let all_threads = crate::allocation_tests::AllThreads::start();
    let measurement = crate::allocation_tests::Measurement::start();
    let mut after = Vec::with_capacity(12);
    let mut requests = 0;
    for _ in 0..12 {
        let mut server = Server::new(&fixture, &[("config.yaml", 1)]).await;
        let job = tokio::spawn({
            let storage = server.storage.clone();
            let owner = owner.clone();
            async move {
                owner
                    .run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                        ctx.stage(PreparationStage::Bootstrap, async {
                            Repository::open(Some(config()), storage, Default::default())
                                .await
                                .map_err(|e| err(ErrorKind::Storage, e))
                        })
                        .await
                        .map(|_| ())
                    })
                    .await
            }
        });
        server.reached("config.yaml", 1).await;
        server.reached("repo", 1).await;
        job.abort();
        let _ = job.await;
        settled(&owner, PreparationState::Aftermath).await;
        let before = server.requests.lock().unwrap().len();
        for _ in 0..8 {
            assert!(
                owner
                    .run(Instant::now() + OPEN_TIMEOUT, |_| async { Ok(()) })
                    .await
                    .is_err()
            );
        }
        assert_eq!(server.requests.lock().unwrap().len(), before);
        server.gates[0].release();
        settled(&owner, PreparationState::Available).await;
        assert_eq!(server.active.load(Ordering::SeqCst), 0);
        requests += server.requests.lock().unwrap().len();
        server.task.abort();
        let _ = (&mut server.task).await;
        drop(server);
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        after.push(all_threads.live());
    }
    let counts = measurement.counts();
    eprintln!(
        "actual bootstrap cycles=12 requests={requests} thread_scoped_peak={} thread_scoped_live={} foreign={}",
        counts.peak, counts.live, counts.foreign_deallocation
    );
    eprintln!(
        "actual bootstrap all-thread baseline={} peak={} live={} post_drain={after:?}",
        all_threads.baseline,
        all_threads.peak(),
        all_threads.live()
    );
    assert!(after.last().unwrap().saturating_sub(after[0]) < 64 * 1024);
}
