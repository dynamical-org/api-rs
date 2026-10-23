//! Session-bound resolution; no caller can fabricate a destination or range.
use super::*;
use crate::http::provider::{Provider, ProviderTransport};
use icechunk::{
    format::{
        ChunkIndices,
        manifest::{Checksum, ChunkPayload},
    },
    session::Session,
};
use std::{num::NonZeroU64, ops::Range};
use tokio::sync::RwLock;

pub(crate) struct SessionState {
    pub(crate) session: Arc<RwLock<Session>>,
    snapshot_id: Arc<str>,
    pub(crate) policy: Arc<SessionSourcePolicy>,
    transport: ProviderTransport,
}
impl SessionState {
    pub(super) fn new(
        session: Arc<RwLock<Session>>,
        snapshot: String,
        policy: Arc<SessionSourcePolicy>,
    ) -> Result<Self, ReaderError> {
        Ok(Self {
            session,
            snapshot_id: snapshot.into(),
            policy,
            transport: ProviderTransport::new(Provider::S3)
                .map_err(|e| err(ErrorKind::Storage, e))?,
        })
    }
}
pub(crate) struct AttestedArray {
    path: Arc<str>,
    shape: Vec<u64>,
    chunks: Vec<u64>,
    profile: [u8; 32],
}
impl AttestedArray {
    pub(crate) fn new(
        path: &str,
        cap: &crate::Capability,
        metadata: &serde_json::Value,
        raw: &[u8],
    ) -> Result<Self, ReaderError> {
        let chunks = crate::dimensions(&metadata["chunk_grid"]["configuration"]["chunk_shape"])?;
        if chunks.len() != cap.shape.len() || chunks.contains(&0) {
            return Err(invalid());
        }
        Ok(Self {
            path: path.into(),
            shape: cap.shape.clone(),
            chunks,
            profile: Sha256::digest(raw).into(),
        })
    }
    fn indices(&self, key: &str) -> Result<ChunkIndices, ReaderError> {
        let suffix = key
            .strip_prefix(self.path.as_ref())
            .and_then(|s| s.strip_prefix("/c"))
            .ok_or_else(invalid)?;
        let mut indices = Vec::with_capacity(self.shape.len());
        if !self.shape.is_empty() {
            let suffix = suffix.strip_prefix('/').ok_or_else(invalid)?;
            for ((raw, size), chunk) in suffix.split('/').zip(&self.shape).zip(&self.chunks) {
                if raw.is_empty()
                    || (raw.len() > 1 && raw.starts_with('0'))
                    || !raw.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(invalid());
                }
                let index = raw.parse::<u32>().map_err(|_| invalid())?;
                if u64::from(index) >= size.div_ceil(*chunk) {
                    return Err(invalid());
                }
                indices.push(index);
            }
            if suffix.split('/').count() != self.shape.len() {
                return Err(invalid());
            }
        } else if !suffix.is_empty() {
            return Err(invalid());
        }
        Ok(ChunkIndices(indices))
    }
}
struct UnitIdentity {
    snapshot_id: Arc<str>,
    array_path: Arc<str>,
    indices: ChunkIndices,
    metadata_profile_id: [u8; 32],
}
pub(super) struct AuthorizedSource {
    original: Box<str>,
    pub(super) declared_prefix_index: usize,
    route: Route,
}
pub(crate) struct ResolvedVirtualRef {
    owner: Arc<SessionState>,
    unit: UnitIdentity,
    source: AuthorizedSource,
    offset: u64,
    length: NonZeroU64,
    checksum: Option<Checksum>,
    policy_id: [u8; 32],
}
pub(crate) struct PhysicalRange {
    reference: Arc<ResolvedVirtualRef>,
    logical: Range<u64>,
    physical: Range<u64>,
    url: reqwest::Url,
}
pub(crate) enum ResolvedPayload {
    Missing,
    Existing(u64),
    LegacyHttp(u64),
    Virtual(Arc<ResolvedVirtualRef>),
}
impl ResolvedPayload {
    pub(crate) fn length(&self) -> Option<u64> {
        match self {
            Self::Missing => None,
            Self::Existing(n) | Self::LegacyHttp(n) => Some(*n),
            Self::Virtual(v) => Some(v.length.get()),
        }
    }
    pub(crate) fn provider(&self) -> bool {
        matches!(self,Self::Virtual(v) if v.source.route!=Route::Http)
    }
}
impl SessionSourcePolicy {
    pub(super) fn authorize(&self, uri: &str) -> Result<AuthorizedSource, ReaderError> {
        if uri.len() > 4096 {
            return Err(invalid());
        }
        if uri.starts_with("s3://") || uri.starts_with("gs://") || uri.starts_with("gcs://") {
            original(uri, false)?;
        } else {
            let url = reqwest::Url::parse(uri).map_err(|_| invalid())?;
            if !["https", "http"].contains(&url.scheme())
                || url.as_str() != uri
                || url.query().is_some()
                || url.fragment().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(invalid());
            }
        }
        let (index, rule) = self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, r)| uri.starts_with(r.declaration.prefix()))
            .max_by_key(|(_, r)| r.declaration.prefix().len())
            .ok_or_else(invalid)?;
        Ok(AuthorizedSource {
            original: uri.into(),
            declared_prefix_index: index,
            route: rule.route.clone(),
        })
    }
}
fn encoded_length(payload: &ChunkPayload) -> Result<NonZeroU64, ReaderError> {
    NonZeroU64::new(payload.length()).ok_or_else(|| err(ErrorKind::Decode, "Empty encoded chunk"))
}
pub(crate) async fn resolve(
    session: Arc<RwLock<Session>>,
    owner: Option<&Arc<SessionState>>,
    array: &AttestedArray,
    key: &str,
) -> Result<ResolvedPayload, ReaderError> {
    if owner.is_some_and(|owner| !Arc::ptr_eq(&owner.session, &session)) {
        return Err(invalid());
    }
    let indices = array.indices(key)?;
    let path =
        icechunk::format::Path::try_from(format!("/{}", array.path)).map_err(|_| invalid())?;
    let session = session.read().await;
    let Some(payload) = session
        .get_chunk_ref(&path, &indices)
        .await
        .map_err(|e| err(ErrorKind::Storage, e))?
    else {
        return Ok(ResolvedPayload::Missing);
    };
    let length = encoded_length(&payload)?;
    match payload {
        ChunkPayload::Virtual(reference) => {
            reference
                .offset
                .checked_add(length.get())
                .ok_or_else(invalid)?;
            if let Some(owner) = owner {
                if reference.location.url().len() > 4096 {
                    return Err(invalid());
                }
                if let Some(relative) = reference.location.url().strip_prefix("vcc://") {
                    let (name, key) = relative.split_once('/').ok_or_else(invalid)?;
                    let rule = owner
                        .policy
                        .rules
                        .iter()
                        .find(|r| r.name.as_deref() == Some(name))
                        .ok_or_else(invalid)?;
                    rule.declaration
                        .prefix()
                        .len()
                        .checked_add(key.len())
                        .filter(|n| *n <= 4096)
                        .ok_or_else(invalid)?;
                }
                let uri = session
                    .resolve_virtual_location(&reference.location)
                    .map_err(|_| invalid())?;
                let source = owner.policy.authorize(&uri)?;
                if source.route != Route::Http
                    && (length.get() > 8 * 1024 * 1024
                        || matches!(&reference.checksum,Some(Checksum::ETag(tag)) if tag.0.len()>1024))
                {
                    return Err(invalid());
                }
                let unit = UnitIdentity {
                    snapshot_id: owner.snapshot_id.clone(),
                    array_path: array.path.clone(),
                    indices,
                    metadata_profile_id: array.profile,
                };
                Ok(ResolvedPayload::Virtual(Arc::new(ResolvedVirtualRef {
                    owner: owner.clone(),
                    unit,
                    source,
                    offset: reference.offset,
                    length,
                    checksum: reference.checksum,
                    policy_id: owner.policy.id,
                })))
            } else {
                let uri = session
                    .resolve_virtual_location(&reference.location)
                    .map_err(|e| err(ErrorKind::Storage, e))?;
                if !uri.starts_with("https://") && !uri.starts_with("http://") {
                    return Err(unsupported(
                        "Provider virtual payload requires explicit source preparation",
                    ));
                }
                Ok(ResolvedPayload::LegacyHttp(length.get()))
            }
        }
        ChunkPayload::Ref(_) | ChunkPayload::Inline(_) => {
            Ok(ResolvedPayload::Existing(length.get()))
        }
        _ => Err(invalid()),
    }
}
impl ResolvedVirtualRef {
    fn map(self: Arc<Self>, logical: Range<u64>) -> Result<PhysicalRange, ReaderError> {
        if logical.start >= logical.end || logical.end > self.length.get() {
            return Err(invalid());
        }
        let physical = self.offset.checked_add(logical.start).ok_or_else(invalid)?
            ..self.offset.checked_add(logical.end).ok_or_else(invalid)?;
        let (scheme, bucket, key) = original(&self.source.original, false)?;
        let destination = match &self.source.route {
            Route::S3(region) if scheme == "s3" => {
                format!("https://{bucket}.s3.{region}.amazonaws.com/{key}")
            }
            Route::Gcs if ["gs", "gcs"].contains(&scheme) => {
                format!("https://storage.googleapis.com/{bucket}/{key}")
            }
            _ => return Err(invalid()),
        };
        let url = reqwest::Url::parse(&destination).map_err(|_| invalid())?;
        if url.as_str() != destination
            || url.scheme() != "https"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        Ok(PhysicalRange {
            reference: self,
            logical,
            physical,
            url,
        })
    }
}
impl PhysicalRange {
    async fn fetch(
        &self,
    ) -> Result<icechunk::virtual_chunks::HttpVirtualChunkResponse, ReaderError> {
        // Descriptor and policy stay alive through the body and later decode.
        let provider = match self.reference.source.route {
            Route::S3(_) => Provider::S3,
            Route::Gcs => Provider::Gcs,
            Route::Http => return Err(invalid()),
        };
        self.reference
            .owner
            .transport
            .for_provider(provider)
            .fetch(
                self.url.as_str(),
                &self.physical,
                self.reference.checksum.as_ref(),
            )
            .await
            .map_err(|kind| {
                let mut error = err(ErrorKind::Storage, kind);
                error.virtual_chunk_fetch = true;
                error.virtual_failure = Some(kind.into());
                error
            })
    }
}
#[cfg(test)]
mod tests;
