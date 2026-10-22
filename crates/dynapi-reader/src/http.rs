//! Runtime-only HTTP virtual transport. Authorization and checksum validation
//! remain in Icechunk; object_store retains its range and retry implementation.
use std::{ops::Range, sync::Arc, time::Duration};

use futures::StreamExt;
use icechunk::{
    config::HttpConfig,
    format::manifest::{Checksum, VirtualReferenceError, VirtualReferenceErrorKind},
    virtual_chunks::{HttpVirtualChunkFetcher, HttpVirtualChunkResponse},
};
use icechunk_arrow_object_store::object_store::{
    self, BackoffConfig, ClientOptions, GetOptions, ObjectStore, RetryConfig,
    client::{
        HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse, HttpService,
    },
    http::HttpBuilder,
    path::Path,
};

#[derive(Debug, Clone)]
struct LastModifiedPresent(bool);

#[derive(Debug, Clone)]
pub(crate) struct BoundedHttpFetcher {
    client: reqwest::Client,
    prefixes: Arc<[String]>,
}

impl BoundedHttpFetcher {
    pub(crate) fn new(prefixes: Vec<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(icechunk_types::user_agent())
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(20))
                .build()?,
            prefixes: prefixes.into(),
        })
    }
}

fn invalid_response(message: impl ToString) -> HttpError {
    HttpError::new(
        HttpErrorKind::Decode,
        std::io::Error::other(message.to_string()),
    )
}

fn fetch_error(error: impl std::error::Error + Send + Sync + 'static) -> VirtualReferenceError {
    VirtualReferenceError::capture(VirtualReferenceErrorKind::FetchError(Box::new(error)))
}

impl HttpConnector for BoundedHttpFetcher {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(self.clone()))
    }
}

#[async_trait::async_trait]
impl HttpService for BoundedHttpFetcher {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let range = request
            .headers()
            .get("range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
            .and_then(|(first, last)| {
                Some((
                    first.parse::<u64>().ok()?,
                    last.parse::<u64>().ok()?.checked_add(1)?,
                ))
            })
            .filter(|(first, end)| first < end)
            .ok_or_else(|| invalid_response("Expected a bounded HTTP byte range"))?;
        let mut response = HttpService::call(&self.client, request).await?;
        if !response.status().is_success() {
            // object_store handles statuses (including all 3xx) and retries. It
            // otherwise collects error text without a bound, so discard it here.
            *response.body_mut() = Vec::<u8>::new().into();
            return Ok(response);
        }
        if response.status().as_u16() != 206 {
            return Err(invalid_response("Expected HTTP 206 for a byte range"));
        }
        // Validate before object_store parses the header too: its parser adds
        // one to the inclusive endpoint without checked arithmetic.
        let valid_range = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes "))
            .and_then(|value| value.split_once('/'))
            .and_then(|(span, total)| {
                let (first, last) = span.split_once('-')?;
                Some((
                    first.parse::<u64>().ok()?,
                    last.parse::<u64>().ok()?.checked_add(1)?,
                    total.parse::<u64>().ok()?,
                ))
            })
            .is_some_and(|(first, end, total)| (first, end) == range && end <= total);
        if !valid_range {
            return Err(invalid_response(
                "Content-Range differs from requested range",
            ));
        }
        let expected = range.1 - range.0;
        if let Some(length) = response.headers().get("content-length") {
            if length
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                != Some(expected)
            {
                return Err(invalid_response(
                    "Content-Length differs from requested range",
                ));
            }
        } else {
            // object_store requires a size for metadata. Supply the already
            // validated range length without changing the HTTP body's framing;
            // the streaming collector below still checks actual bytes and EOF.
            response.headers_mut().insert(
                "content-length",
                expected.to_string().parse().map_err(invalid_response)?,
            );
        }
        let present = response.headers().contains_key("last-modified");
        response
            .extensions_mut()
            .insert(LastModifiedPresent(present));
        Ok(response)
    }
}

#[async_trait::async_trait]
impl HttpVirtualChunkFetcher for BoundedHttpFetcher {
    async fn fetch(
        &self,
        url: &str,
        range: &Range<u64>,
        checksum: Option<&Checksum>,
        _config: &HttpConfig,
    ) -> Result<HttpVirtualChunkResponse, VirtualReferenceError> {
        let expected = range
            .end
            .checked_sub(range.start)
            .filter(|length| {
                *length > 0
                    && *length
                        <= (super::MAX_INTEGER_CHUNK_BYTES + 16 + super::MAX_INDEX_BYTES) as u64
            })
            .ok_or_else(|| fetch_error(std::io::Error::other("Invalid HTTP virtual byte range")))?;
        let url = reqwest::Url::parse(url).map_err(fetch_error)?;
        let path = Path::from_url_path(url.path()).map_err(fetch_error)?;
        // Authorize the exact URL object_store will request, after decoding its
        // object key and re-encoding path segments. Icechunk's callback and native
        // routes differ on encoded slashes; neither may widen the caller's prefix.
        let mut physical_url =
            reqwest::Url::parse(&url.origin().ascii_serialization()).map_err(fetch_error)?;
        physical_url
            .path_segments_mut()
            .map_err(|_| fetch_error(std::io::Error::other("Invalid HTTP origin")))?
            .extend(path.parts());
        if !self
            .prefixes
            .iter()
            .any(|prefix| physical_url.as_str().starts_with(prefix))
        {
            return Err(VirtualReferenceError::capture(
                VirtualReferenceErrorKind::NoContainerForUrl(url.to_string()),
            ));
        }
        let client = HttpBuilder::new()
            .with_url(url.origin().ascii_serialization())
            .with_client_options(ClientOptions::new().with_allow_http(url.scheme() == "http"))
            .with_http_connector(self.clone())
            .with_retry(RetryConfig {
                backoff: BackoffConfig {
                    init_backoff: Duration::from_millis(100),
                    max_backoff: Duration::from_secs(1),
                    base: 2.,
                },
                max_retries: 1,
                retry_timeout: Duration::from_secs(90),
            })
            .build()
            .map_err(fetch_error)?;
        let mut options = GetOptions {
            range: Some(range.clone().into()),
            ..Default::default()
        };
        match checksum {
            Some(Checksum::ETag(etag)) => {
                options.if_match = Some(icechunk::storage::strip_quotes(&etag.0).to_owned());
            }
            Some(Checksum::LastModified(seconds)) => {
                options.if_unmodified_since =
                    Some((std::time::UNIX_EPOCH + Duration::from_secs(seconds.0 as u64)).into());
            }
            None => {}
        }
        let result = client.get_opts(&path, options).await.map_err(fetch_error)?;
        let etag = result.meta.e_tag.clone();
        let last_modified = result
            .extensions
            .get::<LastModifiedPresent>()
            .filter(|present| present.0)
            .and_then(|_| u32::try_from(result.meta.last_modified.timestamp()).ok());
        let mut stream = result.into_stream();
        // Never reserve from response headers. No data frame is copied into the
        // collector unless it fits in the authorized requested byte range.
        let mut data = Vec::with_capacity(expected as usize);
        while let Some(bytes) = stream.next().await {
            let bytes = bytes.map_err(fetch_error)?;
            if bytes.len() as u64 > expected - data.len() as u64 {
                return Err(fetch_error(std::io::Error::other(
                    "HTTP body exceeds requested range",
                )));
            }
            data.extend_from_slice(&bytes);
        }
        if data.len() as u64 != expected {
            return Err(fetch_error(std::io::Error::other(
                "HTTP body shorter than requested range",
            )));
        }
        Ok(HttpVirtualChunkResponse {
            data: data.into(),
            etag,
            last_modified,
        })
    }
}
