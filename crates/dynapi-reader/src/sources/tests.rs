use super::*;
fn store() -> StoreDescriptor {
    StoreDescriptor::Local("/fixture/repository".into())
}
pub(super) fn declaration(prefix: &str) -> VirtualSourceDeclaration {
    if prefix.starts_with("s3:") {
        VirtualSourceDeclaration::AnonymousS3 {
            url_prefix: prefix.into(),
            name: None,
        }
    } else {
        VirtualSourceDeclaration::AnonymousGcs {
            url_prefix: prefix.into(),
            name: None,
        }
    }
}
pub(super) fn captured(prefix: &str, name: Option<&str>, region: &str) -> RepositoryConfig {
    let kind = if prefix.starts_with("s3:") {
        ObjectStoreConfig::S3(S3Options::default().with_region(region))
    } else {
        ObjectStoreConfig::Gcs(HashMap::new())
    };
    let container = match name {
        Some(name) => VirtualChunkContainer::new_named(name.into(), prefix.into(), kind),
        None => VirtualChunkContainer::new(prefix.into(), kind),
    }
    .unwrap();
    RepositoryConfig {
        virtual_chunk_containers: Some(HashMap::from([(prefix.into(), container)])),
        ..Default::default()
    }
}
#[test]
fn original_keys_are_validated_before_any_url_normalization() {
    for key in ["A/b~c", "a/..hidden", "a/file.", "a/file_01.grib2", "0"] {
        assert_eq!(
            original(&format!("s3://example-bucket/{key}"), false)
                .unwrap()
                .2,
            key
        );
    }
    for key in [
        "", "/a", "a/", "a//b", "a/./b", "a/../b", ".", "..", "%2f", "%2F", "%252F", "%2e", "%25",
        "%", "a?b", "a#b", "%3F", "%23", "a\\b", "a b", "a+b", "é", "\0", "\r\n", "soap",
    ] {
        assert!(
            original(&format!("s3://example-bucket/{key}"), false).is_err(),
            "{key:?}"
        );
    }
    assert!(original("gs://example-bucket/soap", false).is_ok());
    for bucket in [
        "Example",
        "bucket.evil",
        "127.0.0.1",
        "user@bucket",
        "bucket:443",
        "bucket%2ecom",
        "a",
        "-bad",
        "bad-",
        "bucket--x-s3",
        "bucket-s3alias",
        "xn--example",
    ] {
        assert!(
            original(&format!("s3://{bucket}/key"), false).is_err(),
            "{bucket}"
        );
    }
    assert!(original(&format!("s3://example-bucket/{}", "a".repeat(1025)), false).is_err());
}
#[test]
fn policy_is_deterministic_but_names_regions_aliases_and_repository_are_identity() {
    let prefix = "s3://example-bucket/a/";
    let d = declaration(prefix);
    let original_config = captured(prefix, Some("source"), "us-east-1");
    let (p, recipe) = policy(&store(), vec![d.clone(), d.clone()], Some(&original_config)).unwrap();
    let (same, _) = policy(&store(), vec![d.clone()], Some(&original_config)).unwrap();
    assert_eq!(p.id, same.id);
    assert!(p.retained <= MAX_POLICY && p.retained > std::mem::size_of::<SessionSourcePolicy>());
    assert!(recipe.credentials.is_empty());
    assert_eq!(recipe.containers.len(), 1);
    let mut changed = original_config.clone();
    changed
        .virtual_chunk_containers
        .as_mut()
        .unwrap()
        .get_mut(prefix)
        .unwrap()
        .name = Some("other".into());
    assert_ne!(
        p.id,
        policy(&store(), vec![d.clone()], Some(&changed))
            .unwrap()
            .0
            .id
    );
    let region = captured(prefix, Some("source"), "eu-central-1");
    assert_ne!(
        p.id,
        policy(&store(), vec![d.clone()], Some(&region))
            .unwrap()
            .0
            .id
    );
    assert_ne!(
        p.id,
        policy(
            &StoreDescriptor::Local("/other".into()),
            vec![d.clone()],
            Some(&original_config)
        )
        .unwrap()
        .0
        .id
    );
    let supplied = VirtualSourceDeclaration::AnonymousS3 {
        url_prefix: prefix.into(),
        name: Some("source".into()),
    };
    assert_ne!(
        p.id,
        policy(&store(), vec![supplied], Some(&original_config))
            .unwrap()
            .0
            .id
    );
    for scheme in ["gs", "gcs"] {
        let prefix = format!("{scheme}://example-bucket/a/");
        let config = captured(&prefix, None, "");
        let policy = policy(&store(), vec![declaration(&prefix)], Some(&config))
            .unwrap()
            .0;
        assert!(policy.authorize(&format!("{prefix}file.grib2")).is_ok());
        let other = if scheme == "gs" { "gcs" } else { "gs" };
        assert!(
            policy
                .authorize(&format!("{other}://example-bucket/a/file.grib2"))
                .is_err()
        );
    }
}

#[test]
fn declarations_options_names_and_limits_fail_closed() {
    let prefix = "s3://example-bucket/a/";
    let d = declaration(prefix);
    assert!(policy(&store(), vec![d.clone()], None).is_err());
    assert!(
        policy(
            &store(),
            vec![d.clone()],
            Some(&RepositoryConfig::default())
        )
        .is_err()
    );
    for mutate in [0, 1, 2, 3, 4, 5, 6, 7] {
        let mut config = captured(prefix, None, "us-east-1");
        let ObjectStoreConfig::S3(options) = &mut config
            .virtual_chunk_containers
            .as_mut()
            .unwrap()
            .get_mut(prefix)
            .unwrap()
            .store
        else {
            unreachable!()
        };
        match mutate {
            0 => options.endpoint_url = Some("https://example.invalid".into()),
            1 => options.allow_http = true,
            2 => options.force_path_style = true,
            3 => options.requester_pays = true,
            4 => options.region = None,
            5 => options.region = Some("us-west-2".into()),
            6 => options.network_stream_timeout_seconds = Some(60),
            7 => options.checksum_algorithm = Some(icechunk::config::S3ChecksumAlgorithm::Crc32),
            _ => unreachable!(),
        }
        assert!(
            policy(&store(), vec![d.clone()], Some(&config)).is_err(),
            "option {mutate}"
        );
    }
    let config = captured(prefix, Some("source"), "us-east-1");
    assert!(
        policy(
            &store(),
            vec![VirtualSourceDeclaration::AnonymousS3 {
                url_prefix: prefix.into(),
                name: Some("other".into())
            }],
            Some(&config)
        )
        .is_err()
    );
    for name in ["", ".", "..", "a/b", "a\\b", "a%", "a?", "a#", "a b", "é"] {
        assert!(
            validate(
                &[VirtualSourceDeclaration::AnonymousS3 {
                    url_prefix: prefix.into(),
                    name: Some(name.into())
                }],
                &store()
            )
            .is_err(),
            "{name}"
        );
    }
    assert!(validate(&vec![d.clone(); 65], &store()).is_err());
    assert!(
        validate(
            std::slice::from_ref(&d),
            &StoreDescriptor::Local("a".repeat(MAX_TEXT))
        )
        .is_err()
    );
    assert!(
        validate(
            &[VirtualSourceDeclaration::AnonymousGcs {
                url_prefix: prefix.into(),
                name: None
            }],
            &store()
        )
        .is_err()
    );
    let conflicting = VirtualSourceDeclaration::AnonymousS3 {
        url_prefix: prefix.into(),
        name: Some("source".into()),
    };
    assert!(policy(&store(), vec![d, conflicting], Some(&config)).is_err());
    let mut gcs = captured("gs://example-bucket/", None, "");
    gcs.virtual_chunk_containers
        .as_mut()
        .unwrap()
        .get_mut("gs://example-bucket/")
        .unwrap()
        .store = ObjectStoreConfig::Gcs(HashMap::from([(
        "endpoint".into(),
        "https://example.invalid".into(),
    )]));
    assert!(
        policy(
            &store(),
            vec![declaration("gs://example-bucket/")],
            Some(&gcs)
        )
        .is_err()
    );
    assert!(serde_json::from_value::<VirtualSourceDeclaration>(serde_json::json!({"type":"anonymous_s3","url_prefix":prefix,"endpoint":"https://example.invalid"})).is_err());
}
#[test]
fn original_authorization_uses_longest_prefix_without_permission_union() {
    let a = "s3://example-bucket/a/";
    let b = "s3://example-bucket/a/nested/";
    let mut config = captured(a, None, "us-east-1");
    config.virtual_chunk_containers.as_mut().unwrap().extend(
        captured(b, None, "us-east-1")
            .virtual_chunk_containers
            .unwrap(),
    );
    let p = policy(
        &store(),
        vec![declaration(a), declaration(b)],
        Some(&config),
    )
    .unwrap()
    .0;
    let authorized = p.authorize("s3://example-bucket/a/nested/file").unwrap();
    assert_eq!(
        p.rules[authorized.declared_prefix_index]
            .declaration
            .prefix(),
        b
    );
    for denied in [
        "s3://example-bucket/ab/file",
        "s3://other-bucket/a/file",
        "gs://example-bucket/a/file",
        "s3://example-bucket/a/../file",
        "s3://example-bucket/a/%2Ffile",
    ] {
        assert!(p.authorize(denied).is_err(), "{denied}");
    }
    let narrow = policy(&store(), vec![declaration(b)], Some(&config))
        .unwrap()
        .0;
    assert!(narrow.authorize("s3://example-bucket/a/file").is_err());
    let mut changed = config.clone();
    if let ObjectStoreConfig::S3(options) = &mut changed
        .virtual_chunk_containers
        .as_mut()
        .unwrap()
        .get_mut(b)
        .unwrap()
        .store
    {
        options.region = Some("eu-central-1".into());
    }
    assert!(
        policy(
            &store(),
            vec![declaration(a), declaration(b)],
            Some(&changed)
        )
        .is_err()
    );
}

struct Fixture(std::path::PathBuf);
impl Fixture {
    async fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "dynapi-sources-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fn copy(source: &std::path::Path, target: &std::path::Path) {
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
        copy(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/http/repository"),
            &path,
        );
        Self(path)
    }
    fn descriptor(&self) -> StoreDescriptor {
        StoreDescriptor::Local(self.0.to_str().unwrap().into())
    }
    async fn reference(&self, location: Option<&str>, length: u64) -> String {
        use icechunk::{
            format::manifest::{VirtualChunkLocation, VirtualChunkRef},
            store::Store,
        };
        let storage = crate::new_storage(&self.descriptor()).await.unwrap();
        let config = captured("s3://example-bucket/a/", Some("source"), "us-east-1");
        let mut config = config;
        config.virtual_chunk_containers.as_mut().unwrap().extend(
            captured("gs://example-bucket/a/", Some("gcs-source"), "")
                .virtual_chunk_containers
                .unwrap(),
        );
        preparation::disable_preload(&mut config);
        let repo = Repository::open(Some(config), storage, HashMap::new())
            .await
            .unwrap();
        repo.save_config().await.unwrap();
        let session = Arc::new(tokio::sync::RwLock::new(
            repo.writable_session("main").await.unwrap(),
        ));
        let store = Store::from_session(session.clone()).await;
        if let Some(location) = location {
            store
                .set_virtual_ref(
                    "stored/c/0",
                    VirtualChunkRef {
                        location: VirtualChunkLocation::from_url(location).unwrap(),
                        offset: 7,
                        length,
                        checksum: None,
                    },
                    false,
                )
                .await
                .unwrap();
        } else {
            store.delete("stored/c/0").await.unwrap();
        }
        session
            .write()
            .await
            .commit("Synthetic source reference")
            .execute()
            .await
            .unwrap()
            .to_string()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
#[tokio::test]
async fn public_source_apis_share_one_owner_preserve_deadlines_and_fail_before_payload() {
    use crate::{Selector, VirtualFailureKind};
    let _serial = preparation::SOURCE_TEST_LOCK.lock().await;
    let fixture = Fixture::new().await;
    let declarations = vec![
        declaration("s3://example-bucket/a/"),
        declaration("gs://example-bucket/a/"),
    ];
    for uri in [
        "s3://example-bucket/a/file",
        "vcc://source/file",
        "gs://example-bucket/a/file",
        "vcc://gcs-source/file",
    ] {
        let snapshot = fixture.reference(Some(uri), 16).await;
        let session = ReadSession::open_with_sources(
            &fixture.descriptor(),
            DatasetVersion::Snapshot(&snapshot),
            &declarations,
        )
        .await
        .unwrap();
        assert!(session.retained_source_policy_bytes() > 0);
        let error = session
            .read_dataset_selection("stored", &[Selector::Index(0)], None)
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("Provider virtual numeric payload is not supported"),
            "{error}"
        );
    }
    // Named expansion cannot widen the original anonymous grant.
    for uri in [
        "vcc://source/../outside",
        "s3://example-bucket/ab/file",
        "gs://example-bucket/a/%2Ffile",
    ] {
        let snapshot = fixture.reference(Some(uri), 16).await;
        let session = ReadSession::open_with_sources(
            &fixture.descriptor(),
            DatasetVersion::Snapshot(&snapshot),
            &declarations,
        )
        .await
        .unwrap();
        assert!(
            session
                .read_dataset_selection("stored", &[Selector::Index(0)], None)
                .await
                .is_err()
        );
    }
    {
        let length = u64::MAX;
        let snapshot = fixture.reference(Some("vcc://source/file"), length).await;
        let session = ReadSession::open_with_sources(
            &fixture.descriptor(),
            DatasetVersion::Snapshot(&snapshot),
            &declarations,
        )
        .await
        .unwrap();
        assert!(
            session
                .read_dataset_selection("stored", &[Selector::Index(0)], None)
                .await
                .is_err()
        );
    }
    fixture.reference(None, 0).await;
    let prepared = prepare_sources(&fixture.descriptor(), &declarations)
        .await
        .unwrap();
    assert_eq!(
        crate::preparation_snapshot().state,
        crate::PreparationState::Available
    );
    let clone = prepared.clone();
    assert_eq!(clone.deadline, prepared.deadline);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let job = tokio::spawn({
        let entered = entered.clone();
        let release = release.clone();
        async move {
            preparation::run(Instant::now() + OPEN_TIMEOUT, move |ctx| async move {
                ctx.stage(PreparationStage::Bootstrap, async {
                    entered.notify_one();
                    release.notified().await;
                    Ok(())
                })
                .await
            })
            .await
        }
    });
    entered.notified().await;
    job.abort();
    let _ = job.await;
    assert_eq!(
        crate::preparation_snapshot().state,
        crate::PreparationState::Aftermath
    );
    for error in [
        prepare_sources(&fixture.descriptor(), &declarations)
            .await
            .unwrap_err(),
        ReadSession::open_prepared(&prepared, DatasetVersion::Branch("main"))
            .await
            .err()
            .unwrap(),
        ReadSession::open_with_sources(
            &fixture.descriptor(),
            DatasetVersion::Branch("main"),
            &declarations,
        )
        .await
        .err()
        .unwrap(),
    ] {
        assert_eq!(
            error.virtual_failure,
            Some(VirtualFailureKind::PreparationUnavailable)
        );
    }
    // Existing numeric/empty-authorization metadata callers remain independent.
    let numeric = ReadSession::open(&fixture.descriptor(), DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    assert!(!numeric.root_metadata(1024 * 1024).await.unwrap().is_empty());
    let missing = numeric
        .read_dataset_selection("stored", &[Selector::Index(0)], None)
        .await
        .unwrap();
    assert!(f32::from_le_bytes(missing.bytes.try_into().unwrap()).is_nan());
    release.notify_one();
    for _ in 0..1000 {
        if crate::preparation_snapshot().state == crate::PreparationState::Available {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        crate::preparation_snapshot().state,
        crate::PreparationState::Available
    );
    let mut expired = clone;
    expired.deadline = Instant::now() - std::time::Duration::from_secs(1);
    let error = ReadSession::open_prepared(&expired, DatasetVersion::Branch("main"))
        .await
        .err()
        .unwrap();
    assert_eq!(error.message, "Reader timed out after 90 seconds");
    assert_eq!(error.virtual_failure, None);
}

#[test]
fn known_buckets_require_the_captured_region_from_the_mapping_table() {
    for (bucket, wrong) in [
        ("noaa-gfs-bdp-pds", "eu-central-1"),
        ("noaa-gefs-pds", "eu-central-1"),
        ("noaa-hrrr-bdp-pds", "eu-central-1"),
        ("noaa-rrfs-pds", "eu-central-1"),
        ("ecmwf-forecasts", "us-east-1"),
    ] {
        let prefix = format!("s3://{bucket}/");
        let config = captured(&prefix, None, wrong);
        assert!(policy(&store(), vec![declaration(&prefix)], Some(&config)).is_err());
    }
}
#[test]
fn maximum_count_policy_assembly_and_retention_are_bounded() {
    // Warm URL parser process caches outside the current-thread measurement.
    let http = VirtualSourceDeclaration::Http {
        url_prefix: "https://example.test/a/".into(),
        name: None,
    };
    drop(policy(&store(), vec![http], None).unwrap());
    let measurement = crate::allocation_tests::Measurement::start();
    let mut declarations = Vec::new();
    for i in 0..64 {
        declarations.push(VirtualSourceDeclaration::Http {
            url_prefix: format!("https://example{i}.test/{}/", "a".repeat(240)),
            name: Some(format!("source{i}")),
        });
    }
    validate(&declarations, &store()).unwrap();
    let (policy, recipe) = policy(&store(), declarations, None).unwrap();
    assert!(policy.retained <= MAX_POLICY);
    assert_eq!(recipe.containers.len(), 128);
    drop((policy, recipe));
    let counts = measurement.counts();
    eprintln!(
        "source policy assembly peak={} live={} foreign={}",
        counts.peak, counts.live, counts.foreign_deallocation
    );
    assert!(counts.peak <= MAX_POLICY);
    assert_eq!(counts.live, 0);
    assert!(!counts.foreign_deallocation);
}
