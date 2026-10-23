//! Provisional, bounded stored-numeric reader. No application or HTTP contracts.
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::StreamExt;
use icechunk::{
    Repository,
    config::{Credentials, HttpConfig, S3Credentials, S3Options},
    repository::VersionInfo,
    virtual_chunks::VirtualChunkContainer,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zarrs::array::{Array, ArrayBytes, ArraySubset, CodecOptions};
use zarrs_icechunk::AsyncIcechunkStore;
use zarrs_storage::{
    AsyncListableStorageTraits, AsyncMaybeBytesIterator, AsyncReadableStorageTraits, MaybeBytes,
    StorageError, StoreKey, StorePrefix,
    byte_range::{ByteRange, ByteRangeIterator},
};

#[cfg(test)]
mod allocation_tests;
pub mod cf_time;
#[allow(
    dead_code,
    reason = "Private decoder qualification precedes reader integration"
)]
mod grib;
mod http;
mod repository_http;
mod scale_offset;
use scale_offset::ScaleOffset;

pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CHUNK_BYTES: usize = 56 * 1024 * 1024;
const MAX_INTEGER_CHUNK_BYTES: usize = 64 * 1024 * 1024;
const MAX_NARROW_INTEGER_CHUNK_BYTES: usize = 16 * 1024 * 1024;
const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;
/// Bound checked before adapters allocate positional selector copies.
pub const MAX_SELECTORS: usize = 1_048_576;
const MAX_CHUNKS: usize = 65_536;
const MAX_RANK: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum StoreDescriptor {
    Local(String),
    Http {
        url: String,
    },
    AnonymousS3 {
        bucket: String,
        prefix: String,
        region: String,
    },
}

/// One catalog-declared HTTP(S) container. Access is anonymous and confined to
/// this prefix; a name optionally resolves Icechunk `vcc://name/...` references.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpVirtualChunkContainer {
    pub url_prefix: String,
    #[serde(default)]
    pub name: Option<String>,
}

fn http_virtual_options() -> HashMap<String, String> {
    // This deliberately invalid native-only option disables client construction
    // before I/O if Icechunk ever falls back around the runtime callback. The
    // callback ignores these options and uses its own bounded reqwest client.
    HashMap::from([("timeout".into(), "native-transport-disabled".into())])
}

struct HttpContainers {
    containers: HashMap<String, VirtualChunkContainer>,
    credentials: HashMap<String, Option<Credentials>>,
}

fn http_containers(declared: &[HttpVirtualChunkContainer]) -> Result<HttpContainers, ReaderError> {
    let mut containers = HashMap::new();
    let mut credentials = HashMap::new();
    let mut names = std::collections::HashSet::new();
    let mut origins = std::collections::HashSet::new();
    for declaration in declared {
        let store = icechunk::ObjectStoreConfig::Http(HttpConfig {
            opts: http_virtual_options(),
            headers: HashMap::new(),
        });
        let container = match &declaration.name {
            Some(name) => {
                if !names.insert(name) {
                    return Err(unsupported("Duplicate HTTP container name"));
                }
                VirtualChunkContainer::new_named(
                    name.clone(),
                    declaration.url_prefix.clone(),
                    store,
                )
            }
            None => VirtualChunkContainer::new(declaration.url_prefix.clone(), store),
        }
        .map_err(unsupported)?;
        if containers
            .insert(declaration.url_prefix.clone(), container)
            .is_some()
        {
            return Err(unsupported("Duplicate HTTP container prefix"));
        }
        let url = reqwest::Url::parse(&declaration.url_prefix).map_err(unsupported)?;
        origins.insert(format!("{}/", url.origin().ascii_serialization()));
        credentials.insert(
            declaration.url_prefix.clone(),
            Some(Credentials::HttpAccess),
        );
    }
    // Route every spelling on these origins through the runtime callback,
    // including references whose encoded slash bypasses the narrower raw prefix
    // match in Icechunk 2.2.2. The fetcher reauthorizes the actual request URL
    // against only the original declarations before making a request.
    for origin in origins {
        containers.entry(origin.clone()).or_insert(
            VirtualChunkContainer::new(
                origin.clone(),
                icechunk::ObjectStoreConfig::Http(HttpConfig {
                    opts: http_virtual_options(),
                    headers: HashMap::new(),
                }),
            )
            .map_err(unsupported)?,
        );
        credentials
            .entry(origin)
            .or_insert(Some(Credentials::HttpAccess));
    }
    Ok(HttpContainers {
        containers,
        credentials,
    })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Selector {
    Index(i64),
    Indices(Vec<i64>),
}

#[derive(Debug, Serialize)]
pub struct TypedBuffer {
    pub dtype: String,
    pub shape: Vec<usize>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidSelection,
    SizeLimit,
    Unsupported,
    Snapshot,
    Storage,
    Decode,
}

#[derive(Debug, Serialize)]
pub struct ReaderError {
    pub kind: ErrorKind,
    pub message: String,
    /// Set from Icechunk's typed FetchError before zarrs erases its source.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub virtual_chunk_fetch: bool,
    /// Python's corresponding Icechunk exception class, when the native source is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_class: Option<&'static str>,
}
impl fmt::Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}
impl std::error::Error for ReaderError {}
fn err(kind: ErrorKind, message: impl ToString) -> ReaderError {
    ReaderError {
        kind,
        message: message.to_string(),
        virtual_chunk_fetch: false,
        exception_class: None,
    }
}
fn unsupported(message: impl ToString) -> ReaderError {
    err(ErrorKind::Unsupported, message)
}

#[derive(Debug, Default, Serialize)]
pub struct ReadTrace {
    /// Logical Zarr store accesses; not a claim about physical S3/filesystem reads.
    pub accesses: Vec<Access>,
    pub decoded_inner_chunks: Vec<Vec<u64>>,
    /// Full padded inner-chunk work, including edge padding and fill units.
    pub decoded_bytes: usize,
    pub subset_bytes: usize,
    pub output_bytes: usize,
    /// Bytes delivered by logical Zarr gets (metadata plus encoded payloads).
    pub store_bytes: u64,
    pub max_concurrent_chunks: usize,
}
#[derive(Debug, Serialize)]
pub struct Access {
    pub key: String,
    pub ranges: Vec<Range>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Range {
    FromStart { offset: u64, length: Option<u64> },
    Suffix { suffix: u64 },
}

#[derive(Debug)]
struct PayloadPolicy {
    full_bytes: usize,
    blosc: Option<BloscPolicy>,
    shard: Option<ShardPolicy>,
    scalar_zstd: bool,
    member_zstd: bool,
    max_decoded_bytes: usize,
}
#[derive(Debug)]
struct BloscPolicy {
    compressor_id: u8,
    typesize: u8,
    byte_typesize: bool,
    shuffle_flags: u8,
}
#[derive(Debug)]
struct ShardPolicy {
    index_bytes: u64,
    index_at_start: bool,
    inner_count: usize,
}
struct ValidatedIndex {
    key: String,
    payloads: Vec<std::ops::Range<u64>>,
}
#[derive(Clone, Copy)]
enum PayloadRead {
    Index,
    FullShard,
    Blosc,
    Bytes,
    ScalarZstd,
    MemberZstd,
}

fn validate_scalar_zstd(bytes: &[u8]) -> Result<(), StorageError> {
    // One ordinary frame, no dictionaries/checksum/reserved flags, exact 8-byte output.
    if bytes.len() < 6
        || bytes.len() > 64
        || bytes[..4] != [0x28, 0xb5, 0x2f, 0xfd]
        || bytes[4] & 0x1f != 0
        || zstd::zstd_safe::get_frame_content_size(bytes)
            .ok()
            .flatten()
            != Some(8)
        || zstd::zstd_safe::find_frame_compressed_size(bytes).ok() != Some(bytes.len())
    {
        return Err(invalid_payload("Unattested scalar Zstd frame"));
    }
    Ok(())
}
fn decode_scalar_zstd(bytes: &[u8]) -> Result<Vec<u8>, StorageError> {
    validate_scalar_zstd(bytes)?;
    let mut decoder = zstd::bulk::Decompressor::new().map_err(invalid_payload)?;
    decoder
        .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(10))
        .map_err(invalid_payload)?;
    let mut output = [0u8; 8];
    let size = decoder
        .decompress_to_buffer(bytes, &mut output[..])
        .map_err(invalid_payload)?;
    if size != output.len() {
        return Err(invalid_payload(
            "Scalar Zstd output differs from int64 width",
        ));
    }
    Ok(output.to_vec())
}

fn validate_member_zstd(bytes: &[u8], decoded_bytes: usize) -> Result<(), StorageError> {
    if bytes.len() < 6
        || bytes.len() > 256
        || bytes[..4] != [0x28, 0xb5, 0x2f, 0xfd]
        || bytes[4] & 0x1f != 0
        // Bulk decompression can ignore WindowLogMax when using the caller's
        // fixed destination as history. A non-single-segment window descriptor
        // of zero is the only one whose window is at most 1 KiB.
        || (bytes[4] & 0x20 == 0 && bytes[5] != 0)
        || zstd::zstd_safe::get_frame_content_size(bytes)
            .ok()
            .flatten()
            != Some(decoded_bytes as u64)
        || zstd::zstd_safe::find_frame_compressed_size(bytes).ok() != Some(bytes.len())
    {
        return Err(invalid_payload("Unattested int16 Zstd frame"));
    }
    Ok(())
}

fn decode_member_zstd(bytes: &[u8], decoded_bytes: usize) -> Result<Vec<u8>, StorageError> {
    validate_member_zstd(bytes, decoded_bytes)?;
    let mut decoder = zstd::bulk::Decompressor::new().map_err(invalid_payload)?;
    decoder
        .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(10))
        .map_err(invalid_payload)?;
    let mut output = vec![0; decoded_bytes];
    let size = decoder
        .decompress_to_buffer(bytes, &mut output[..])
        .map_err(invalid_payload)?;
    if size != output.len() {
        return Err(invalid_payload("Zstd output differs from full int16 chunk"));
    }
    Ok(output)
}

fn member_zstd(meta: &Value) -> bool {
    meta["data_type"] == "int16"
        && meta["shape"]
            .as_array()
            .is_some_and(|shape| shape.len() == 1)
        && meta["codecs"]
            == json!([
                {"name":"bytes","configuration":{"endian":"little"}},
                {"name":"zstd","configuration":{"level":0,"checksum":false}}
            ])
}

fn invalid_payload(message: impl ToString) -> StorageError {
    StorageError::Other(format!(
        "Payload rejected before decode: {}",
        message.to_string()
    ))
}
fn checked_range(
    offset: u64,
    length: u64,
    size: u64,
) -> Result<std::ops::Range<u64>, StorageError> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| invalid_payload("Shard range arithmetic overflow"))?;
    if length == 0 || offset >= end || end > size {
        return Err(invalid_payload("Invalid shard range ordering or bounds"));
    }
    Ok(offset..end)
}
fn resolve_range(range: ByteRange, size: u64) -> Result<std::ops::Range<u64>, StorageError> {
    match range {
        ByteRange::FromStart(offset, Some(length)) => checked_range(offset, length, size),
        ByteRange::FromStart(offset, None) => checked_range(
            offset,
            size.checked_sub(offset)
                .ok_or_else(|| invalid_payload("Range starts beyond chunk"))?,
            size,
        ),
        ByteRange::Suffix(length) => checked_range(
            size.checked_sub(length)
                .ok_or_else(|| invalid_payload("Index exceeds shard size"))?,
            length,
            size,
        ),
    }
}
impl PayloadPolicy {
    fn from_metadata(meta: &Value, cap: &Capability) -> Result<Self, ReaderError> {
        let full_bytes = checked_bytes(&cap.inner, cap.width, cap.chunk_limit())?;
        let sharded = meta["codecs"][0]["name"] == "sharding_indexed";
        let codecs = if sharded {
            &meta["codecs"][0]["configuration"]["codecs"]
        } else {
            &meta["codecs"]
        };
        let scalar_zstd = cap.shape.is_empty();
        let member_zstd = member_zstd(meta);
        let blosc = if !scalar_zstd
            && !member_zstd
            && codecs
                .as_array()
                .is_some_and(|c| c.last().is_some_and(|c| c["name"] == "blosc"))
        {
            let config = &codecs
                .as_array()
                .expect("attested codecs")
                .last()
                .expect("blosc")["configuration"];
            Some(BloscPolicy {
                compressor_id: match config["cname"].as_str() {
                    Some("zstd") => 4,
                    Some("lz4") => 1,
                    _ => return Err(unsupported("Unrecognized Blosc compressor policy")),
                },
                typesize: config["typesize"].as_u64().expect("attested typesize") as u8,
                byte_typesize: config["typesize"].as_u64() == Some(cap.width as u64),
                shuffle_flags: match config["shuffle"].as_str() {
                    Some("noshuffle") => 0,
                    Some("shuffle") => 0x01,
                    Some("bitshuffle") => 0x04,
                    _ => return Err(unsupported("Unrecognized Blosc shuffle policy")),
                },
            })
        } else {
            None
        };
        let shard = if sharded {
            let outer = dimensions(&meta["chunk_grid"]["configuration"]["chunk_shape"])?;
            let counts: Vec<_> = outer.iter().zip(&cap.inner).map(|(o, i)| o / i).collect();
            let index_bytes = checked_bytes(&counts, 16, MAX_INDEX_BYTES - 4)? + 4;
            Some(ShardPolicy {
                index_bytes: index_bytes as u64,
                index_at_start: meta["codecs"][0]["configuration"]["index_location"] == "start",
                inner_count: (index_bytes - 4) / 16,
            })
        } else {
            None
        };
        Ok(Self {
            full_bytes,
            blosc,
            shard,
            scalar_zstd,
            member_zstd,
            max_decoded_bytes: cap.chunk_limit(),
        })
    }
    fn encoded_limit(&self) -> usize {
        if self.scalar_zstd {
            64
        } else if self.member_zstd {
            256
        } else {
            self.max_decoded_bytes + 16
        }
    }
    fn validate_frame(&self, bytes: &[u8]) -> Result<(), StorageError> {
        if bytes.len() > self.encoded_limit() {
            return Err(invalid_payload("Encoded chunk exceeds limit"));
        }
        let Some(blosc) = &self.blosc else {
            if bytes.len() != self.full_bytes {
                return Err(invalid_payload(
                    "Plain bytes length differs from full inner chunk",
                ));
            }
            return Ok(());
        };
        if bytes.len() < 16 {
            return Err(invalid_payload("Truncated Blosc header"));
        }
        let nbytes = u32::from_le_bytes(
            bytes[4..8]
                .try_into()
                .map_err(|_| invalid_payload("Blosc nbytes header"))?,
        ) as usize;
        if nbytes != self.full_bytes {
            return Err(invalid_payload(format!(
                "Blosc decoded size {nbytes} differs from expected full inner chunk {} or exceeds limit",
                self.full_bytes
            )));
        }
        let cbytes = u32::from_le_bytes(
            bytes[12..16]
                .try_into()
                .map_err(|_| invalid_payload("Blosc cbytes header"))?,
        ) as usize;
        if cbytes != bytes.len() {
            return Err(invalid_payload(
                "Blosc encoded size differs from payload length",
            ));
        }
        let flags = bytes[2];
        if bytes[0] != 2 || flags & 0x08 != 0 {
            return Err(invalid_payload(
                "Unsupported Blosc frame version or reserved flags",
            ));
        }
        // The decoder uses the header typesize for unshuffling and partial-read
        // alignment. Header typesize 1 aligns every numeric read; keep nonnative
        // metadata item sizes restricted to their exact admitted header.
        if bytes[3] != blosc.typesize && !(bytes[3] == 1 && blosc.byte_typesize) {
            return Err(invalid_payload(
                "Blosc typesize differs from admitted metadata",
            ));
        }
        let blocksize = u32::from_le_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| invalid_payload("Blosc blocksize header"))?,
        ) as usize;
        if blocksize == 0 || blocksize > nbytes {
            return Err(invalid_payload("Invalid Blosc blocksize"));
        }
        if flags & 0x02 != 0 {
            // Memcpy bypasses compressor dispatch and both shuffle transforms.
            // Typesize still matters to zarrs' partial-read item alignment.
            if cbytes != nbytes + 16 {
                return Err(invalid_payload(
                    "Blosc memcpy length differs from nbytes + header",
                ));
            }
        } else {
            if flags >> 5 != blosc.compressor_id {
                return Err(invalid_payload(
                    "Blosc compressor differs from admitted metadata",
                ));
            }
            if bytes[1] != 1 {
                return Err(invalid_payload("Unsupported Blosc zstd format version"));
            }
            if flags & 0x05 != blosc.shuffle_flags {
                return Err(invalid_payload(
                    "Blosc shuffle flags differ from admitted metadata",
                ));
            }
        }
        Ok(())
    }
    fn validate_index(
        &self,
        bytes: &[u8],
        size: u64,
    ) -> Result<Vec<std::ops::Range<u64>>, StorageError> {
        let shard = self
            .shard
            .as_ref()
            .ok_or_else(|| invalid_payload("Unexpected shard index"))?;
        if bytes.len() as u64 != shard.index_bytes {
            return Err(invalid_payload("Wrong shard index length"));
        }
        let index_start = shard.index_start(size)?;
        let data_start = if shard.index_at_start {
            shard.index_bytes
        } else {
            0
        };
        let data_end = if shard.index_at_start {
            size
        } else {
            index_start
        };
        let mut payloads = Vec::new();
        // Arithmetic is validated before zarrs sees the CRC-protected index. Zarrs
        // still verifies CRC32C; a valid checksum alone does not prove safe ranges.
        for entry in bytes[..bytes.len() - 4].chunks_exact(16) {
            let offset = u64::from_le_bytes(
                entry[..8]
                    .try_into()
                    .map_err(|_| invalid_payload("Index offset"))?,
            );
            let length = u64::from_le_bytes(
                entry[8..]
                    .try_into()
                    .map_err(|_| invalid_payload("Index length"))?,
            );
            if offset == u64::MAX && length == u64::MAX {
                continue;
            }
            let span = checked_range(offset, length, size)?;
            if span.start < data_start || span.end > data_end {
                return Err(invalid_payload(
                    "Payload overlaps shard index or exceeds data bounds",
                ));
            }
            if length > self.encoded_limit() as u64 {
                return Err(invalid_payload("Encoded shard payload exceeds limit"));
            }
            if self.blosc.is_none() && length != self.full_bytes as u64 {
                return Err(invalid_payload(
                    "Plain shard payload length differs from full inner chunk",
                ));
            }
            payloads.push(span);
        }
        Ok(payloads)
    }
}
impl ShardPolicy {
    fn index_start(&self, size: u64) -> Result<u64, StorageError> {
        let suffix_start = size
            .checked_sub(self.index_bytes)
            .ok_or_else(|| invalid_payload("Shard is shorter than index"))?;
        Ok(if self.index_at_start { 0 } else { suffix_start })
    }
}

struct StorageFailure {
    virtual_fetch: bool,
    message: String,
    exception_class: &'static str,
}

fn storage_exception_class(kind: &icechunk::store::StoreErrorKind) -> &'static str {
    use icechunk::format::manifest::VirtualReferenceErrorKind;
    use icechunk::{session::SessionErrorKind, store::StoreErrorKind};
    match kind {
        StoreErrorKind::SessionError(SessionErrorKind::VirtualReferenceError(error)) => match error
        {
            VirtualReferenceErrorKind::FetchError(_)
            | VirtualReferenceErrorKind::ObjectModified(_)
            | VirtualReferenceErrorKind::InvalidObjectSize { .. } => "StorageError",
            VirtualReferenceErrorKind::OtherError(_) => "InternalError",
            _ => "InvalidInputError",
        },
        StoreErrorKind::InvalidVirtualChunkContainer { .. } => "InvalidInputError",
        StoreErrorKind::NotFound(_) => "NodeNotFoundError",
        StoreErrorKind::SessionError(SessionErrorKind::FormatError(_)) => "FormatError",
        _ => "StorageError",
    }
}

fn storage_failure(error: &icechunk::store::StoreError) -> StorageFailure {
    let virtual_fetch = matches!(
        error.kind(),
        icechunk::store::StoreErrorKind::SessionError(
            icechunk::session::SessionErrorKind::VirtualReferenceError(
                icechunk::format::manifest::VirtualReferenceErrorKind::FetchError(_)
            )
        )
    );
    // ICError Display omits the source chain. Preserve both renderings of
    // each source, as Python does, so upstream diagnostics survive zarrs.
    let mut message = format!("{error}\n{:?}", error.kind());
    let mut source = std::error::Error::source(error);
    for _ in 0..32 {
        let Some(cause) = source else {
            break;
        };
        message.push_str(&format!("\n{cause}\n{cause:?}"));
        source = cause.source();
    }
    StorageFailure {
        virtual_fetch,
        message,
        exception_class: storage_exception_class(error.kind()),
    }
}

struct ObservedStore {
    inner: AsyncIcechunkStore,
    failure: Arc<Mutex<Option<StorageFailure>>>,
    trace: Arc<Mutex<ReadTrace>>,
    payload: std::sync::OnceLock<PayloadPolicy>,
    // Only one decoder runs at a time; retain at most one bounded shard index.
    index: Mutex<Option<ValidatedIndex>>,
}
impl ObservedStore {
    fn storage_error(&self, error: icechunk::store::StoreError) -> StorageError {
        *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(storage_failure(&error));
        invalid_payload(error.to_string())
    }

    fn record(&self, key: &StoreKey, ranges: Vec<Range>) {
        self.trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .accesses
            .push(Access {
                key: key.to_string(),
                ranges,
            });
    }
    fn range_kind(
        &self,
        key: &StoreKey,
        span: &std::ops::Range<u64>,
        size: u64,
        policy: &PayloadPolicy,
    ) -> Result<PayloadRead, StorageError> {
        if let Some(shard) = &policy.shard {
            let index_start = shard.index_start(size)?;
            if span.start == index_start && span.end - span.start == shard.index_bytes {
                return Ok(PayloadRead::Index);
            }
            if span.start == 0 && span.end == size {
                if shard.inner_count != 1
                    || size > policy.encoded_limit() as u64 + shard.index_bytes
                {
                    return Err(invalid_payload(
                        "Full shard fetch exceeds single-inner encoded bound",
                    ));
                }
                return Ok(PayloadRead::FullShard);
            }
            if span.end - span.start > policy.encoded_limit() as u64 {
                return Err(invalid_payload("Encoded range exceeds limit"));
            }
            let index = self.index.lock().unwrap_or_else(|e| e.into_inner());
            let index = index
                .as_ref()
                .filter(|i| i.key == key.as_str())
                .ok_or_else(|| invalid_payload("Payload requested before validated index"))?;
            let entry = index
                .payloads
                .iter()
                .find(|p| span.start >= p.start && span.end <= p.end)
                .ok_or_else(|| {
                    invalid_payload("Range not contained in a validated inner payload")
                })?;
            if policy.blosc.is_some() {
                if span != entry {
                    return Err(invalid_payload("Expected complete Blosc frame"));
                }
                Ok(PayloadRead::Blosc)
            } else {
                Ok(PayloadRead::Bytes)
            }
        } else {
            if size > policy.encoded_limit() as u64 {
                return Err(invalid_payload("Encoded chunk exceeds limit"));
            }
            if policy.scalar_zstd {
                if span.start != 0 || span.end != size {
                    return Err(invalid_payload("Expected complete scalar Zstd frame"));
                }
                return Ok(PayloadRead::ScalarZstd);
            }
            if policy.member_zstd {
                if span.start != 0 || span.end != size {
                    return Err(invalid_payload("Expected complete int16 Zstd frame"));
                }
                return Ok(PayloadRead::MemberZstd);
            }
            if policy.blosc.is_some() {
                if span.start != 0 || span.end != size {
                    return Err(invalid_payload("Expected complete Blosc frame"));
                }
                Ok(PayloadRead::Blosc)
            } else {
                if size != policy.full_bytes as u64 {
                    return Err(invalid_payload(
                        "Plain bytes length differs from full inner chunk",
                    ));
                }
                Ok(PayloadRead::Bytes)
            }
        }
    }
    async fn read_payload(
        &self,
        key: &StoreKey,
        range: ByteRange,
    ) -> Result<MaybeBytes, StorageError> {
        let policy = self
            .payload
            .get()
            .ok_or_else(|| invalid_payload("Payload metadata has not been attested"))?;
        let store = icechunk::Store::from_session(self.inner.session()).await;
        // getsize reads the logical chunk reference, not its payload. Icechunk uses
        // zero for an absent chunk, so distinguish that with its typed NotFound.
        let size = store
            .getsize(key.as_str())
            .await
            .map_err(|e| self.storage_error(e))?;
        if size == 0 {
            return match store
                .get(key.as_str(), &icechunk::format::ByteRange::ALL)
                .await
            {
                Err(error)
                    if matches!(error.kind(), icechunk::store::StoreErrorKind::NotFound(..)) =>
                {
                    Ok(None)
                }
                Err(error) => Err(self.storage_error(error)),
                Ok(_) => Err(invalid_payload("Empty encoded chunk")),
            };
        }
        let span = resolve_range(range, size)?;
        let kind = self.range_kind(key, &span, size, policy)?;
        // resolve_range checked the sum and bounds BEFORE this constructor and
        // before Icechunk can slice inline bytes or translate a physical range.
        let range =
            icechunk::format::ByteRange::from_offset_with_length(span.start, span.end - span.start);
        let bytes = store
            .get(key.as_str(), &range)
            .await
            .map_err(|e| self.storage_error(e))?;
        if bytes.len() as u64 != span.end - span.start {
            return Err(invalid_payload("Fetched range length mismatch"));
        }
        self.trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .store_bytes += bytes.len() as u64;
        match kind {
            PayloadRead::Blosc => policy.validate_frame(&bytes)?,
            PayloadRead::Bytes => {}
            PayloadRead::ScalarZstd => validate_scalar_zstd(&bytes)?,
            PayloadRead::MemberZstd => validate_member_zstd(&bytes, policy.full_bytes)?,
            PayloadRead::Index => {
                let payloads = policy.validate_index(&bytes, size)?;
                *self.index.lock().unwrap_or_else(|e| e.into_inner()) = Some(ValidatedIndex {
                    key: key.to_string(),
                    payloads,
                });
            }
            PayloadRead::FullShard => {
                let shard = policy
                    .shard
                    .as_ref()
                    .ok_or_else(|| invalid_payload("Missing shard policy"))?;
                let start = usize::try_from(shard.index_start(size)?)
                    .map_err(|_| invalid_payload("Index position too large"))?;
                let index = bytes
                    .get(start..start + shard.index_bytes as usize)
                    .ok_or_else(|| invalid_payload("Index outside fetched shard"))?;
                for entry in policy.validate_index(index, size)? {
                    let start = usize::try_from(entry.start)
                        .map_err(|_| invalid_payload("Payload offset too large"))?;
                    let end = usize::try_from(entry.end)
                        .map_err(|_| invalid_payload("Payload end too large"))?;
                    policy.validate_frame(
                        bytes
                            .get(start..end)
                            .ok_or_else(|| invalid_payload("Payload outside shard"))?,
                    )?;
                }
            }
        }
        Ok(Some(bytes))
    }
}
#[async_trait::async_trait]
impl AsyncReadableStorageTraits for ObservedStore {
    async fn get(&self, key: &StoreKey) -> Result<MaybeBytes, StorageError> {
        self.record(
            key,
            vec![Range::FromStart {
                offset: 0,
                length: None,
            }],
        );
        if key.as_str() == "zarr.json" || key.as_str().ends_with("/zarr.json") {
            let value = self.inner.get(key).await?;
            self.trace
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .store_bytes += value.as_ref().map_or(0, |bytes| bytes.len() as u64);
            Ok(value)
        } else {
            self.read_payload(key, ByteRange::FromStart(0, None)).await
        }
    }
    async fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        ranges: ByteRangeIterator<'a>,
    ) -> Result<AsyncMaybeBytesIterator<'a>, StorageError> {
        let ranges: Vec<_> = ranges.collect();
        self.record(
            key,
            ranges
                .iter()
                .map(|r| match r {
                    ByteRange::FromStart(offset, length) => Range::FromStart {
                        offset: *offset,
                        length: *length,
                    },
                    ByteRange::Suffix(suffix) => Range::Suffix { suffix: *suffix },
                })
                .collect(),
        );
        let mut values = Vec::new();
        for range in ranges {
            let bytes = self.read_payload(key, range).await?;
            let Some(bytes) = bytes else {
                return Ok(None);
            };
            values.push(Ok(bytes));
        }
        Ok(Some(futures::stream::iter(values).boxed()))
    }
    async fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        self.inner.size_key(key).await
    }
    fn supports_get_partial(&self) -> bool {
        true
    }
}

struct Capability {
    dtype: String,
    width: usize,
    shape: Vec<u64>,
    inner: Vec<u64>,
    scale_offset: Option<ScaleOffset>,
}
fn dimensions(value: &Value) -> Result<Vec<u64>, ReaderError> {
    let values = value
        .as_array()
        .ok_or_else(|| unsupported("Expected dimension list"))?;
    if values.len() > MAX_RANK {
        return Err(unsupported("Rank exceeds 16"));
    }
    values
        .iter()
        .map(|v| v.as_u64().ok_or_else(|| unsupported("Invalid dimension")))
        .collect()
}
fn product(values: impl IntoIterator<Item = usize>, limit: usize) -> Result<usize, ReaderError> {
    values.into_iter().try_fold(1usize, |n, v| {
        n.checked_mul(v).filter(|v| *v <= limit).ok_or_else(|| {
            err(
                ErrorKind::SizeLimit,
                "Size arithmetic overflow or allocation/work limit exceeded",
            )
        })
    })
}
fn checked_bytes(shape: &[u64], width: usize, limit: usize) -> Result<usize, ReaderError> {
    let dims = shape
        .iter()
        .map(|v| usize::try_from(*v).map_err(|_| err(ErrorKind::SizeLimit, "Dimension too large")))
        .collect::<Result<Vec<_>, _>>()?;
    product(dims.into_iter().chain([width]), limit)
}
fn exact_keys(value: &Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|m| m.len() == keys.len() && keys.iter().all(|k| m.contains_key(*k)))
}
fn bytes_codec(value: &Value, width: usize) -> bool {
    *value == json!({"name":"bytes","configuration":{"endian":"little"}})
        || (width == 1
            && (*value == json!({"name":"bytes"})
                || *value == json!({"name":"bytes","configuration":{}})
                || *value == json!({"name":"bytes","configuration":{"endian":null}})))
}
fn numeric_codecs(value: &Value, width: usize, integer: bool) -> Result<(), ReaderError> {
    let codecs = value
        .as_array()
        .ok_or_else(|| unsupported("Missing codecs"))?;
    numeric_codec_slice(codecs, width, integer)
}
fn numeric_codec_slice(codecs: &[Value], width: usize, integer: bool) -> Result<(), ReaderError> {
    if codecs.iter().any(|codec| codec["name"] == "zstd") {
        return Err(unsupported("Unsupported standalone Zstd configuration"));
    }
    if !(1..=2).contains(&codecs.len()) || !bytes_codec(&codecs[0], width) {
        return Err(unsupported(
            "Expected little-endian bytes, optionally followed by blosc",
        ));
    }
    if codecs.len() == 2 {
        let codec = &codecs[1];
        let c = &codec["configuration"];
        if !exact_keys(codec, &["name", "configuration"])
            || codec["name"] != "blosc"
            || !exact_keys(c, &["typesize", "cname", "clevel", "shuffle", "blocksize"])
            || !(c["typesize"].as_u64() == Some(width as u64)
                || (integer
                    && c["typesize"]
                        .as_u64()
                        .is_some_and(|n| (1..=255).contains(&n))))
            || !(c["cname"] == "zstd"
                || (width == 4
                    && c["cname"] == "lz4"
                    && c["clevel"] == 5
                    && c["shuffle"] == "shuffle"))
            || !c["clevel"].as_u64().is_some_and(|v| v <= 9)
            || !matches!(
                c["shuffle"].as_str(),
                Some("noshuffle" | "shuffle" | "bitshuffle")
            )
            || c["blocksize"] != 0
        {
            return Err(unsupported(
                "Unrecognized blosc configuration (requires zstd or float32 lz4/clevel 5/shuffle, admitted typesize, clevel 0..9, known shuffle, blocksize 0)",
            ));
        }
    }
    Ok(())
}
const MAX_SIBLINGS: usize = 4096;
const MAX_SIBLING_METADATA_BYTES: usize = 1024 * 1024;

/// xarray copies a time variable's units onto the variable its `bounds` attribute names
/// (`conventions._update_bounds_attributes`), so an array referenced as bounds by a
/// sibling in the same group may decode to datetimes. Refuse such targets.
async fn refuse_bounds_targets(store: &ObservedStore, array_path: &str) -> Result<(), ReaderError> {
    let (parent, name) = match array_path.rsplit_once('/') {
        Some((parent, name)) => (format!("{parent}/"), name),
        None => (String::new(), array_path),
    };
    let prefix = if parent.is_empty() {
        StorePrefix::root()
    } else {
        StorePrefix::new(parent).map_err(|e| unsupported(e.to_string()))?
    };
    let listing = store
        .inner
        .list_dir(&prefix)
        .await
        .map_err(|e| err(ErrorKind::Storage, e))?;
    if listing.prefixes().len() > MAX_SIBLINGS {
        return Err(err(
            ErrorKind::SizeLimit,
            "Too many sibling nodes to check bounds",
        ));
    }
    for child in listing.prefixes() {
        let key = StoreKey::new(format!("{}zarr.json", child.as_str()))
            .map_err(|e| unsupported(e.to_string()))?;
        let Some(raw) = store
            .get(&key)
            .await
            .map_err(|e| err(ErrorKind::Storage, e))?
        else {
            continue;
        };
        if raw.len() > MAX_SIBLING_METADATA_BYTES {
            return Err(err(ErrorKind::SizeLimit, "Sibling metadata too large"));
        }
        let sibling: Value =
            serde_json::from_slice(&raw).map_err(|e| unsupported(e.to_string()))?;
        if sibling["attributes"]["bounds"].as_str() == Some(name) {
            return Err(unsupported(format!(
                "Array {name} is the bounds variable of a sibling; xarray may decode it as time"
            )));
        }
    }
    Ok(())
}

/// Reserved coordinate names admit only their pinned metadata tuple, including
/// the exact attributes. Logical shape may vary; deployed padded chunks do not.
fn coordinate_profile(meta: &Value, array_path: &str) -> Result<Option<Value>, ReaderError> {
    let profiles: Value = serde_json::from_str(include_str!("coordinate_profiles.json"))
        .map_err(|e| unsupported(format!("Invalid coordinate profiles: {e}")))?;
    let name = array_path.rsplit('/').next().unwrap_or(array_path);
    let Some(profile) = profiles.get(name) else {
        return Ok(None);
    };
    let fields = profile
        .as_object()
        .ok_or_else(|| unsupported("Invalid coordinate profile"))?;
    for (key, value) in fields {
        if meta.get(key) != Some(value) {
            return Err(unsupported(format!("Unattested {name} coordinate {key}")));
        }
    }
    Ok(Some(profile.clone()))
}

fn attest(meta: &Value, array_path: &str) -> Result<Capability, ReaderError> {
    attest_with_mode(meta, array_path, false)
}
fn attest_with_mode(
    meta: &Value,
    array_path: &str,
    dataset: bool,
) -> Result<Capability, ReaderError> {
    // Parse the complete schema even when no chunk will pass through zarrs.
    // Keep scalar payload decoding in our fixed-size decompressor.
    let metadata: zarrs::metadata::v3::ArrayMetadataV3 =
        serde_json::from_value(meta.clone()).map_err(|e| unsupported(e.to_string()))?;
    for (name, field) in &metadata.additional_fields {
        if field.must_understand() {
            return Err(unsupported(format!(
                "Unknown required metadata field {name}"
            )));
        }
    }
    if metadata
        .dimension_names
        .as_ref()
        .is_some_and(|names| names.len() != metadata.shape.len())
    {
        return Err(unsupported("Dimension names differ from array rank"));
    }
    if meta["zarr_format"] != 3 || meta["node_type"] != "array" {
        return Err(unsupported("Only Zarr v3 arrays"));
    }
    let (dtype, width) = match meta["data_type"].as_str() {
        Some("float32") => ("float32", 4),
        Some("float64") => ("float64", 8),
        Some("int64") => ("int64", 8),
        Some(dtype @ ("int8" | "uint8")) if dataset => (dtype, 1),
        Some(dtype @ ("int16" | "uint16")) if dataset => (dtype, 2),
        Some(dtype @ ("int32" | "uint32")) if dataset => (dtype, 4),
        _ => return Err(unsupported("Only float32/float64/int64")),
    };
    let shape = dimensions(&meta["shape"])?;
    if shape.is_empty() && dtype != "int64" {
        return Err(unsupported(
            "Only attested int64 scalar arrays are supported",
        ));
    }
    let attrs = meta["attributes"]
        .as_object()
        .ok_or_else(|| unsupported("Expected attributes object"))?;
    let profile = if dataset {
        None
    } else {
        coordinate_profile(meta, array_path)?
    };
    // Dataset mode keeps descriptive attributes, but never silently ignores a CF
    // transform that would change values. The original attested API stays strict.
    if dataset
        && attrs.keys().any(|key| {
            matches!(
                key.as_str(),
                "scale_factor" | "add_offset" | "missing_value" | "_Unsigned" | "_Encoding"
            )
        })
    {
        return Err(unsupported("Unsupported CF value transform"));
    }
    // An allowlist: xarray coders key on attribute presence (e.g. `dtype`, `_Encoding`,
    // `scale_factor`) regardless of stored dtype, so unknown attributes are refused.
    const ALLOWED_ATTRIBUTES: &[&str] = &[
        "long_name",
        "short_name",
        "standard_name",
        "units",
        "step_type",
        "comment",
        "coordinates",
        "grid_mapping",
        "_FillValue",
    ];
    if let Some(key) = attrs.keys().find(|k| {
        !dataset
            && !ALLOWED_ATTRIBUTES.contains(&k.as_str())
            && !profile
                .as_ref()
                .is_some_and(|p| p["attributes"].get(k.as_str()).is_some())
    }) {
        return Err(unsupported(format!(
            "Attribute {key} is outside the stored-float contract"
        )));
    }
    let cf_attrs = if dataset {
        attrs
            .iter()
            .filter(|(k, _)| matches!(k.as_str(), "units" | "calendar" | "dtype" | "_FillValue"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    } else {
        attrs.clone()
    };
    let time_encoding = cf_time::attested_encoding(dtype, &cf_attrs).is_ok();
    // BooleanCoder and other dtype-directed casts remain excluded.
    if (attrs.contains_key("calendar") || attrs.contains_key("dtype")) && !time_encoding {
        return Err(unsupported("Unattested CF calendar or dtype"));
    }
    if !time_encoding
        && attrs.get("units").and_then(Value::as_str).is_some_and(|s| {
            s.contains("since")
                || ((!dataset || attrs.contains_key("dtype"))
                    && matches!(
                        s,
                        "seconds"
                            | "minutes"
                            | "hours"
                            | "days"
                            | "milliseconds"
                            | "microseconds"
                            | "nanoseconds"
                    ))
        })
    {
        return Err(unsupported("Time conversion outside stored-float contract"));
    }
    let fill = &meta["fill_value"];
    if let Some(value) = attrs.get("_FillValue") {
        // Xarray serializes a NaN _FillValue as base64 IEEE bytes; no finite sentinel masking.
        let nan = value.as_str().is_some_and(|s| s == "AAAAAAAA+H8=");
        if !nan || fill != "NaN" {
            return Err(unsupported("Only NaN _FillValue masking is supported"));
        }
    }
    if dtype.starts_with("int") || dtype.starts_with("uint") {
        let valid_fill = match dtype {
            "int8" => fill.as_i64().is_some_and(|v| i8::try_from(v).is_ok()),
            "int16" => fill.as_i64().is_some_and(|v| i16::try_from(v).is_ok()),
            "int32" => fill.as_i64().is_some_and(|v| i32::try_from(v).is_ok()),
            "int64" => fill.as_i64().is_some(),
            "uint8" => fill.as_u64().is_some_and(|v| u8::try_from(v).is_ok()),
            "uint16" => fill.as_u64().is_some_and(|v| u16::try_from(v).is_ok()),
            "uint32" => fill.as_u64().is_some_and(|v| u32::try_from(v).is_ok()),
            _ => false,
        };
        if !valid_fill || attrs.contains_key("_FillValue") {
            return Err(unsupported(
                "Expected in-range integer fill without attribute masking",
            ));
        }
    }
    if !(fill.is_number() || matches!(fill.as_str(), Some("NaN" | "Infinity" | "-Infinity"))) {
        return Err(unsupported("Unrecognized float fill"));
    }
    if meta
        .get("storage_transformers")
        .is_some_and(|v| v != &json!([]))
    {
        return Err(unsupported("Storage transformers are not supported"));
    }
    if meta["chunk_grid"]["name"] != "regular"
        || !exact_keys(&meta["chunk_grid"], &["name", "configuration"])
        || !exact_keys(&meta["chunk_grid"]["configuration"], &["chunk_shape"])
    {
        return Err(unsupported("Only regular chunk grids"));
    }
    if meta["chunk_key_encoding"] != json!({"name":"default","configuration":{"separator":"/"}}) {
        return Err(unsupported("Only default / chunk keys"));
    }
    let outer = dimensions(&meta["chunk_grid"]["configuration"]["chunk_shape"])?;
    if outer.len() != shape.len() || outer.contains(&0) {
        return Err(unsupported("Invalid chunk shape"));
    }
    let codecs = meta["codecs"]
        .as_array()
        .ok_or_else(|| unsupported("Missing codecs"))?;
    let scale_offset = if codecs.first().is_some_and(|c| c["name"] == "scale_offset") {
        if dtype != "float32" || meta["fill_value"] != "NaN" {
            return Err(unsupported("scale_offset requires float32 with NaN fill"));
        }
        Some(ScaleOffset::parse(&codecs[0])?)
    } else {
        None
    };
    if dtype == "int64" && (!dataset || shape.is_empty()) {
        let expected = if shape.is_empty() {
            json!([{"name":"bytes","configuration":{"endian":"little"}}, {"name":"zstd","configuration":{"level":0,"checksum":false}}])
        } else {
            json!([{"name":"bytes","configuration":{"endian":"little"}}, {"name":"blosc","configuration":{"typesize":8,"cname":"zstd","clevel":3,"shuffle":"shuffle","blocksize":0}}])
        };
        if meta["codecs"] != expected {
            return Err(unsupported("Unattested int64 codec chain"));
        }
    }
    let inner = if shape.is_empty() {
        outer
    } else if codecs.len() == 1 && codecs[0]["name"] == "sharding_indexed" {
        let c = &codecs[0]["configuration"];
        if !exact_keys(&codecs[0], &["name", "configuration"])
            || !exact_keys(
                c,
                &["chunk_shape", "codecs", "index_codecs", "index_location"],
            )
        {
            return Err(unsupported("Unknown shard configuration"));
        }
        let index = &c["index_codecs"];
        if index != &json!([{"name":"bytes","configuration":{"endian":"little"}},{"name":"crc32c"}])
            || !matches!(c["index_location"].as_str(), Some("start" | "end"))
        {
            return Err(unsupported(
                "Expected little-endian bytes+crc32c shard index at start or end",
            ));
        }
        numeric_codecs(
            &c["codecs"],
            width,
            dataset && (dtype.starts_with("int") || dtype.starts_with("uint")),
        )?;
        let inner = dimensions(&c["chunk_shape"])?;
        if inner.len() != shape.len()
            || inner.contains(&0)
            || outer.iter().zip(&inner).any(|(o, i)| o % i != 0)
        {
            return Err(unsupported("Invalid inner chunk shape"));
        }
        let counts: Vec<_> = outer.iter().zip(&inner).map(|(o, i)| o / i).collect();
        checked_bytes(&counts, 16, MAX_INDEX_BYTES - 4)?;
        inner
    } else {
        checked_bytes(&outer, width, chunk_limit(dtype))?;
        if dataset && member_zstd(meta) {
            checked_bytes(&outer, width, 128)?;
        } else {
            numeric_codec_slice(
                &codecs[usize::from(scale_offset.is_some())..],
                width,
                dataset && (dtype.starts_with("int") || dtype.starts_with("uint")),
            )?;
        }
        outer
    };
    checked_bytes(&inner, width, chunk_limit(dtype))?;
    Ok(Capability {
        dtype: dtype.into(),
        width,
        shape,
        inner,
        scale_offset,
    })
}

fn chunk_limit(dtype: &str) -> usize {
    match dtype {
        "int64" => MAX_INTEGER_CHUNK_BYTES,
        "float32" | "float64" => MAX_CHUNK_BYTES,
        _ => MAX_NARROW_INTEGER_CHUNK_BYTES,
    }
}

impl Capability {
    fn chunk_limit(&self) -> usize {
        chunk_limit(&self.dtype)
    }
}

/// Validate a full dimension-coordinate read and bound its chunk cardinality
/// without materializing selectors or selector groups. Used by metadata budgets.
pub fn coordinate_chunk_count(meta: &Value, name: &str) -> Result<usize, ReaderError> {
    let cap = attest_with_mode(meta, name, true)?;
    if cap.shape.len() != 1 {
        return Err(unsupported("Expected a one-dimensional coordinate"));
    }
    let size = usize::try_from(cap.shape[0])
        .map_err(|_| err(ErrorKind::SizeLimit, "Coordinate too large"))?;
    if size > MAX_SELECTORS || size > MAX_OUTPUT_BYTES / cap.width {
        return Err(err(ErrorKind::SizeLimit, "Coordinate too large"));
    }
    let chunks = cap.shape[0].div_ceil(cap.inner[0]);
    if chunks > MAX_CHUNKS as u64 {
        return Err(err(
            ErrorKind::SizeLimit,
            "Selection touches too many chunks",
        ));
    }
    Ok(chunks as usize)
}

type AxisGroup = (u64, Vec<(usize, u64)>);
struct Prepared {
    shape: Vec<usize>,
    groups: Vec<Vec<AxisGroup>>,
    elements: usize,
}
fn prepare(cap: &Capability, selection: &[Selector]) -> Result<Prepared, ReaderError> {
    if selection.len() != cap.shape.len() {
        return Err(err(
            ErrorKind::InvalidSelection,
            "Exactly one selector per stored dimension is required",
        ));
    }
    // Count distinct chunks before allocating any per-selector group buffers.
    // The bounded set is discarded per axis; even sparse/duplicate selections
    // cannot cause million-group staging before MAX_CHUNKS is enforced.
    let empty = selection
        .iter()
        .any(|s| matches!(s, Selector::Indices(xs) if xs.is_empty()));
    let mut chunks = 1usize;
    let mut selectors = 0usize;
    for ((selector, bound), chunk) in selection.iter().zip(&cap.shape).zip(&cap.inner) {
        let indices = match selector {
            Selector::Index(i) => std::slice::from_ref(i),
            Selector::Indices(xs) => xs.as_slice(),
        };
        selectors = selectors
            .checked_add(indices.len())
            .filter(|n| *n <= MAX_SELECTORS)
            .ok_or_else(|| err(ErrorKind::SizeLimit, "Too many selector indices"))?;
        let mut unique = std::collections::BTreeSet::new();
        for index in indices {
            let index = u64::try_from(*index)
                .map_err(|_| err(ErrorKind::InvalidSelection, "Negative index"))?;
            if index >= *bound {
                return Err(err(ErrorKind::InvalidSelection, "Index out of range"));
            }
            if !empty && unique.insert(index / chunk) && unique.len() > MAX_CHUNKS / chunks {
                return Err(err(
                    ErrorKind::SizeLimit,
                    "Selection touches too many chunks",
                ));
            }
        }
        if !empty {
            chunks *= unique.len();
        }
    }
    if empty {
        return Ok(Prepared {
            shape: selection
                .iter()
                .filter_map(|s| match s {
                    Selector::Indices(xs) => Some(xs.len()),
                    Selector::Index(_) => None,
                })
                .collect(),
            groups: Vec::new(),
            elements: 0,
        });
    }
    let mut shape = Vec::new();
    let mut groups = Vec::new();
    let mut selector_count = 0usize;
    for ((selector, bound), chunk) in selection.iter().zip(&cap.shape).zip(&cap.inner) {
        let indices = match selector {
            Selector::Index(i) => std::slice::from_ref(i),
            Selector::Indices(v) => {
                shape.push(v.len());
                v.as_slice()
            }
        };
        selector_count = selector_count
            .checked_add(indices.len())
            .filter(|n| *n <= MAX_SELECTORS)
            .ok_or_else(|| err(ErrorKind::SizeLimit, "Too many selector indices"))?;
        let mut axis: BTreeMap<u64, Vec<(usize, u64)>> = BTreeMap::new();
        for (position, index) in indices.iter().enumerate() {
            let index = u64::try_from(*index)
                .map_err(|_| err(ErrorKind::InvalidSelection, "Negative index"))?;
            if index >= *bound {
                return Err(err(ErrorKind::InvalidSelection, "Index out of range"));
            }
            axis.entry(index / chunk)
                .or_default()
                .push((position, index % chunk));
        }
        groups.push(axis.into_iter().collect());
    }
    // A zero axis makes the entire product zero, regardless of preceding axes.
    let elements = if shape.contains(&0) {
        0
    } else {
        product(shape.iter().copied(), MAX_OUTPUT_BYTES / cap.width)?
    };
    if elements != 0 {
        product(groups.iter().map(Vec::len), MAX_CHUNKS)?;
    }
    Ok(Prepared {
        shape,
        groups,
        elements,
    })
}
fn advance(position: &mut [usize], sizes: &[usize]) -> bool {
    for d in (0..position.len()).rev() {
        position[d] += 1;
        if position[d] < sizes[d] {
            return true;
        }
        position[d] = 0;
    }
    false
}

/// Read stored numeric values at an exact snapshot. Scalar axes are removed; lists are Cartesian.
pub async fn read_selection(
    descriptor: &StoreDescriptor,
    snapshot_id: &str,
    array_path: &str,
    selection: &[Selector],
) -> Result<TypedBuffer, ReaderError> {
    read_selection_traced(descriptor, snapshot_id, array_path, selection)
        .await
        .map(|(buffer, _)| buffer)
}
/// The trace describes logical store requests and inner chunk decode units, for offline tests.
pub async fn read_selection_traced(
    descriptor: &StoreDescriptor,
    snapshot_id: &str,
    array_path: &str,
    selection: &[Selector],
) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
    read_selection_traced_with_containers(descriptor, snapshot_id, array_path, selection, &[]).await
}

/// Read with only the explicitly declared anonymous HTTP virtual containers.
pub async fn read_selection_traced_with_containers(
    descriptor: &StoreDescriptor,
    snapshot_id: &str,
    array_path: &str,
    selection: &[Selector],
    containers: &[HttpVirtualChunkContainer],
) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
    tokio::time::timeout(
        Duration::from_secs(90),
        read_inner(
            descriptor,
            snapshot_id,
            array_path,
            selection,
            None,
            containers,
        ),
    )
    .await
    .map_err(|_| err(ErrorKind::Storage, "Reader timed out after 90 seconds"))?
}
/// Explicit CF read. Float dimension coordinates are decoded over the complete
/// array before selection, matching xarray's eager index construction.
/// Full decoding units obey the same output, selector and chunk-work bounds as raw reads.
pub async fn read_cf_selection_traced(
    descriptor: &StoreDescriptor,
    snapshot_id: &str,
    array_path: &str,
    selection: &[Selector],
    request: &cf_time::Request,
) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
    read_cf_selection_traced_with_containers(
        descriptor,
        snapshot_id,
        array_path,
        selection,
        request,
        &[],
    )
    .await
}

/// Explicit CF read with catalog-declared anonymous HTTP virtual containers.
pub async fn read_cf_selection_traced_with_containers(
    descriptor: &StoreDescriptor,
    snapshot_id: &str,
    array_path: &str,
    selection: &[Selector],
    request: &cf_time::Request,
    containers: &[HttpVirtualChunkContainer],
) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
    tokio::time::timeout(
        Duration::from_secs(90),
        read_inner(
            descriptor,
            snapshot_id,
            array_path,
            selection,
            Some(request),
            containers,
        ),
    )
    .await
    .map_err(|_| err(ErrorKind::Storage, "Reader timed out after 90 seconds"))?
}
async fn read_inner(
    descriptor: &StoreDescriptor,
    snapshot_id: &str,
    array_path: &str,
    selection: &[Selector],
    cf_request: Option<&cf_time::Request>,
    containers: &[HttpVirtualChunkContainer],
) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
    let _: icechunk::format::SnapshotId = snapshot_id
        .try_into()
        .map_err(|e| err(ErrorKind::Snapshot, e))?;
    validate_array_path(array_path)?;
    let session = ReadSession::open(
        descriptor,
        DatasetVersion::Snapshot(snapshot_id),
        containers,
    )
    .await?;
    session
        .read_inner(array_path, selection, cf_request, false)
        .await
}

/// A branch is resolved once; all reads use the resulting immutable snapshot.
#[derive(Clone, Copy, Debug)]
pub enum DatasetVersion<'a> {
    Branch(&'a str),
    Snapshot(&'a str),
}

/// Shared readonly Icechunk session. No repository/session is reopened by reads.
pub struct ReadSession {
    store: AsyncIcechunkStore,
    snapshot_id: String,
}
impl ReadSession {
    pub async fn open(
        descriptor: &StoreDescriptor,
        version: DatasetVersion<'_>,
        containers: &[HttpVirtualChunkContainer],
    ) -> Result<Self, ReaderError> {
        tokio::time::timeout(
            Duration::from_secs(90),
            Self::open_inner(descriptor, version, containers),
        )
        .await
        .map_err(|_| err(ErrorKind::Storage, "Reader timed out after 90 seconds"))?
    }
    async fn open_inner(
        descriptor: &StoreDescriptor,
        version: DatasetVersion<'_>,
        containers: &[HttpVirtualChunkContainer],
    ) -> Result<Self, ReaderError> {
        let declared_http_prefixes: Vec<String> = containers
            .iter()
            .map(|container| container.url_prefix.clone())
            .collect();
        let HttpContainers {
            containers,
            credentials,
        } = http_containers(containers)?;
        let storage = match descriptor {
            StoreDescriptor::Local(path) => {
                icechunk::new_local_filesystem_storage(std::path::Path::new(path))
                    .await
                    .map_err(|e| err(ErrorKind::Storage, e))?
            }
            StoreDescriptor::Http { url } => {
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err(unsupported("Expected HTTP(S) repository URL"));
                }
                Arc::new(
                    repository_http::BoundedRepositoryStorage::new(url)
                        .map_err(|e| err(ErrorKind::Storage, e))?,
                )
            }
            StoreDescriptor::AnonymousS3 {
                bucket,
                prefix,
                region,
            } => icechunk::new_s3_object_store_storage(
                S3Options::default()
                    .with_region(region)
                    .with_anonymous(true),
                bucket.clone(),
                Some(prefix.clone()),
                Some(S3Credentials::Anonymous),
                vec![],
                vec![],
            )
            .await
            .map_err(|e| err(ErrorKind::Storage, e))?,
        };
        let mut repo = Repository::open(
            Some(icechunk::RepositoryConfig {
                // Replace persisted containers, including their headers/options. Merely
                // opening a repository must never grant access to undeclared locations.
                virtual_chunk_containers: Some(containers),
                storage: Some(icechunk::storage::Settings {
                    retries: Some(icechunk::storage::RetriesSettings {
                        max_tries: std::num::NonZeroU16::new(2),
                        initial_backoff_ms: Some(100),
                        max_backoff_ms: Some(1000),
                    }),
                    timeouts: Some(icechunk::storage::TimeoutSettings {
                        connect_timeout_ms: Some(5000),
                        read_timeout_ms: Some(20000),
                        operation_timeout_ms: Some(30000),
                        operation_attempt_timeout_ms: Some(20000),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            storage,
            credentials,
        )
        .await
        .map_err(|e| err(ErrorKind::Storage, e))?;
        if !declared_http_prefixes.is_empty() {
            repo.set_http_virtual_chunk_fetcher(Arc::new(
                http::BoundedHttpFetcher::new(declared_http_prefixes)
                    .map_err(|e| err(ErrorKind::Storage, e))?,
            ));
        }
        let version = match version {
            DatasetVersion::Snapshot(id) => VersionInfo::SnapshotId(
                id.try_into()
                    .map_err(|_| err(ErrorKind::Snapshot, format!("Unknown snapshot {id:?}")))?,
            ),
            DatasetVersion::Branch(branch) => VersionInfo::BranchTipRef(branch.into()),
        };
        let session = repo.readonly_session(&version).await.map_err(|e| {
            let missing = matches!(
                e.kind(),
                icechunk::repository::RepositoryErrorKind::SnapshotNotFound { .. }
                    | icechunk::repository::RepositoryErrorKind::FormatError(
                        icechunk::format::IcechunkFormatErrorKind::SnapshotIdNotFound { .. }
                    )
            );
            match &version {
                VersionInfo::SnapshotId(id) if missing => err(
                    ErrorKind::Snapshot,
                    format!("Unknown snapshot {:?}", id.to_string()),
                ),
                _ => err(ErrorKind::Snapshot, e.kind()),
            }
        })?;
        let snapshot_id = session.snapshot_id().to_string();
        Ok(ReadSession {
            store: AsyncIcechunkStore::new(session),
            snapshot_id,
        })
    }
    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    /// Root Zarr metadata only (no chunk enumeration), charged before copying
    /// each serialized metadata object into the result. The Icechunk snapshot's
    /// upstream allocation is outside this bound; see the reader README.
    pub async fn root_metadata(
        &self,
        max_bytes: usize,
    ) -> Result<BTreeMap<String, Vec<u8>>, ReaderError> {
        tokio::time::timeout(Duration::from_secs(90), async {
            let session = self.store.session();
            let session = session.read().await;
            let nodes = session
                .list_nodes(&icechunk::format::Path::root())
                .await
                .map_err(|e| err(ErrorKind::Storage, e))?;
            let mut result = BTreeMap::new();
            let mut used = 0usize;
            for node in nodes {
                let node = node.map_err(|e| err(ErrorKind::Storage, e))?;
                let path = node.path.to_string();
                let name = path.trim_matches('/');
                if name.contains('/') {
                    continue;
                }
                // Shape/dimension names live outside user_data in Icechunk.
                // Charge their reconstructed JSON before asking the store to
                // serialize it. Escaped strings need at most six bytes/scalar.
                let extra = if let icechunk::format::snapshot::NodeData::Array {
                    shape,
                    dimension_names,
                    ..
                } = &node.node_data
                {
                    let mut extra = shape.len().checked_mul(64);
                    if let Some(names) = dimension_names {
                        for name in names {
                            let length = match name {
                                icechunk::format::snapshot::DimensionName::Name(name) => name.len(),
                                _ => 4,
                            };
                            extra = extra.and_then(|n| {
                                length
                                    .checked_mul(6)
                                    .and_then(|len| len.checked_add(8))
                                    .and_then(|len| n.checked_add(len))
                            });
                        }
                    }
                    extra
                } else {
                    Some(0)
                };
                used = used
                    .checked_add(node.user_data.len())
                    .and_then(|v| v.checked_add(name.len() + 96))
                    .and_then(|v| extra.and_then(|extra| v.checked_add(extra)))
                    .filter(|v| *v <= max_bytes)
                    .ok_or_else(|| {
                        err(
                            ErrorKind::SizeLimit,
                            "Opened dataset metadata exceeds byte limit",
                        )
                    })?;
                // user_data is Icechunk's stored Zarr metadata; use the store to
                // restore dimension names/shape exactly as xarray receives them.
                let key = if name.is_empty() {
                    "zarr.json".into()
                } else {
                    format!("{name}/zarr.json")
                };
                let raw = self
                    .store
                    .get(&StoreKey::new(key).map_err(unsupported)?)
                    .await
                    .map_err(|e| err(ErrorKind::Storage, e))?
                    .ok_or_else(|| err(ErrorKind::Storage, "Metadata missing"))?;
                used = used
                    .checked_add(raw.len())
                    .filter(|v| *v <= max_bytes)
                    .ok_or_else(|| {
                        err(
                            ErrorKind::SizeLimit,
                            "Opened dataset metadata exceeds byte limit",
                        )
                    })?;
                result.insert(name.to_string(), raw.to_vec());
            }
            Ok(result)
        })
        .await
        .map_err(|_| err(ErrorKind::Storage, "Reader timed out after 90 seconds"))?
    }

    /// Broader descriptive metadata/coordinate layouts for the dataset adapter;
    /// numeric codec, payload, output and selector bounds are unchanged.
    pub async fn read_dataset_selection(
        &self,
        array_path: &str,
        selection: &[Selector],
        cf: Option<&cf_time::Request>,
    ) -> Result<TypedBuffer, ReaderError> {
        tokio::time::timeout(
            Duration::from_secs(90),
            self.read_inner(array_path, selection, cf, true),
        )
        .await
        .map_err(|_| err(ErrorKind::Storage, "Reader timed out after 90 seconds"))?
        .map(|(buffer, _)| buffer)
    }
    async fn read_inner(
        &self,
        array_path: &str,
        selection: &[Selector],
        cf_request: Option<&cf_time::Request>,
        dataset: bool,
    ) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
        validate_array_path(array_path)?;
        let failure = Arc::new(Mutex::new(None));
        let result = self
            .read_array(array_path, selection, cf_request, dataset, failure.clone())
            .await;
        result.map_err(|mut error| {
            if dataset
                && let Some(failure) = failure.lock().unwrap_or_else(|e| e.into_inner()).take()
            {
                error.kind = ErrorKind::Storage;
                error.virtual_chunk_fetch = failure.virtual_fetch;
                error.message = failure.message;
                error.exception_class = Some(failure.exception_class);
            }
            error
        })
    }
    async fn read_array(
        &self,
        array_path: &str,
        selection: &[Selector],
        cf_request: Option<&cf_time::Request>,
        dataset: bool,
        failure: Arc<Mutex<Option<StorageFailure>>>,
    ) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
        let trace = Arc::new(Mutex::new(ReadTrace::default()));
        let store = Arc::new(ObservedStore {
            inner: AsyncIcechunkStore::from(self.store.session()),
            failure,
            trace: trace.clone(),
            payload: std::sync::OnceLock::new(),
            index: Mutex::new(None),
        });
        let key = StoreKey::new(format!("{array_path}/zarr.json"))
            .map_err(|e| unsupported(e.to_string()))?;
        let raw = store
            .get(&key)
            .await
            .map_err(|e| err(ErrorKind::Storage, e))?
            .ok_or_else(|| err(ErrorKind::Storage, "Array metadata missing"))?;
        let meta: Value = serde_json::from_slice(&raw).map_err(|e| unsupported(e.to_string()))?;
        let cap = if dataset {
            attest_with_mode(&meta, array_path, true)?
        } else {
            attest(&meta, array_path)?
        };
        refuse_bounds_targets(&store, array_path).await?;
        store
            .payload
            .set(PayloadPolicy::from_metadata(&meta, &cap)?)
            .map_err(|_| err(ErrorKind::Storage, "Payload policy already set"))?;
        let requested = prepare(&cap, selection)?;
        let full_selection = if let Some(request) = cf_request {
            if request.revision != cf_time::REVISION {
                return Err(unsupported("Unattested CF revision"));
            }
            match cf_time::attested_encoding(&cap.dtype, &request.attributes)? {
                cf_time::Encoding::DurationSeconds
                    if meta["dimension_names"]
                        == json!([array_path.rsplit('/').next().unwrap()]) =>
                {
                    let size = usize::try_from(cap.shape[0])
                        .map_err(|_| err(ErrorKind::SizeLimit, "CF decoding unit too large"))?;
                    if size > MAX_SELECTORS || size > MAX_OUTPUT_BYTES / cap.width {
                        return Err(err(ErrorKind::SizeLimit, "CF decoding unit too large"));
                    }
                    Some(vec![Selector::Indices((0..size as i64).collect())])
                }
                // Auxiliary variables decode lazily after isel in xarray. Integer
                // epochs have no value-dependent context, so selection is safe too.
                cf_time::Encoding::DurationSeconds | cf_time::Encoding::EpochSeconds => None,
            }
        } else {
            None
        };
        let requested_selection = selection;
        let selection = full_selection.as_deref().unwrap_or(selection);
        let prepared = prepare(&cap, selection)?;
        let mut bytes = vec![0u8; prepared.elements * cap.width];
        trace.lock().unwrap_or_else(|e| e.into_inner()).output_bytes = bytes.len();
        if cap.shape.is_empty() {
            let key = StoreKey::new(format!("{array_path}/c")).map_err(unsupported)?;
            bytes = match store
                .get(&key)
                .await
                .map_err(|e| err(ErrorKind::Decode, e))?
            {
                Some(encoded) => {
                    decode_scalar_zstd(&encoded).map_err(|e| err(ErrorKind::Decode, e))?
                }
                None => meta["fill_value"]
                    .as_i64()
                    .ok_or_else(|| unsupported("Invalid int64 fill"))?
                    .to_le_bytes()
                    .to_vec(),
            };
            let mut trace = trace.lock().unwrap_or_else(|e| e.into_inner());
            trace.decoded_inner_chunks.push(vec![]);
            trace.decoded_bytes = 8;
            trace.subset_bytes = 8;
            trace.max_concurrent_chunks = 1;
        } else if prepared.elements > 0 {
            let member_zstd = member_zstd(&meta);
            // Scale_offset is decoded locally in place; zarrs receives only the
            // attested bytes/Blosc suffix. NaN fill is unchanged by the transform.
            let array = if member_zstd {
                None
            } else if cap.scale_offset.is_some() {
                let mut metadata = meta.clone();
                metadata["codecs"]
                    .as_array_mut()
                    .expect("attested codecs")
                    .remove(0);
                let metadata =
                    serde_json::from_value(metadata).map_err(|e| unsupported(e.to_string()))?;
                Some(
                    Array::new_with_metadata(
                        store.clone(),
                        &format!("/{array_path}"),
                        zarrs::metadata::ArrayMetadata::V3(metadata),
                    )
                    .map_err(|e| unsupported(e.to_string()))?,
                )
            } else {
                Some(
                    Array::async_open(store.clone(), &format!("/{array_path}"))
                        .await
                        .map_err(|e| unsupported(e.to_string()))?,
                )
            };
            let options = CodecOptions::default().with_concurrent_target(1);
            let sizes: Vec<_> = prepared.groups.iter().map(Vec::len).collect();
            let mut position = vec![0; sizes.len()];
            loop {
                let chosen: Vec<_> = prepared
                    .groups
                    .iter()
                    .zip(&position)
                    .map(|(g, p)| &g[*p])
                    .collect();
                let chunk: Vec<_> = chosen.iter().map(|g| g.0).collect();
                let start: Vec<_> = chunk.iter().zip(&cap.inner).map(|(i, n)| i * n).collect();
                let shape: Vec<_> = start
                    .iter()
                    .zip(&cap.inner)
                    .zip(&cap.shape)
                    .map(|((s, n), b)| (*n).min(b - s))
                    .collect();
                let subset = ArraySubset::new_with_start_shape(start, shape.clone())
                    .map_err(|e| err(ErrorKind::Decode, e))?;
                // The codec has already decoded the admitted numeric dtype to
                // native-endian fixed bytes. Avoid a separate generic retrieval
                // future and typed-buffer copy for each integer/float width.
                let mut decoded = if member_zstd {
                    let key = StoreKey::new(format!("{array_path}/c/{}", chunk[0]))
                        .map_err(unsupported)?;
                    let size = checked_bytes(&cap.inner, cap.width, 128)?;
                    let mut decoded = match store
                        .get(&key)
                        .await
                        .map_err(|e| err(ErrorKind::Decode, e))?
                    {
                        Some(encoded) => decode_member_zstd(&encoded, size)
                            .map_err(|e| err(ErrorKind::Decode, e))?,
                        None => {
                            let fill =
                                meta["fill_value"].as_i64().expect("attested int16 fill") as i16;
                            fill.to_le_bytes().repeat(size / 2)
                        }
                    };
                    decoded.truncate(checked_bytes(&shape, cap.width, 128)?);
                    decoded
                } else {
                    array
                        .as_ref()
                        .expect("numeric array")
                        .async_retrieve_array_subset_opt::<ArrayBytes<'static>>(&subset, &options)
                        .await
                        .map_err(|e| err(ErrorKind::Decode, e))?
                        .into_fixed()
                        .map_err(|e| err(ErrorKind::Decode, e))?
                        .into_owned()
                };
                #[cfg(target_endian = "big")]
                {
                    if !member_zstd {
                        for element in decoded.chunks_exact_mut(cap.width) {
                            element.reverse();
                        }
                    }
                }
                if let Some(transform) = cap.scale_offset {
                    transform.decode(&mut decoded);
                }
                let count = checked_bytes(&shape, cap.width, cap.chunk_limit())?;
                if decoded.len() != count {
                    return Err(err(
                        ErrorKind::Decode,
                        "Decoded byte length differs from shape",
                    ));
                }
                {
                    let mut trace = trace.lock().unwrap_or_else(|e| e.into_inner());
                    trace.decoded_inner_chunks.push(chunk);
                    trace.decoded_bytes = trace
                        .decoded_bytes
                        .checked_add(checked_bytes(&cap.inner, cap.width, cap.chunk_limit())?)
                        .ok_or_else(|| err(ErrorKind::SizeLimit, "Decoded work overflow"))?;
                    trace.subset_bytes = trace
                        .subset_bytes
                        .checked_add(count)
                        .ok_or_else(|| err(ErrorKind::SizeLimit, "Subset byte count overflow"))?;
                    trace.max_concurrent_chunks = 1;
                }
                let scatter_sizes: Vec<_> = chosen.iter().map(|g| g.1.len()).collect();
                let mut scatter = vec![0; sizes.len()];
                loop {
                    let mut output = 0usize;
                    let mut source = 0usize;
                    for d in 0..sizes.len() {
                        let (out, index) = chosen[d].1[scatter[d]];
                        source = source * (shape[d] as usize) + (index as usize);
                        if let Selector::Indices(v) = &selection[d] {
                            output = output * v.len() + out;
                        }
                    }
                    bytes[output * cap.width..(output + 1) * cap.width]
                        .copy_from_slice(&decoded[source * cap.width..(source + 1) * cap.width]);
                    if !advance(&mut scatter, &scatter_sizes) {
                        break;
                    }
                }
                if !advance(&mut position, &sizes) {
                    break;
                }
            }
        }
        drop(store);
        let trace = Arc::try_unwrap(trace)
            .map_err(|_| err(ErrorKind::Storage, "Trace still shared"))?
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());
        let mut buffer = TypedBuffer {
            dtype: cap.dtype,
            shape: prepared.shape,
            bytes,
        };
        let mut trace = trace;
        if let Some(request) = cf_request {
            buffer = cf_time::decode(buffer, request)?;
            if full_selection.is_some() {
                // prepare() already checked every index and the selected output size.
                let indices = match &requested_selection[0] {
                    Selector::Index(index) => std::slice::from_ref(index),
                    Selector::Indices(indices) => indices.as_slice(),
                };
                let mut bytes = Vec::with_capacity(requested.elements * 8);
                for index in indices {
                    let offset = *index as usize * 8;
                    bytes.extend_from_slice(&buffer.bytes[offset..offset + 8]);
                }
                buffer.shape = requested.shape;
                buffer.bytes = bytes;
            }
        }
        trace.output_bytes = buffer.bytes.len();
        Ok((buffer, trace))
    }
}

fn validate_array_path(array_path: &str) -> Result<(), ReaderError> {
    if array_path.is_empty()
        || array_path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        Err(unsupported("Expected relative array path"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn member_zstd_encoded_boundary_frames_are_valid_and_size_checked() {
        for raw in [false, true] {
            // RFC 8878: single-segment content size 2, empty raw blocks, then
            // two zero bytes in an RLE (4-byte) or raw (5-byte) final block.
            let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, 2];
            frame.resize(frame.len() + 82 * 3, 0);
            frame.extend_from_slice(if raw {
                &[0x11, 0, 0, 0, 0]
            } else {
                &[0x13, 0, 0, 0]
            });
            assert_eq!(frame.len(), if raw { 257 } else { 256 });
            // Bypass the reader policy to prove that the over-limit control
            // is a valid frame, rather than failing on unrelated corruption.
            assert_eq!(zstd::bulk::decompress(&frame, 2).unwrap(), [0, 0]);
            assert_eq!(validate_member_zstd(&frame, 2).is_ok(), !raw);
        }
    }

    #[test]
    fn decoded_unit_boundaries_and_zstd_window_are_fixed() {
        assert_eq!(MAX_CHUNK_BYTES, 58_720_256);
        assert_eq!(chunk_limit("int16"), 16 * 1024 * 1024);
        assert_eq!(chunk_limit("int64"), 64 * 1024 * 1024);
        assert_eq!(
            checked_bytes(&[MAX_CHUNK_BYTES as u64], 1, MAX_CHUNK_BYTES).unwrap(),
            MAX_CHUNK_BYTES
        );
        assert_eq!(
            checked_bytes(&[MAX_CHUNK_BYTES as u64 + 1], 1, MAX_CHUNK_BYTES)
                .unwrap_err()
                .kind,
            ErrorKind::SizeLimit
        );
        assert_eq!(
            checked_bytes(
                &[MAX_INTEGER_CHUNK_BYTES as u64],
                1,
                MAX_INTEGER_CHUNK_BYTES
            )
            .unwrap(),
            MAX_INTEGER_CHUNK_BYTES
        );
        let input = [i16::MIN, 0, i16::MAX]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        let encoded = zstd::bulk::compress(&input, 0).unwrap();
        assert_eq!(decode_member_zstd(&encoded, 6).unwrap(), input);
        for malformed in [
            [encoded.clone(), encoded.clone()].concat(),
            [encoded.clone(), vec![0]].concat(),
            vec![0; 257],
        ] {
            assert!(decode_member_zstd(&malformed, 6).is_err());
        }
        assert!(decode_member_zstd(&encoded, 8).is_err());
        // A syntactically valid non-single-segment frame may advertise a huge
        // window despite its six-byte content size. Limit native scratch first.
        let mut large_window = vec![0x28, 0xb5, 0x2f, 0xfd, 0x80, 0xa0, 6, 0, 0, 0];
        large_window.extend_from_slice(&[0x31, 0, 0]);
        large_window.extend_from_slice(&input);
        assert!(decode_member_zstd(&large_window, 6).is_err());
        large_window[5] = 0;
        assert_eq!(decode_member_zstd(&large_window, 6).unwrap(), input);
    }

    #[test]
    fn typed_virtual_failure_retains_source_diagnostics() {
        use icechunk::format::manifest::VirtualReferenceErrorKind;
        use icechunk::{
            session::SessionErrorKind,
            store::{StoreError, StoreErrorKind},
        };
        for diagnostic in [
            "operation timeout (all attempts including retries) occurred after 20s",
            "Error { code: \"SlowDown\", message: \"synthetic\" }",
        ] {
            let error = StoreError::capture(StoreErrorKind::SessionError(
                SessionErrorKind::VirtualReferenceError(VirtualReferenceErrorKind::FetchError(
                    Box::new(std::io::Error::other(diagnostic)),
                )),
            ));
            let failure = storage_failure(&error);
            assert!(failure.virtual_fetch);
            assert!(failure.message.contains(diagnostic));
            assert_eq!(failure.exception_class, "StorageError");
            let generic = StoreError::capture(StoreErrorKind::Other(diagnostic.into()));
            assert!(!storage_failure(&generic).virtual_fetch);
        }
        let permanent = StoreError::capture(StoreErrorKind::SessionError(
            SessionErrorKind::VirtualReferenceError(VirtualReferenceErrorKind::NoContainerForName(
                "synthetic".into(),
            )),
        ));
        assert!(!storage_failure(&permanent).virtual_fetch);
        assert_eq!(
            storage_failure(&permanent).exception_class,
            "InvalidInputError"
        );
    }
    #[test]
    fn checked_work_and_output() {
        assert!(product([usize::MAX, 2], usize::MAX).is_err());
        let cap = Capability {
            dtype: "float64".into(),
            width: 8,
            shape: vec![u64::MAX, 4],
            inner: vec![1, 1],
            scale_offset: None,
        };
        assert_eq!(
            prepare(&cap, &[Selector::Index(-1), Selector::Index(0)])
                .err()
                .unwrap()
                .kind,
            ErrorKind::InvalidSelection
        );
        assert!(prepare(&cap, &[Selector::Index(0)]).is_err());
        let p = prepare(
            &cap,
            &[Selector::Indices(vec![]), Selector::Indices(vec![3, 1, 3])],
        )
        .unwrap();
        assert_eq!(p.shape, vec![0, 3]);
        assert_eq!(p.elements, 0);
        assert!(checked_bytes(&[u64::MAX, u64::MAX], 8, MAX_CHUNK_BYTES).is_err());
        let p = prepare(
            &cap,
            &[
                Selector::Indices(vec![0; 4096]),
                Selector::Indices(vec![0; 4096]),
            ],
        );
        assert_eq!(p.err().unwrap().kind, ErrorKind::SizeLimit);
    }
}
