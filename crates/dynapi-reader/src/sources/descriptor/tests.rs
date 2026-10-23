use super::*;
async fn owner(prefix: &str, region: &str) -> Arc<SessionState> {
    let config = super::super::tests::captured(prefix, None, region);
    let (policy, _) = super::super::policy(
        &StoreDescriptor::Local("/fixture".into()),
        vec![super::super::tests::declaration(prefix)],
        Some(&config),
    )
    .unwrap();
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http/repository");
    let storage = icechunk::new_local_filesystem_storage(&path).await.unwrap();
    let repo = Repository::open(
        Some(super::super::config(HashMap::new())),
        storage,
        HashMap::new(),
    )
    .await
    .unwrap();
    let session = repo
        .readonly_session(&icechunk::repository::VersionInfo::BranchTipRef(
            "main".into(),
        ))
        .await
        .unwrap();
    let snapshot = session.snapshot_id().to_string();
    Arc::new(SessionState::new(Arc::new(RwLock::new(session)), snapshot, Arc::new(policy)).unwrap())
}
fn reference(
    owner: Arc<SessionState>,
    uri: &str,
    offset: u64,
    length: u64,
) -> Arc<ResolvedVirtualRef> {
    let source = owner.policy.authorize(uri).unwrap();
    Arc::new(ResolvedVirtualRef {
        unit: UnitIdentity {
            snapshot_id: owner.snapshot_id.clone(),
            array_path: "stored".into(),
            indices: ChunkIndices(vec![0]),
            metadata_profile_id: [7; 32],
        },
        policy_id: owner.policy.id,
        owner,
        source,
        offset,
        length: NonZeroU64::new(length).unwrap(),
        checksum: None,
    })
}
#[tokio::test]
async fn mapped_ranges_preserve_opaque_identity_and_own_the_session() {
    for (prefix, region, uri, expected) in [
        (
            "s3://noaa-gfs-bdp-pds/",
            "us-east-1",
            "s3://noaa-gfs-bdp-pds/A/b~c/file.",
            "https://noaa-gfs-bdp-pds.s3.us-east-1.amazonaws.com/A/b~c/file.",
        ),
        (
            "s3://noaa-gefs-pds/",
            "us-east-1",
            "s3://noaa-gefs-pds/a/..hidden",
            "https://noaa-gefs-pds.s3.us-east-1.amazonaws.com/a/..hidden",
        ),
        (
            "s3://noaa-hrrr-bdp-pds/",
            "us-east-1",
            "s3://noaa-hrrr-bdp-pds/file",
            "https://noaa-hrrr-bdp-pds.s3.us-east-1.amazonaws.com/file",
        ),
        (
            "s3://noaa-rrfs-pds/",
            "us-east-1",
            "s3://noaa-rrfs-pds/file",
            "https://noaa-rrfs-pds.s3.us-east-1.amazonaws.com/file",
        ),
        (
            "s3://ecmwf-forecasts/",
            "eu-central-1",
            "s3://ecmwf-forecasts/file",
            "https://ecmwf-forecasts.s3.eu-central-1.amazonaws.com/file",
        ),
        (
            "gs://ecmwf-open-data/",
            "",
            "gs://ecmwf-open-data/file",
            "https://storage.googleapis.com/ecmwf-open-data/file",
        ),
        (
            "gcs://ecmwf-open-data/",
            "",
            "gcs://ecmwf-open-data/file",
            "https://storage.googleapis.com/ecmwf-open-data/file",
        ),
    ] {
        let owner = owner(prefix, region).await;
        let weak = Arc::downgrade(&owner);
        let descriptor = reference(owner.clone(), uri, 17, 23);
        drop(owner);
        let request = descriptor.clone().map(2..7).unwrap();
        assert_eq!(request.physical, 19..24);
        assert_eq!(request.logical, 2..7);
        assert_eq!(request.url.as_str(), expected);
        assert_eq!(request.reference.unit.array_path.as_ref(), "stored");
        assert_eq!(request.reference.unit.indices.0, [0]);
        assert_eq!(request.reference.unit.metadata_profile_id, [7; 32]);
        assert_eq!(
            request.reference.unit.snapshot_id,
            request.reference.owner.snapshot_id
        );
        assert_eq!(
            request.reference.policy_id,
            request.reference.owner.policy.id
        );
        assert_eq!(request.reference.source.original.as_ref(), uri);
        assert!(weak.upgrade().is_some());
        drop(descriptor);
        assert!(weak.upgrade().is_some());
        drop(request);
        assert!(weak.upgrade().is_none());
    }
}
#[tokio::test]
async fn range_math_never_clamps_or_wraps() {
    let owner = owner("s3://example-bucket/", "us-east-1").await;
    let descriptor = reference(owner.clone(), "s3://example-bucket/file", 7, 11);
    for range in [
        0..0,
        11..11,
        Range { start: 5, end: 4 },
        0..12,
        12..13,
        u64::MAX - 1..u64::MAX,
    ] {
        assert!(descriptor.clone().map(range).is_err());
    }
    assert_eq!(descriptor.map(0..11).unwrap().physical, 7..18);
    assert!(
        reference(owner, "s3://example-bucket/file", u64::MAX - 1, 4)
            .map(0..4)
            .is_err()
    );
}
#[test]
fn only_exact_attested_chunk_keys_bind_units() {
    let array = AttestedArray {
        path: "a".into(),
        shape: vec![9, 5],
        chunks: vec![4, 2],
        profile: [1; 32],
    };
    assert_eq!(array.indices("a/c/2/2").unwrap().0, [2, 2]);
    for key in [
        "a/c/3/0",
        "a/c/0/3",
        "a/c/00/0",
        "a/c/+0/0",
        "a/c/-1/0",
        "a/c/0",
        "a/c/0/0/0",
        "a/c/0//0",
        "a/c/0/../0",
        "a/c/%30/0",
        "a/c/4294967296/0",
        "a/c/0/0/",
        "other/c/0/0",
        "ab/c/0/0",
        "/a/c/0/0",
        "a/c0/0",
        "a/c/０/0",
    ] {
        assert!(array.indices(key).is_err(), "{key}");
    }
    let scalar = AttestedArray {
        path: "s".into(),
        shape: vec![],
        chunks: vec![],
        profile: [2; 32],
    };
    assert!(scalar.indices("s/c").unwrap().0.is_empty());
    for key in ["s/c/", "s/c/0", "s/c0"] {
        assert!(scalar.indices(key).is_err());
    }
}
#[tokio::test]
async fn native_object_store_provider_arms_are_denied_before_client_or_io() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for prefix in [
        "s3://example-bucket/",
        "gs://example-bucket/",
        "gcs://example-bucket/",
    ] {
        let config = super::super::tests::captured(prefix, Some("source"), "us-east-1");
        let (_, mut recipe) = super::super::policy(
            &StoreDescriptor::Local("/fixture".into()),
            vec![super::super::tests::declaration(prefix)],
            Some(&config),
        )
        .unwrap();
        assert!(recipe.credentials.is_empty());
        // Make an accidental native S3 dispatch observable only on loopback.
        // GCS receives an invalid option, so a client-construction attempt also
        // fails this exact Unauthorized assertion without reaching the network.
        match &mut recipe.containers.get_mut(prefix).unwrap().store {
            ObjectStoreConfig::S3(options) => {
                options.endpoint_url = Some(format!("http://{}", listener.local_addr().unwrap()));
                options.allow_http = true;
            }
            ObjectStoreConfig::Gcs(options) => {
                options.insert("not_a_supported_option".into(), "fixture".into());
            }
            _ => unreachable!(),
        }
        let resolver = icechunk::virtual_chunks::VirtualChunkResolver::new(
            recipe.containers.into_values(),
            recipe.credentials,
            Default::default(),
        );
        for uri in [format!("{prefix}file"), "vcc://source/file".into()] {
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                resolver.fetch_chunk(&uri, &(0..4), None),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(matches!(error.kind(),icechunk::format::manifest::VirtualReferenceErrorKind::UnauthorizedVirtualChunkContainer{..}),"{error:?}");
        }
    }
    assert!(
        matches!(listener.into_std().unwrap().accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn present_empty_references_are_errors_not_absent_fill() {
    use icechunk::format::manifest::{VirtualChunkLocation, VirtualChunkRef};
    let reference = ChunkPayload::Virtual(VirtualChunkRef {
        location: VirtualChunkLocation::from_url("s3://example-bucket/file").unwrap(),
        offset: 7,
        length: 0,
        checksum: None,
    });
    let error = encoded_length(&reference).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode);
    assert_eq!(error.message, "Empty encoded chunk");
    assert!(encoded_length(&ChunkPayload::Inline(bytes::Bytes::new())).is_err());
}
