//! Explicit anonymous capabilities. Persisted configuration supplies routing,
//! never authorization, credentials, endpoints or HTTP headers.
use crate::{
    DatasetVersion, ErrorKind, HttpContainers, HttpVirtualChunkContainer, ReadSession, ReaderError,
    StoreDescriptor, err, http_containers,
    preparation::{self, Context, OPEN_TIMEOUT, PreparationStage},
    unsupported,
};
use icechunk::{
    ObjectStoreConfig, Repository, RepositoryConfig,
    config::{Credentials, S3Options},
    virtual_chunks::VirtualChunkContainer,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::time::Instant;

#[allow(
    dead_code,
    reason = "Descriptor fetch/decode consumer follows in GRIB integration"
)]
pub(crate) mod descriptor;
pub(crate) use descriptor::SessionState;
const MAX_DECLARATIONS: usize = 64;
const MAX_TEXT: usize = 64 * 1024;
const MAX_POLICY: usize = 1024 * 1024;

/// An explicit anonymous grant for an original source prefix. No credentials,
/// endpoint, region, header, proxy or fallback options are accepted here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VirtualSourceDeclaration {
    Http {
        url_prefix: String,
        #[serde(default)]
        name: Option<String>,
    },
    AnonymousS3 {
        url_prefix: String,
        #[serde(default)]
        name: Option<String>,
    },
    AnonymousGcs {
        url_prefix: String,
        #[serde(default)]
        name: Option<String>,
    },
}
impl VirtualSourceDeclaration {
    fn prefix(&self) -> &str {
        match self {
            Self::Http { url_prefix, .. }
            | Self::AnonymousS3 { url_prefix, .. }
            | Self::AnonymousGcs { url_prefix, .. } => url_prefix,
        }
    }
    fn name(&self) -> Option<&str> {
        match self {
            Self::Http { name, .. }
            | Self::AnonymousS3 { name, .. }
            | Self::AnonymousGcs { name, .. } => name.as_deref(),
        }
    }
    fn provider(&self) -> bool {
        !matches!(self, Self::Http { .. })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Http,
    S3(String),
    Gcs,
}
#[derive(Clone, Debug)]
struct Rule {
    declaration: VirtualSourceDeclaration,
    name: Option<String>,
    route: Route,
}
pub(crate) struct SessionSourcePolicy {
    rules: Vec<Rule>,
    id: [u8; 32],
    retained: usize,
}
/// Opaque immutable captured routing. Clones preserve the original preparation
/// deadline; retaining a completed value does not reserve preparation capacity.
#[derive(Clone)]
pub struct PreparedSources {
    repository: Arc<Repository>,
    policy: Arc<SessionSourcePolicy>,
    deadline: Instant,
}
impl std::fmt::Debug for PreparedSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSources").finish_non_exhaustive()
    }
}
impl PreparedSources {
    pub fn policy_id(&self) -> [u8; 32] {
        self.policy.id
    }
    pub fn retained_policy_bytes(&self) -> usize {
        self.policy.retained
    }
}
fn invalid() -> ReaderError {
    unsupported("Unsupported virtual source policy")
}
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && ![".", ".."].contains(&name)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
}
fn bucket_ok(bucket: &str) -> bool {
    (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && bucket.as_bytes()[0].is_ascii_alphanumeric()
        && bucket.as_bytes()[bucket.len() - 1].is_ascii_alphanumeric()
        && !bucket.ends_with("--x-s3")
        && !bucket.ends_with("-s3alias")
        && !bucket.ends_with("--ol-s3")
        && !bucket.starts_with("xn--")
        && !bucket.starts_with("sthree-")
        && !bucket.starts_with("amzn-s3-demo-")
}
fn key_ok(key: &str, prefix: bool) -> bool {
    let key = if prefix {
        key.strip_suffix('/').unwrap_or(key)
    } else {
        key
    };
    (prefix || !key.is_empty())
        && key.len() <= 1024
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~-/".contains(&b))
        && (key.is_empty()
            || key
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."))
}
// Deliberately parse original bytes before WHATWG normalization can erase a
// dot segment, query, fragment or authority spelling.
fn original(uri: &str, prefix: bool) -> Result<(&str, &str, &str), ReaderError> {
    if uri.len() > 4096 {
        return Err(invalid());
    }
    let (scheme, rest) = uri.split_once("://").ok_or_else(invalid)?;
    if !["s3", "gs", "gcs"].contains(&scheme) {
        return Err(invalid());
    }
    let (bucket, key) = rest.split_once('/').ok_or_else(invalid)?;
    if !bucket_ok(bucket) || !key_ok(key, prefix) || (scheme == "s3" && !prefix && key == "soap") {
        return Err(invalid());
    }
    Ok((scheme, bucket, key))
}
fn validate(
    declarations: &[VirtualSourceDeclaration],
    store: &StoreDescriptor,
) -> Result<(), ReaderError> {
    if declarations.len() > MAX_DECLARATIONS {
        return Err(invalid());
    }
    let mut text = match store {
        StoreDescriptor::Local(s) => s.len(),
        StoreDescriptor::Http { url } => url.len(),
        StoreDescriptor::AnonymousS3 {
            bucket,
            prefix,
            region,
        } => bucket
            .len()
            .saturating_add(prefix.len())
            .saturating_add(region.len()),
    };
    for d in declarations {
        text = text
            .checked_add(d.prefix().len() + d.name().map_or(0, str::len))
            .filter(|n| *n <= MAX_TEXT)
            .ok_or_else(invalid)?;
        if d.prefix().len() > 4096
            || !d.prefix().ends_with('/')
            || d.name().is_some_and(|s| !name_ok(s))
        {
            return Err(invalid());
        }
        match d {
            VirtualSourceDeclaration::AnonymousS3 { .. }
                if original(d.prefix(), true)?.0 != "s3" =>
            {
                return Err(invalid());
            }
            VirtualSourceDeclaration::AnonymousGcs { .. }
                if !["gs", "gcs"].contains(&original(d.prefix(), true)?.0) =>
            {
                return Err(invalid());
            }
            VirtualSourceDeclaration::Http { .. } => {
                let url = reqwest::Url::parse(d.prefix()).map_err(|_| invalid())?;
                if !["http", "https"].contains(&url.scheme())
                    || url.host_str().is_none()
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.as_str() != d.prefix()
                {
                    return Err(invalid());
                }
            }
            _ => {}
        }
    }
    if text > MAX_TEXT {
        return Err(invalid());
    }
    Ok(())
}
fn field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}
fn optional(hash: &mut Sha256, value: Option<&str>) {
    hash.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        field(hash, value.as_bytes());
    }
}
fn policy(
    store: &StoreDescriptor,
    declarations: Vec<VirtualSourceDeclaration>,
    persisted: Option<&RepositoryConfig>,
) -> Result<(SessionSourcePolicy, HttpContainers), ReaderError> {
    let mut declarations = declarations;
    declarations.sort();
    declarations.dedup();
    let mut rules = Vec::with_capacity(declarations.len());
    let mut names = HashSet::new();
    let mut prefixes = HashSet::new();
    let mut regions = HashMap::new();
    let mut text = 0usize;
    for declaration in declarations {
        if !prefixes.insert(declaration.prefix().to_owned()) {
            return Err(invalid());
        }
        let captured = if declaration.provider() {
            let mut matching = persisted
                .and_then(|c| c.virtual_chunk_containers.as_ref())
                .into_iter()
                .flat_map(|c| c.values())
                .filter(|c| c.url_prefix() == declaration.prefix());
            let container = matching.next().ok_or_else(invalid)?;
            if matching.next().is_some() {
                return Err(invalid());
            }
            Some(container)
        } else {
            None
        };
        let supplied_name = declaration.name();
        let captured_name = captured.and_then(|c| c.name());
        if let (Some(a), Some(b)) = (supplied_name, captured_name)
            && a != b
        {
            return Err(invalid());
        }
        let name = supplied_name.or(captured_name);
        if let Some(name) = name
            && (!name_ok(name) || !names.insert(name.to_owned()))
        {
            return Err(invalid());
        }
        let route = match (&declaration, captured.map(|c| &c.store)) {
            (VirtualSourceDeclaration::Http { .. }, _) => Route::Http,
            (
                VirtualSourceDeclaration::AnonymousS3 { .. },
                Some(ObjectStoreConfig::S3(options)),
            ) => {
                let region = options
                    .region
                    .as_deref()
                    .filter(|s| ["us-east-1", "eu-central-1"].contains(s))
                    .ok_or_else(invalid)?;
                if options.endpoint_url.is_some()
                    || options.allow_http
                    || options.force_path_style
                    || options.requester_pays
                    || options.network_stream_timeout_seconds.is_some()
                    || options.checksum_algorithm.is_some()
                {
                    return Err(invalid());
                }
                let (_, bucket, _) = original(declaration.prefix(), true)?;
                let known_region = match bucket {
                    "noaa-gfs-bdp-pds" | "noaa-gefs-pds" | "noaa-hrrr-bdp-pds"
                    | "noaa-rrfs-pds" => Some("us-east-1"),
                    "ecmwf-forecasts" => Some("eu-central-1"),
                    _ => None,
                };
                if known_region.is_some_and(|expected| expected != region) {
                    return Err(invalid());
                }
                if regions
                    .insert(bucket.to_owned(), region.to_owned())
                    .is_some_and(|old| old != region)
                {
                    return Err(invalid());
                }
                Route::S3(region.to_owned())
            }
            (
                VirtualSourceDeclaration::AnonymousGcs { .. },
                Some(ObjectStoreConfig::Gcs(options)),
            ) if options.is_empty() => Route::Gcs,
            _ => return Err(invalid()),
        };
        text = text
            .checked_add(
                declaration.prefix().len()
                    + supplied_name.map_or(0, str::len)
                    + captured_name.map_or(0, str::len)
                    + match &route {
                        Route::S3(s) => s.len(),
                        _ => 0,
                    },
            )
            .filter(|n| *n <= MAX_TEXT)
            .ok_or_else(invalid)?;
        let name = name.map(str::to_owned);
        rules.push(Rule {
            declaration,
            name,
            route,
        });
    }
    // Assembly is bounded before constructing sanitized native maps: eight
    // copies of the admitted text plus 4KiB of fixed/map space per rule.
    // The tighter actual-capacity charge below precedes repository reopen.
    text.checked_mul(8)
        .and_then(|n| n.checked_add(rules.len() * 4096))
        .filter(|n| *n <= MAX_POLICY)
        .ok_or_else(invalid)?;
    let http: Vec<_> = rules
        .iter()
        .filter(|r| r.route == Route::Http)
        .map(|r| HttpVirtualChunkContainer {
            url_prefix: r.declaration.prefix().into(),
            name: r.name.clone(),
        })
        .collect();
    let mut recipe = http_containers(&http)?;
    for rule in rules.iter().filter(|r| r.route != Route::Http) {
        let store = match &rule.route {
            Route::S3(region) => ObjectStoreConfig::S3(
                S3Options::default()
                    .with_region(region)
                    .with_anonymous(true),
            ),
            Route::Gcs => ObjectStoreConfig::Gcs(HashMap::new()),
            Route::Http => unreachable!(),
        };
        let container = match &rule.name {
            Some(name) => VirtualChunkContainer::new_named(
                name.clone(),
                rule.declaration.prefix().into(),
                store,
            ),
            None => VirtualChunkContainer::new(rule.declaration.prefix().into(), store),
        }
        .map_err(|_| invalid())?;
        recipe
            .containers
            .insert(rule.declaration.prefix().into(), container);
        // ABSENT is denial. Some(None) would grant anonymous/environment access.
    }
    let mut hash = Sha256::new();
    field(
        &mut hash,
        b"dynapi-source-policy-v1;endpoints1;keys1;numeric1;grib1;E8388608;D16777216;preload0",
    );
    match store {
        StoreDescriptor::Local(s) => {
            field(&mut hash, b"local");
            field(&mut hash, s.as_bytes());
        }
        StoreDescriptor::Http { url } => {
            field(&mut hash, b"http");
            field(&mut hash, url.as_bytes());
        }
        StoreDescriptor::AnonymousS3 {
            bucket,
            prefix,
            region,
        } => {
            field(&mut hash, b"repository-s3");
            for s in [bucket, prefix, region] {
                field(&mut hash, s.as_bytes());
            }
        }
    }
    for rule in &rules {
        field(
            &mut hash,
            match rule.declaration {
                VirtualSourceDeclaration::Http { .. } => b"http",
                VirtualSourceDeclaration::AnonymousS3 { .. } => b"s3",
                VirtualSourceDeclaration::AnonymousGcs { .. } => b"gcs",
            },
        );
        field(&mut hash, rule.declaration.prefix().as_bytes());
        optional(&mut hash, rule.declaration.name());
        optional(&mut hash, rule.name.as_deref());
        field(
            &mut hash,
            match &rule.route {
                Route::Http => b"legacy-http",
                Route::S3(region) => region.as_bytes(),
                Route::Gcs => b"storage.googleapis.com",
            },
        );
    }
    // Conservatively charge sanitized native-map copies (config/resolver/reopen)
    // as well as our own actual Vec/String capacities. Upstream repository
    // metadata caches/decoded manifests remain the existing trusted scope.
    let rule_bytes = rules.capacity() * std::mem::size_of::<Rule>()
        + rules
            .iter()
            .map(|r| {
                r.declaration.prefix().len()
                    + r.declaration.name().map_or(0, str::len)
                    + r.name.as_ref().map_or(0, String::capacity)
                    + match &r.route {
                        Route::S3(s) => s.capacity(),
                        _ => 0,
                    }
            })
            .sum::<usize>();
    let map_bytes = recipe.containers.capacity()
        * (std::mem::size_of::<(String, VirtualChunkContainer)>() + 32)
        + recipe
            .containers
            .iter()
            .map(|(k, v)| {
                k.capacity()
                    + v.url_prefix().len()
                    + v.name.as_ref().map_or(0, String::capacity)
                    + 256
            })
            .sum::<usize>()
        + recipe.credentials.capacity()
            * (std::mem::size_of::<(String, Option<Credentials>)>() + 32)
        + recipe
            .credentials
            .keys()
            .map(String::capacity)
            .sum::<usize>();
    let retained = std::mem::size_of::<SessionSourcePolicy>() + rule_bytes + 4 * map_bytes;
    if retained > MAX_POLICY {
        return Err(invalid());
    }
    Ok((
        SessionSourcePolicy {
            rules,
            id: hash.finalize().into(),
            retained,
        },
        recipe,
    ))
}
fn config(containers: HashMap<String, VirtualChunkContainer>) -> RepositoryConfig {
    let mut config = RepositoryConfig {
        virtual_chunk_containers: Some(containers),
        storage: Some(crate::repository_settings()),
        ..Default::default()
    };
    preparation::disable_preload(&mut config);
    config
}
async fn prepare(
    ctx: &Context,
    store: StoreDescriptor,
    declarations: Vec<VirtualSourceDeclaration>,
    deadline: Instant,
) -> Result<PreparedSources, ReaderError> {
    let storage = ctx
        .stage(PreparationStage::Bootstrap, async {
            crate::new_storage(&store).await
        })
        .await?;
    let bootstrap = ctx
        .stage(PreparationStage::Bootstrap, async {
            Repository::open(
                Some(config(HashMap::new())),
                storage.clone(),
                HashMap::new(),
            )
            .await
            .map_err(|e| err(ErrorKind::Storage, e))
        })
        .await?;
    let original = if declarations.iter().any(VirtualSourceDeclaration::provider) {
        ctx.stage(PreparationStage::Capture, async {
            Repository::fetch_config(storage)
                .await
                .map_err(|e| err(ErrorKind::Storage, e))
        })
        .await?
        .map(|(config, _)| config)
    } else {
        None
    };
    let (
        policy,
        HttpContainers {
            containers,
            credentials,
        },
    ) = ctx
        .stage(PreparationStage::Validation, async {
            policy(&store, declarations, original.as_ref())
        })
        .await?;
    let mut repository = ctx
        .stage(PreparationStage::Validation, async {
            bootstrap
                .reopen(Some(config(containers)), Some(credentials))
                .await
                .map_err(|e| err(ErrorKind::Storage, e))
        })
        .await?;
    let prefixes = policy
        .rules
        .iter()
        .filter(|r| r.route == Route::Http)
        .map(|r| r.declaration.prefix().to_owned())
        .collect::<Vec<_>>();
    if !prefixes.is_empty() {
        repository.set_http_virtual_chunk_fetcher(Arc::new(
            crate::http::BoundedHttpFetcher::new(prefixes)
                .map_err(|e| err(ErrorKind::Storage, e))?,
        ));
    }
    Ok(PreparedSources {
        repository: Arc::new(repository),
        policy: Arc::new(policy),
        deadline,
    })
}
pub async fn prepare_sources(
    store: &StoreDescriptor,
    declarations: &[VirtualSourceDeclaration],
) -> Result<PreparedSources, ReaderError> {
    let deadline = Instant::now() + OPEN_TIMEOUT;
    validate(declarations, store)?;
    let store = store.clone();
    let declarations = declarations.to_vec();
    preparation::run(deadline, move |ctx| async move {
        prepare(&ctx, store, declarations, deadline).await
    })
    .await
}
fn version(version: DatasetVersion<'_>) -> Result<icechunk::repository::VersionInfo, ReaderError> {
    match version {
        DatasetVersion::Snapshot(id) => Ok(icechunk::repository::VersionInfo::SnapshotId(
            id.try_into()
                .map_err(|_| err(ErrorKind::Snapshot, format!("Unknown snapshot {id:?}")))?,
        )),
        DatasetVersion::Branch(branch) if branch.len() <= 4096 => Ok(
            icechunk::repository::VersionInfo::BranchTipRef(branch.into()),
        ),
        _ => Err(invalid()),
    }
}
async fn open(
    ctx: &Context,
    prepared: PreparedSources,
    version: icechunk::repository::VersionInfo,
) -> Result<ReadSession, ReaderError> {
    let session = ctx
        .stage(PreparationStage::Session, async {
            prepared
                .repository
                .readonly_session(&version)
                .await
                .map_err(|e| {
                    let missing = matches!(
                        e.kind(),
                        icechunk::repository::RepositoryErrorKind::SnapshotNotFound { .. }
                            | icechunk::repository::RepositoryErrorKind::FormatError(
                                icechunk::format::IcechunkFormatErrorKind::SnapshotIdNotFound { .. }
                            )
                    );
                    match &version {
                        icechunk::repository::VersionInfo::SnapshotId(id) if missing => err(
                            ErrorKind::Snapshot,
                            format!("Unknown snapshot {:?}", id.to_string()),
                        ),
                        _ => err(ErrorKind::Snapshot, e.kind()),
                    }
                })
        })
        .await?;
    let snapshot_id = session.snapshot_id().to_string();
    let store = zarrs_icechunk::AsyncIcechunkStore::new(session);
    let sources = Arc::new(SessionState::new(
        store.session(),
        snapshot_id.clone(),
        prepared.policy,
    )?);
    Ok(ReadSession {
        store,
        snapshot_id,
        sources: Some(sources),
    })
}
impl ReadSession {
    pub async fn open_prepared(
        prepared: &PreparedSources,
        requested: DatasetVersion<'_>,
    ) -> Result<Self, ReaderError> {
        let version = version(requested)?;
        let deadline = prepared.deadline;
        let prepared = prepared.clone();
        preparation::run(deadline, move |ctx| async move {
            open(&ctx, prepared, version).await
        })
        .await
    }
    pub async fn open_with_sources(
        store: &StoreDescriptor,
        requested: DatasetVersion<'_>,
        declarations: &[VirtualSourceDeclaration],
    ) -> Result<Self, ReaderError> {
        let deadline = Instant::now() + OPEN_TIMEOUT;
        validate(declarations, store)?;
        let version = version(requested)?;
        let store = store.clone();
        let declarations = declarations.to_vec();
        preparation::run(deadline, move |ctx| async move {
            let prepared = prepare(&ctx, store, declarations, deadline).await?;
            open(&ctx, prepared, version).await
        })
        .await
    }
    pub fn retained_source_policy_bytes(&self) -> usize {
        self.sources.as_ref().map_or(0, |s| s.policy.retained)
    }
}
#[cfg(test)]
mod tests;
