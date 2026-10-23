//! Runtime-only virtual transport. The legacy adapter leaves authorization and
//! checksum postconditions in Icechunk. Opt-in provider policy is not yet wired.
use icechunk::{
    config::HttpConfig,
    format::manifest::{Checksum, VirtualReferenceError, VirtualReferenceErrorKind},
    virtual_chunks::{HttpVirtualChunkFetcher, HttpVirtualChunkResponse},
};
use icechunk_arrow_object_store::object_store::{
    client::{HttpError, HttpErrorKind},
    path::Path,
};
use std::{ops::Range, sync::Arc};

pub(crate) mod provider;
mod transport;

#[derive(Debug, Clone)]
pub(crate) struct BoundedHttpFetcher {
    transport: transport::BoundedRangeTransport,
    prefixes: Arc<[String]>,
}
impl BoundedHttpFetcher {
    pub(crate) fn new(prefixes: Vec<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            transport: transport::BoundedRangeTransport::new(false)?,
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
#[async_trait::async_trait]
impl HttpVirtualChunkFetcher for BoundedHttpFetcher {
    async fn fetch(
        &self,
        url: &str,
        range: &Range<u64>,
        checksum: Option<&Checksum>,
        _config: &HttpConfig,
    ) -> Result<HttpVirtualChunkResponse, VirtualReferenceError> {
        transport::expected_length(range)?;
        let url = reqwest::Url::parse(url).map_err(fetch_error)?;
        let path = Path::from_url_path(url.path()).map_err(fetch_error)?;
        // Authorize the exact decoded/re-encoded URL object_store will request.
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
        self.transport
            .fetch(
                &url,
                range,
                checksum,
                transport::FetchContext::new(transport::Policy::Legacy),
            )
            .await
    }
}
