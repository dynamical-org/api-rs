//! Readonly repository HTTP transport. Validate and bound encoded objects before
//! Icechunk's config/asset readers can collect them. Metadata decoding is upstream.
use std::{fmt, ops::Range, pin::Pin, time::Duration};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{
    Stream, StreamExt,
    stream::{self, BoxStream},
};
use icechunk_storage::{
    DeleteObjectsResult, GetModifiedResult, ListInfo, Settings, Storage, StorageError, StorageInfo,
    StorageResult, VersionInfo, VersionedUpdateResult, obj_not_found_res, obj_store_error,
};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncBufRead, sync::OnceCell};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RANGE_BYTES: u64 = (super::MAX_INTEGER_CHUNK_BYTES + 16 + super::MAX_INDEX_BYTES) as u64;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send>>;
type Reader = Pin<Box<dyn AsyncBufRead + Send>>;

#[derive(Debug, thiserror::Error)]
enum ResponseError {
    #[error("Invalid repository URL or object path")]
    Path,
    #[error("Repository HTTP redirects are disabled")]
    Redirect,
    #[error("Unexpected repository HTTP status {0}")]
    Status(u16),
    #[error("Repository HTTP Content-Range differs from requested range")]
    Range,
    #[error("Repository HTTP Content-Length or body length is inconsistent")]
    Length,
    #[error("Repository HTTP object exceeds encoded byte limit {0}")]
    Size(u64),
    #[error("Repository HTTP storage is readonly; operation is unsupported")]
    Readonly,
    #[error("Repository HTTP Last-Modified is missing or invalid")]
    LastModified,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BoundedRepositoryStorage {
    url: String,
    #[serde(skip)]
    client: OnceCell<reqwest::Client>,
}

impl BoundedRepositoryStorage {
    pub(crate) fn new(url: &str) -> StorageResult<Self> {
        let storage = Self {
            url: url.trim_end_matches('/').to_owned(),
            client: OnceCell::new(),
        };
        storage.object_url("repo")?;
        Ok(storage)
    }

    fn object_url(&self, path: &str) -> StorageResult<reqwest::Url> {
        let mut url =
            reqwest::Url::parse(&self.url).map_err(|_| obj_store_error(ResponseError::Path))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || path.split('/').any(|p| {
                p.is_empty()
                    || p == "."
                    || p == ".."
                    || p.bytes()
                        .any(|b| !b.is_ascii_alphanumeric() && !b"-_.".contains(&b))
            })
        {
            return Err(obj_store_error(ResponseError::Path));
        }
        url.path_segments_mut()
            .map_err(|_| obj_store_error(ResponseError::Path))?
            .pop_if_empty()
            .extend(path.split('/'));
        Ok(url)
    }

    async fn client(&self) -> StorageResult<&reqwest::Client> {
        self.client
            .get_or_try_init(|| async {
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    // Keep wire byte limits even if another dependency enables codecs.
                    .no_gzip()
                    .no_brotli()
                    .no_deflate()
                    .no_zstd()
                    .user_agent(icechunk_types::user_agent())
                    .connect_timeout(Duration::from_secs(5))
                    .read_timeout(Duration::from_secs(20))
                    .timeout(Duration::from_secs(20))
                    .build()
                    .map_err(obj_store_error)
            })
            .await
    }

    async fn response(
        &self,
        path: &str,
        range: Option<&Range<u64>>,
        previous: Option<&VersionInfo>,
        head: bool,
    ) -> StorageResult<reqwest::Response> {
        let url = self.object_url(path)?;
        let client = self.client().await?;
        for attempt in 0..2 {
            let mut request = if head {
                client.head(url.clone())
            } else {
                client.get(url.clone())
            };
            // Prevent transparent content coding from changing the meaning of byte ranges.
            request = request.header(header::ACCEPT_ENCODING, "identity");
            if let Some(range) = range {
                request = request.header(
                    header::RANGE,
                    format!("bytes={}-{}", range.start, range.end - 1),
                );
            }
            if let Some(etag) = previous.and_then(VersionInfo::etag) {
                request = request.header(header::IF_NONE_MATCH, etag);
            }
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if attempt == 0
                        && (status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS)
                    {
                        // Drop error bodies without collecting them, including retry bodies.
                        drop(response);
                    } else {
                        return Ok(response);
                    }
                }
                Err(error) if attempt == 1 => return Err(obj_store_error(error)),
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        unreachable!("the final request returns its result")
    }

    async fn read(
        &self,
        path: &str,
        range: Option<&Range<u64>>,
        previous: Option<&VersionInfo>,
    ) -> StorageResult<Option<(Bytes, VersionInfo)>> {
        let limit = match range {
            Some(range) => range
                .end
                .checked_sub(range.start)
                .filter(|n| *n > 0 && *n <= MAX_RANGE_BYTES)
                .ok_or_else(|| obj_store_error(ResponseError::Size(MAX_RANGE_BYTES)))?,
            None if matches!(path, "config.yaml" | "repo") => MAX_CONFIG_BYTES,
            None if path.starts_with("chunks/") => MAX_RANGE_BYTES,
            None => MAX_ASSET_BYTES,
        };
        let response = self.response(path, range, previous, false).await?;
        if response.status() == StatusCode::NOT_MODIFIED
            && previous.and_then(VersionInfo::etag).is_some()
        {
            return Ok(None);
        }
        validate_status(response.status(), range.is_some())?;
        if response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err(obj_store_error(ResponseError::Length));
        }
        if let Some(range) = range {
            validate_range(response.headers().get(header::CONTENT_RANGE), range)?;
        } else if response.headers().contains_key(header::CONTENT_RANGE) {
            return Err(obj_store_error(ResponseError::Range));
        }
        let length = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| obj_store_error(ResponseError::Length))
            })
            .transpose()?;
        if length.is_some_and(|n| n > limit) {
            return Err(obj_store_error(ResponseError::Size(limit)));
        }
        if range.is_some() && length.is_some_and(|n| n != limit) {
            return Err(obj_store_error(ResponseError::Length));
        }
        let version = VersionInfo {
            etag: response
                .headers()
                .get(header::ETAG)
                .map(|value| value.to_str().map(|v| icechunk_storage::ETag(v.to_owned())))
                .transpose()
                .map_err(obj_store_error)?,
            generation: None,
        };
        let expected = if range.is_some() { Some(limit) } else { length };
        let bytes = collect(response, expected, limit).await?;
        Ok(Some((bytes, version)))
    }
}

fn validate_status(status: StatusCode, ranged: bool) -> StorageResult<()> {
    if status == StatusCode::NOT_FOUND {
        return obj_not_found_res();
    }
    if status.is_redirection() {
        return Err(obj_store_error(ResponseError::Redirect));
    }
    if status
        != if ranged {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        }
    {
        return Err(obj_store_error(ResponseError::Status(status.as_u16())));
    }
    Ok(())
}

fn validate_range(value: Option<&header::HeaderValue>, range: &Range<u64>) -> StorageResult<()> {
    let valid = value
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes "))
        .and_then(|v| v.split_once('/'))
        .and_then(|(span, total)| {
            let (first, last) = span.split_once('-')?;
            Some((
                first.parse::<u64>().ok()?,
                last.parse::<u64>().ok()?.checked_add(1)?,
                total.parse::<u64>().ok()?,
            ))
        })
        .is_some_and(|(first, end, total)| {
            first == range.start && end == range.end && end <= total
        });
    if !valid {
        return Err(obj_store_error(ResponseError::Range));
    }
    Ok(())
}

async fn collect(
    response: reqwest::Response,
    expected: Option<u64>,
    limit: u64,
) -> StorageResult<Bytes> {
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    let cap = expected.unwrap_or(limit);
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.map_err(obj_store_error)?;
        if bytes.len() as u64 > cap - data.len() as u64 {
            return Err(obj_store_error(ResponseError::Size(cap)));
        }
        let needed = data.len() + bytes.len();
        if needed > data.capacity() {
            // Neither headers nor geometric Vec growth may allocate beyond the cap.
            let capacity = needed
                .max(data.capacity().saturating_mul(2))
                .min(cap as usize);
            data.reserve_exact(capacity - data.len());
        }
        data.extend_from_slice(&bytes);
    }
    if expected.is_some_and(|n| n != data.len() as u64) {
        return Err(obj_store_error(ResponseError::Length));
    }
    Ok(data.into())
}

impl fmt::Display for BoundedRepositoryStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Bounded readonly HTTP storage")
    }
}

// Required marker in the pinned upstream Storage trait. Re-audit on upgrades.
impl icechunk_types::sealed::Sealed for BoundedRepositoryStorage {}

#[async_trait::async_trait]
#[typetag::serde(name = "dynapi_bounded_readonly_http")]
impl Storage for BoundedRepositoryStorage {
    fn storage_info(&self) -> StorageInfo {
        StorageInfo {
            backend_type: "bounded HTTP",
            fields: vec![],
        }
    }
    async fn can_write(&self) -> StorageResult<bool> {
        Ok(false)
    }

    // Avoid the upstream parallel range collector and its per-part allocations.
    async fn get_object(
        &self,
        settings: &Settings,
        path: &str,
        range: Option<&Range<u64>>,
    ) -> StorageResult<(Reader, VersionInfo)> {
        self.get_object_range_read(settings, path, range).await
    }
    async fn get_object_range(
        &self,
        _settings: &Settings,
        path: &str,
        range: Option<&Range<u64>>,
    ) -> StorageResult<(ByteStream, VersionInfo)> {
        let (bytes, version) = self
            .read(path, range, None)
            .await?
            .ok_or_else(|| obj_store_error(ResponseError::Status(304)))?;
        Ok((Box::pin(stream::once(async { Ok(bytes) })), version))
    }
    async fn get_object_conditional(
        &self,
        _settings: &Settings,
        path: &str,
        previous: Option<&VersionInfo>,
    ) -> StorageResult<GetModifiedResult> {
        match self.read(path, None, previous).await? {
            Some((bytes, new_version)) => Ok(GetModifiedResult::Modified {
                data: Box::pin(std::io::Cursor::new(bytes)),
                new_version,
            }),
            None => Ok(GetModifiedResult::OnLatestVersion),
        }
    }
    async fn get_object_last_modified(
        &self,
        path: &str,
        _settings: &Settings,
    ) -> StorageResult<DateTime<Utc>> {
        let response = self.response(path, None, None, true).await?;
        validate_status(response.status(), false)?;
        response
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| DateTime::parse_from_rfc2822(v).ok())
            .map(|v| v.with_timezone(&Utc))
            .ok_or_else(|| obj_store_error(ResponseError::LastModified))
    }
    async fn put_object(
        &self,
        _: &Settings,
        _: &str,
        _: Bytes,
        _: Option<&str>,
        _: Vec<(String, String)>,
        _: Option<&VersionInfo>,
    ) -> StorageResult<VersionedUpdateResult> {
        Err(obj_store_error(ResponseError::Readonly))
    }
    async fn copy_object(
        &self,
        _: &Settings,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &VersionInfo,
    ) -> StorageResult<VersionedUpdateResult> {
        Err(obj_store_error(ResponseError::Readonly))
    }
    async fn list_objects<'a>(
        &'a self,
        _: &Settings,
        _: &str,
    ) -> StorageResult<BoxStream<'a, StorageResult<ListInfo<String>>>> {
        Err(obj_store_error(ResponseError::Readonly))
    }
    async fn delete_batch(
        &self,
        _: &Settings,
        _: &str,
        _: Vec<(String, u64)>,
    ) -> StorageResult<DeleteObjectsResult> {
        Err(obj_store_error(ResponseError::Readonly))
    }
}

#[cfg(test)]
#[path = "repository_http_tests.rs"]
mod tests;
