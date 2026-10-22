//! Portable STAC parsing and dataset-cache policy. Store I/O is supplied by callers.
//! Disable default features for the wasm-checkable core. `native` adds the async
//! refresh service; `reader` adds conversion to the existing reader's input types.
pub mod cache;
mod collection;
mod document;
mod httpx_url;
#[cfg(feature = "native")]
pub mod native;
mod pyurl;
pub mod refresh;
pub use collection::*;
pub use dynapi_policy::Clock;
use std::fmt;
pub const DEFAULT_STAC_URL: &str = "https://stac.dynamical.org/catalog.json";
pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
/// Public text and the Python exception class used to sanitize refresh failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogError {
    pub message: String,
    pub exception_type: &'static str,
}
impl CatalogError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::typed(message, "CatalogError")
    }
    pub fn typed(message: impl Into<String>, exception_type: &'static str) -> Self {
        Self {
            message: message.into(),
            exception_type,
        }
    }
}
impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for CatalogError {}
/// Python `json.loads(bytes)` acceptance, including BOM/UTF-16/UTF-32 detection,
/// NaN/Infinity and arbitrary integers. Fails with ValueError when a field the
/// catalog reads holds a number serde_json cannot represent (DEVIATIONS.md D11).
pub fn parse_document(body: &[u8]) -> Result<serde_json::Value, CatalogError> {
    match document::parse(body)? {
        (_, true) => Err(CatalogError::typed(
            document::UNSUPPORTED_NUMBER,
            "ValueError",
        )),
        (value, false) => Ok(value),
    }
}
/// A child document: the outer error aborts refresh (as a fetch/JSON failure
/// does in Python); the inner error skips only this collection.
pub fn parse_collection_document(
    body: &[u8],
) -> Result<Result<Collection, CatalogError>, CatalogError> {
    Ok(match document::parse(body)? {
        (_, true) => Err(CatalogError::typed(
            document::UNSUPPORTED_NUMBER,
            "ValueError",
        )),
        (value, false) => Collection::from_stac(&value),
    })
}
