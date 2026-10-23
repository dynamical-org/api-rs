//! One connector and bounded collector for the HTTP adapter and opt-in providers.
use std::{
    ops::Range,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures::StreamExt;
use http_body::{Body, Frame, SizeHint};
use icechunk::{
    format::manifest::{Checksum, VirtualReferenceError},
    virtual_chunks::HttpVirtualChunkResponse,
};
use icechunk_arrow_object_store::object_store::{
    self, BackoffConfig, ClientOptions, GetOptions, ObjectStore, RetryConfig,
    client::{
        HttpClient, HttpConnector, HttpError, HttpRequest, HttpResponse, HttpResponseBody,
        HttpService,
    },
    http::HttpBuilder,
    path::Path,
};

use super::{
    fetch_error, invalid_response,
    provider::{Provider, ProviderFailure, slow_down},
};

pub(super) fn expected_length(range: &Range<u64>) -> Result<u64, VirtualReferenceError> {
    range
        .end
        .checked_sub(range.start)
        .filter(|length| {
            *length > 0
                && *length
                    <= (super::super::MAX_INTEGER_CHUNK_BYTES + 16 + super::super::MAX_INDEX_BYTES)
                        as u64
        })
        .ok_or_else(|| fetch_error(std::io::Error::other("Invalid HTTP virtual byte range")))
}

#[derive(Debug, Clone)]
struct LastModifiedPresent(bool);

#[derive(Debug, Default)]
pub(super) struct Attempts {
    started: AtomicU64,
    active: AtomicU64,
    finished: AtomicU64,
    body_bytes: AtomicU64,
}

#[derive(Debug)]
struct AttemptGuard(Arc<Attempts>);
impl AttemptGuard {
    fn start(counters: Arc<Attempts>) -> Self {
        counters.started.fetch_add(1, Ordering::Relaxed);
        counters.active.fetch_add(1, Ordering::Relaxed);
        Self(counters)
    }
}
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
        self.0.finished.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct AttemptBody {
    inner: HttpResponseBody,
    guard: Option<AttemptGuard>,
}
impl Body for AttemptBody {
    type Data = Bytes;
    type Error = HttpError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, HttpError>>> {
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(data), Some(guard)) = (frame.data_ref(), &self.guard) {
                    guard
                        .0
                        .body_bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                }
            }
            Poll::Ready(None | Some(Err(_))) => {
                self.guard.take();
            }
            Poll::Pending => {}
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Policy {
    Legacy,
    Provider(Provider),
}

#[derive(Debug, Clone, Copy)]
enum RequiredValidator {
    None,
    ETag,
    LastModified,
}

/// This state belongs to one logical range, never a shared client or origin.
#[derive(Debug, Clone)]
pub(super) struct FetchContext {
    pub policy: Policy,
    terminal: Arc<Mutex<ProviderFailure>>,
    required: RequiredValidator,
    destination: Option<Arc<(reqwest::Url, Range<u64>)>>,
}
impl FetchContext {
    pub(super) fn new(policy: Policy) -> Self {
        Self {
            policy,
            required: RequiredValidator::None,
            destination: None,
            terminal: Arc::new(Mutex::new(ProviderFailure::Generic)),
        }
    }
    fn set_failure(&self, failure: ProviderFailure) {
        *self.terminal.lock().unwrap() = failure;
    }
    pub(super) fn failure(&self) -> ProviderFailure {
        *self.terminal.lock().unwrap()
    }
}

#[derive(Debug, Clone)]
pub(super) struct BoundedRangeTransport {
    pub(super) client: reqwest::Client,
    pub(super) attempts: Arc<Attempts>,
}
impl BoundedRangeTransport {
    pub(super) fn new(provider: bool) -> Result<Self, reqwest::Error> {
        let builder = Self::builder(provider);
        Ok(Self {
            client: builder.build()?,
            attempts: Arc::default(),
        })
    }

    pub(super) fn builder(provider: bool) -> reqwest::ClientBuilder {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .user_agent(icechunk_types::user_agent())
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(20));
        if provider {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::ACCEPT_ENCODING,
                reqwest::header::HeaderValue::from_static("identity"),
            );
            builder = builder.no_proxy().default_headers(headers);
        }
        builder
    }

    pub(super) async fn fetch(
        &self,
        url: &reqwest::Url,
        range: &Range<u64>,
        checksum: Option<&Checksum>,
        mut context: FetchContext,
    ) -> Result<HttpVirtualChunkResponse, VirtualReferenceError> {
        let expected = expected_length(range)?;
        if matches!(context.policy, Policy::Provider(_)) {
            context.destination = Some(Arc::new((url.clone(), range.clone())));
            context.required = match checksum {
                Some(Checksum::ETag(_)) => RequiredValidator::ETag,
                Some(Checksum::LastModified(_)) => RequiredValidator::LastModified,
                None => RequiredValidator::None,
            };
        }
        let path = Path::from_url_path(url.path()).map_err(fetch_error)?;
        let client = HttpBuilder::new()
            .with_url(url.origin().ascii_serialization())
            .with_client_options(ClientOptions::new().with_allow_http(url.scheme() == "http"))
            .with_http_connector(Connector {
                transport: self.clone(),
                context: context.clone(),
            })
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
                let value = icechunk::storage::strip_quotes(&etag.0);
                options.if_match = Some(match context.policy {
                    Policy::Legacy => value.to_owned(),
                    Policy::Provider(_) => {
                        if value.is_empty()
                            || value.len() > 1024
                            || value.starts_with("W/")
                            || !value
                                .bytes()
                                .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b))
                        {
                            return Err(fetch_error(std::io::Error::other(
                                "Unsupported provider ETag",
                            )));
                        }
                        format!("\"{value}\"")
                    }
                });
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
        // Capacity comes from the authorized range, never a response header.
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

#[derive(Debug, Clone)]
struct Connector {
    transport: BoundedRangeTransport,
    context: FetchContext,
}
impl HttpConnector for Connector {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(self.clone()))
    }
}
#[async_trait::async_trait]
impl HttpService for Connector {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        // Reset even if request validation fails before a physical send.
        self.context.set_failure(ProviderFailure::Generic);
        let range = request
            .headers()
            .get("range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.split_once('-'))
            .and_then(|(first, last)| {
                Some((
                    first.parse::<u64>().ok()?,
                    last.parse::<u64>().ok()?.checked_add(1)?,
                ))
            })
            .filter(|(first, end)| first < end)
            .ok_or_else(|| invalid_response("Expected a bounded HTTP byte range"))?;
        // object_store may resume an ETag-bearing partial body at a later
        // start within the same logical range. The fixed end and original
        // destination remain authoritative; resumption shares the retry budget.
        if let Some(expected) = &self.context.destination
            && (request.uri() != expected.0.as_str()
                || request.method() != reqwest::Method::GET
                || range.0 < expected.1.start
                || range.1 != expected.1.end)
        {
            return Err(invalid_response(
                "Provider destination or range changed before send",
            ));
        }
        let guard = AttemptGuard::start(self.transport.attempts.clone());
        let response = HttpService::call(&self.transport.client, request).await?;
        let (parts, body) = response.into_parts();
        let mut response = HttpResponse::from_parts(
            parts,
            HttpResponseBody::new(AttemptBody {
                inner: body,
                guard: Some(guard),
            }),
        );
        if !response.status().is_success() {
            if let Policy::Provider(provider) = self.context.policy {
                if response.status().as_u16() == 412 {
                    self.context.set_failure(ProviderFailure::ObjectModified);
                    // The complete precondition-failed status is authoritative;
                    // ignore its body regardless of framing or availability.
                    *response.body_mut() = Vec::<u8>::new().into();
                    return Ok(response);
                }
                let body = std::mem::replace(response.body_mut(), Vec::<u8>::new().into());
                let mut stream = body.bytes_stream();
                let mut buffer = [0u8; 4096];
                let mut used = 0;
                let mut complete = true;
                while let Some(bytes) = stream.next().await {
                    // A failed error-body read must not restart a range fetch.
                    let bytes =
                        bytes.map_err(|_| invalid_response("Provider error body interrupted"))?;
                    if bytes.len() > buffer.len() - used {
                        complete = false;
                        break;
                    }
                    buffer[used..used + bytes.len()].copy_from_slice(&bytes);
                    used += bytes.len();
                }
                drop(stream);
                if provider == Provider::S3
                    && response.status().as_u16() == 503
                    && complete
                    && slow_down(&buffer[..used])
                {
                    self.context.set_failure(ProviderFailure::SlowDown);
                }
            } else {
                // Preserve legacy behavior: no provider body classification.
                *response.body_mut() = Vec::<u8>::new().into();
            }
            // object_store may otherwise collect unbounded error text. The
            // original network body/attempt ends before its retry/backoff.
            return Ok(response);
        }
        if response.status().as_u16() != 206 {
            return Err(invalid_response("Expected HTTP 206 for a byte range"));
        }
        let valid_range = response
            .headers()
            .get("content-range")
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
            .is_some_and(|(first, end, total)| (first, end) == range && end <= total);
        if !valid_range {
            return Err(invalid_response(
                "Content-Range differs from requested range",
            ));
        }
        let expected = range.1 - range.0;
        if let Some(length) = response.headers().get("content-length") {
            if length.to_str().ok().and_then(|v| v.parse::<u64>().ok()) != Some(expected) {
                return Err(invalid_response(
                    "Content-Length differs from requested range",
                ));
            }
        } else {
            response.headers_mut().insert(
                "content-length",
                expected.to_string().parse().map_err(invalid_response)?,
            );
        }
        if matches!(self.context.policy, Policy::Provider(_))
            && response
                .headers()
                .get("content-encoding")
                .is_some_and(|v| v.as_bytes() != b"identity")
        {
            return Err(invalid_response("Unsupported provider Content-Encoding"));
        }
        // header_meta parses these before the facade can perform postchecks.
        // Preserve the validator failure kind rather than losing it in erasure.
        let valid = match self.context.required {
            RequiredValidator::None => true,
            RequiredValidator::ETag => response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .is_some(),
            RequiredValidator::LastModified => response
                .headers()
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| chrono::DateTime::parse_from_rfc2822(v).is_ok()),
        };
        if !valid {
            self.context.set_failure(ProviderFailure::ObjectModified);
            return Err(invalid_response("Invalid required provider validator"));
        }
        let present = response.headers().contains_key("last-modified");
        response
            .extensions_mut()
            .insert(LastModifiedPresent(present));
        Ok(response)
    }
}

#[cfg(test)]
impl Attempts {
    pub(super) fn counts(&self) -> (u64, u64, u64, u64) {
        (
            self.started.load(Ordering::Relaxed),
            self.active.load(Ordering::Relaxed),
            self.finished.load(Ordering::Relaxed),
            self.body_bytes.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_range_request_clears_terminal_state_before_any_send() {
        let context = FetchContext::new(Policy::Provider(Provider::S3));
        context.set_failure(ProviderFailure::SlowDown);
        let transport = BoundedRangeTransport::new(true).unwrap();
        let connector = Connector {
            transport: transport.clone(),
            context: context.clone(),
        };
        let request = HttpRequest::new(Vec::<u8>::new().into());
        assert!(connector.call(request).await.is_err());
        assert_eq!(context.failure(), ProviderFailure::Generic);
        assert_eq!(transport.attempts.counts(), (0, 0, 0, 0));
    }
    #[tokio::test]
    async fn provider_destination_and_range_are_rechecked_before_every_send() {
        let transport = BoundedRangeTransport::new(true).unwrap();
        for (uri, span) in [
            ("https://example.invalid/other", "bytes=7-10"),
            ("https://other.invalid/file", "bytes=7-10"),
            ("http://example.invalid/file", "bytes=7-10"),
            ("https://example.invalid/file?q=x", "bytes=7-10"),
            ("https://example.invalid/file", "bytes=6-10"),
            ("https://example.invalid/file", "bytes=7-11"),
        ] {
            let mut context = FetchContext::new(Policy::Provider(Provider::S3));
            context.destination = Some(Arc::new((
                reqwest::Url::parse("https://example.invalid/file").unwrap(),
                7..11,
            )));
            context.set_failure(ProviderFailure::SlowDown);
            let connector = Connector {
                transport: transport.clone(),
                context: context.clone(),
            };
            let mut request = HttpRequest::new(Vec::<u8>::new().into());
            *request.uri_mut() = uri.parse().unwrap();
            request.headers_mut().insert("range", span.parse().unwrap());
            assert!(connector.call(request).await.is_err());
            assert_eq!(context.failure(), ProviderFailure::Generic);
            assert_eq!(transport.attempts.counts(), (0, 0, 0, 0));
        }
    }
}
