//! Native store-to-query adapter. No HTTP, catalog lookup or dataset cache.
use std::{collections::BTreeMap, sync::Arc};

use dynapi_contract::Value;
use dynapi_query::{
    Coordinate, DataReader, DatasetView, Positions, QueryError, Selection, SpatialCrs, Variable,
};
pub use dynapi_reader::{
    DatasetVersion, HttpVirtualChunkContainer, PreparedSources, StoreDescriptor,
    VirtualSourceDeclaration, prepare_sources,
};
use dynapi_reader::{ErrorKind, ReadSession, ReaderError, Selector, TypedBuffer, cf_time};
use serde_json::Value as Json;
use tokio::runtime::Handle;

mod epoch;
mod metadata;
use metadata::{Budget, attributes, cf_request, coordinate, variable};

/// Maximum charged metadata owned by one opened dataset (see README accounting).
pub const MAX_METADATA_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum OpenError {
    UnknownSnapshot(String),
    Reader(ReaderError),
    Metadata(String),
    MetadataLimit { limit: usize },
}
impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSnapshot(id) => write!(
                f,
                "Unknown snapshot {}",
                dynapi_contract::output::python_repr(id)
            ),
            Self::Reader(error) => f.write_str(&error.message),
            Self::Metadata(message) => f.write_str(message),
            Self::MetadataLimit { limit } => {
                write!(f, "Opened dataset metadata exceeds {limit} bytes")
            }
        }
    }
}
impl std::error::Error for OpenError {}
impl From<ReaderError> for OpenError {
    fn from(error: ReaderError) -> Self {
        Self::Reader(error)
    }
}

/// An immutable view and its one pinned session. Share this Arc, not descriptors,
/// between requests. The caller owns cache policy and lifetime.
pub struct OpenedDataset {
    view: DatasetView,
    session: ReadSession,
    /// Serialized source metadata is retained for CF decoding and read validation.
    arrays: BTreeMap<String, Json>,
    metadata_bytes: usize,
}
impl OpenedDataset {
    pub async fn open(
        descriptor: &StoreDescriptor,
        version: DatasetVersion<'_>,
        containers: &[HttpVirtualChunkContainer],
    ) -> Result<Arc<Self>, OpenError> {
        Self::open_bounded(descriptor, version, containers, MAX_METADATA_BYTES).await
    }
    pub async fn open_bounded(
        descriptor: &StoreDescriptor,
        version: DatasetVersion<'_>,
        containers: &[HttpVirtualChunkContainer],
        limit: usize,
    ) -> Result<Arc<Self>, OpenError> {
        let session = ReadSession::open(descriptor, version, containers)
            .await
            .map_err(|error| {
                if let DatasetVersion::Snapshot(id) = version
                    && error.kind == ErrorKind::Snapshot
                    && error.message.starts_with("Unknown snapshot ")
                {
                    return OpenError::UnknownSnapshot(id.into());
                }
                OpenError::Reader(error)
            })?;
        Self::from_session_bounded(session, limit).await
    }
    pub async fn open_prepared(
        prepared: &PreparedSources,
        version: DatasetVersion<'_>,
    ) -> Result<Arc<Self>, OpenError> {
        let session = ReadSession::open_prepared(prepared, version)
            .await
            .map_err(|error| {
                if let DatasetVersion::Snapshot(id) = version
                    && error.kind == ErrorKind::Snapshot
                    && error.message.starts_with("Unknown snapshot ")
                {
                    OpenError::UnknownSnapshot(id.into())
                } else {
                    OpenError::Reader(error)
                }
            })?;
        Self::from_session_bounded(session, MAX_METADATA_BYTES).await
    }
    pub async fn open_with_sources(
        descriptor: &StoreDescriptor,
        version: DatasetVersion<'_>,
        sources: &[VirtualSourceDeclaration],
    ) -> Result<Arc<Self>, OpenError> {
        let session = ReadSession::open_with_sources(descriptor, version, sources)
            .await
            .map_err(|error| {
                if let DatasetVersion::Snapshot(id) = version
                    && error.kind == ErrorKind::Snapshot
                    && error.message.starts_with("Unknown snapshot ")
                {
                    OpenError::UnknownSnapshot(id.into())
                } else {
                    OpenError::Reader(error)
                }
            })?;
        Self::from_session_bounded(session, MAX_METADATA_BYTES).await
    }
    /// Complete an already pinned opaque session without reopening it.
    pub async fn from_read_session(session: ReadSession) -> Result<Arc<Self>, OpenError> {
        Self::from_session_bounded(session, MAX_METADATA_BYTES).await
    }
    async fn from_session_bounded(
        session: ReadSession,
        limit: usize,
    ) -> Result<Arc<Self>, OpenError> {
        let policy_bytes = session.retained_source_policy_bytes();
        let remaining = limit
            .checked_sub(policy_bytes)
            .ok_or(OpenError::MetadataLimit { limit })?;
        let raw = session.root_metadata(remaining).await.map_err(|error| {
            if error.kind == ErrorKind::SizeLimit {
                OpenError::MetadataLimit { limit }
            } else {
                error.into()
            }
        })?;
        let mut budget = Budget::new(limit);
        budget.add(std::mem::size_of::<Self>() + session.snapshot_id().len())?;
        budget.add(policy_bytes)?;
        // Charge both serialized staging and parsed JSON before coordinate reads.
        for (name, bytes) in &raw {
            budget.add(name.len() + bytes.len())?;
        }
        let mut arrays = BTreeMap::new();
        let mut root = Json::Null;
        for (name, bytes) in raw {
            let meta = budget.parse(&bytes)?;
            budget.add(name.len() + 64)?;
            if name.is_empty() {
                root = meta;
            } else if meta["node_type"] == "array" {
                arrays.insert(name, meta);
            }
        }
        if root["node_type"] != "group" {
            return Err(OpenError::Metadata(
                "Dataset root is not a Zarr group".into(),
            ));
        }
        let mut view = DatasetView {
            attributes: attributes(&root["attributes"], false, false)?,
            ..DatasetView::default()
        };
        budget.attrs(&view.attributes)?;
        let mut coordinate_names = std::collections::BTreeSet::new();
        if let Some(names) = root["attributes"]["coordinates"].as_str() {
            for name in names.split_whitespace() {
                budget.add(name.len() + 96)?;
                coordinate_names.insert(name.to_owned());
            }
        }
        for (name, meta) in &arrays {
            let var = variable(meta)?;
            let shape = meta["shape"]
                .as_array()
                .ok_or_else(|| OpenError::Metadata("Missing array shape".into()))?;
            for (dim, length) in var.dimensions.iter().zip(shape) {
                let length = length
                    .as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| OpenError::Metadata("Invalid dimension length".into()))?;
                if let Some(previous) = view.dimensions.insert(dim.clone(), length) {
                    if previous != length {
                        return Err(OpenError::Metadata(format!(
                            "Conflicting sizes for dimension {dim:?}"
                        )));
                    }
                } else {
                    budget.add(dim.len() + std::mem::size_of::<(String, usize)>() + 64)?;
                }
            }
            if var.dimensions.as_slice() == [name.as_str()] {
                budget.add(name.len() + 96)?;
                coordinate_names.insert(name.clone());
            }
            if let Some(names) = meta["attributes"]["coordinates"].as_str() {
                // decode_cf_variables promotes existing references at dataset scope.
                for coord in names.split_whitespace() {
                    if arrays.contains_key(coord) {
                        budget.add(coord.len() + 96)?;
                        coordinate_names.insert(coord.into());
                    }
                }
            }
            budget.variable(name, &var)?;
            view.variables.insert(name.clone(), var);
        }
        for name in &coordinate_names {
            if let Some(var) = view.variables.get_mut(name) {
                var.is_coordinate = true;
            }
        }
        view.spatial_crs = if let Some(spatial_ref) = arrays
            .get("spatial_ref")
            .filter(|_| coordinate_names.contains("spatial_ref"))
        {
            let attrs = &spatial_ref["attributes"];
            let wkt = attrs
                .get("crs_wkt")
                .filter(|v| metadata::truthy(v))
                .or_else(|| attrs.get("spatial_ref"));
            Some(match wkt.filter(|v| metadata::truthy(v)) {
                Some(Json::String(wkt)) => classify_wkt(wkt),
                Some(_) => SpatialCrs::Projected,
                None => SpatialCrs::Missing,
            })
        } else {
            None
        };
        if matches!(view.spatial_crs, Some(SpatialCrs::Attested(_)))
            && view.dimensions.contains_key("x")
            && view.dimensions.contains_key("y")
            && !(view.dimensions.contains_key("latitude")
                && view.dimensions.contains_key("longitude"))
        {
            for name in ["x", "y"] {
                if arrays.get(name).and_then(|meta| meta["data_type"].as_str()) != Some("float64") {
                    return Err(OpenError::Metadata(format!(
                        "Projected coordinate {name} must have float64 storage"
                    )));
                }
            }
        }
        // Query needs eager dimension coordinates only. Auxiliary coordinates
        // (notably GFS valid_time) remain lazy just as xr.open_zarr(chunks=None).
        for name in &coordinate_names {
            let Some(meta) = arrays.get(name) else {
                continue;
            };
            let dims = &view.variables[name].dimensions;
            if dims.as_slice() != [name.as_str()] {
                continue;
            }
            let size = view.dimensions[name];
            if !query_coordinate(name) {
                continue; // Unselected axes (including REFS statistic) stay lazy.
            }
            let width = match meta["data_type"].as_str() {
                Some("int8" | "uint8") => 1,
                Some("int16" | "uint16") => 2,
                Some("float32" | "int32" | "uint32") => 4,
                Some("float64" | "int64") => 8,
                _ => {
                    return Err(OpenError::Metadata(format!(
                        "Unsupported coordinate dtype for {name}"
                    )));
                }
            };
            // Preflight the reader's full-axis selector/chunk work without allocating
            // selectors or groups. Reserve both Prepared copies, geometric Vec spare
            // capacity, CF staging and retained coordinate storage before the read.
            let groups = dynapi_reader::coordinate_chunk_count(meta, name)?;
            let per_value = width * 4
                + 128
                + if name == "ensemble_member" {
                    std::mem::size_of::<Value>() + 64
                } else {
                    0
                };
            let full_charge = size
                .checked_mul(per_value)
                .and_then(|n| n.checked_add(groups.checked_mul(512)?))
                .and_then(|n| n.checked_add(name.len() + 64))
                .ok_or(OpenError::MetadataLimit { limit })?;
            let cf = cf_request(meta)?;
            // Select a complete plan before charging or allocating any selectors.
            // Duration dimensions require full-axis CF context and stay on the old path.
            let batched = !budget.fits(full_charge)
                && name != "ensemble_member"
                && epoch::eligible(meta, cf.as_ref());
            let charge = if batched {
                epoch::charge(size, groups, name.len()).ok_or(OpenError::MetadataLimit { limit })?
            } else {
                full_charge
            };
            budget.add(charge)?;
            i64::try_from(size.saturating_sub(1))
                .map_err(|_| OpenError::MetadataLimit { limit })?;
            let coord = if batched {
                let cf = cf.as_ref().expect("epoch plan requires CF encoding");
                let session = &session;
                epoch::read(size, |selectors| async move {
                    session
                        .read_dataset_selection(name, &selectors, Some(cf))
                        .await
                })
                .await?
            } else {
                let selectors = vec![Selector::Indices((0..size).map(|i| i as i64).collect())];
                let buffer = session
                    .read_dataset_selection(name, &selectors, cf.as_ref())
                    .await?;
                coordinate(buffer, name == "ensemble_member")?
            };
            view.coordinates.insert(name.clone(), coord);
        }
        // xarray dataset[dimension] supplies a virtual integer index only when
        // there is no stored same-name variable. Keep stored invalid axes distinct.
        for (name, &size) in &view.dimensions {
            if !arrays.contains_key(name) && query_coordinate(name) {
                let member_bytes = if name == "ensemble_member" {
                    std::mem::size_of::<Value>() + 64
                } else {
                    0
                };
                budget.add(
                    size.checked_mul(8 + member_bytes)
                        .and_then(|n| n.checked_add(name.len() + 96))
                        .ok_or(OpenError::MetadataLimit { limit })?,
                )?;
                i64::try_from(size.saturating_sub(1))
                    .map_err(|_| OpenError::MetadataLimit { limit })?;
                // Keep the iterator exact-sized: fallible collection grows the
                // Vec geometrically, retaining more slots than the charge.
                let values: Vec<_> = (0..size).map(|i| i as i64).collect();
                let coord = if name == "ensemble_member" {
                    Coordinate::Members(values.into_iter().map(Value::from).collect())
                } else {
                    Coordinate::Int64(values)
                };
                view.coordinates.insert(name.clone(), coord);
            }
        }
        Ok(Arc::new(Self {
            view,
            session,
            arrays,
            metadata_bytes: budget.used(),
        }))
    }
    pub fn view(&self) -> &DatasetView {
        &self.view
    }
    pub fn snapshot_id(&self) -> &str {
        self.session.snapshot_id()
    }
    /// Conservative charge, including bounded open staging, not allocator RSS.
    pub fn metadata_bytes(&self) -> usize {
        self.metadata_bytes
    }
    pub fn reader(&self, handle: Handle) -> DatasetReader<'_> {
        DatasetReader {
            dataset: self,
            handle,
        }
    }
}

/// WKT classification is deliberately fail-closed; see deviation D12.
/// Attested operations take precedence over geographic identity.
pub fn classify_wkt(wkt: &str) -> SpatialCrs {
    if let Ok(projection) = dynapi_projection::Projection::from_wkt(wkt) {
        SpatialCrs::Attested(projection.kind())
    } else if metadata::GEOGRAPHIC_WKTS.contains(&wkt) {
        SpatialCrs::Geographic
    } else {
        SpatialCrs::Projected
    }
}

pub struct DatasetReader<'a> {
    dataset: &'a OpenedDataset,
    handle: Handle,
}
impl DataReader for DatasetReader<'_> {
    fn read(
        &mut self,
        view: &DatasetView,
        variable: &str,
        positions: &Positions,
    ) -> Result<Vec<Value>, QueryError> {
        if !std::ptr::eq(view, self.dataset.view()) {
            return Err(QueryError::internal(
                "Reader used with a different dataset view",
            ));
        }
        let meta = self
            .dataset
            .arrays
            .get(variable)
            .ok_or_else(|| QueryError::internal(format!("Array {variable:?} missing")))?;
        let var = &view.variables[variable];
        var.dimensions.iter().try_fold(0usize, |count, dim| {
            let size = match positions.get(dim) {
                Some(Selection::Index(_)) => 1,
                Some(Selection::Indices(xs)) => xs.len(),
                None => view.dimensions[dim],
            };
            count
                .checked_add(size)
                .filter(|n| *n <= dynapi_reader::MAX_SELECTORS)
                .ok_or_else(|| QueryError::internal("Too many selector indices"))
        })?;
        let mut selectors = Vec::with_capacity(var.dimensions.len());
        for dim in &var.dimensions {
            let index = |i: usize| {
                i64::try_from(i).map_err(|_| QueryError::internal("Selector exceeds int64"))
            };
            selectors.push(match positions.get(dim) {
                Some(Selection::Index(i)) => Selector::Index(index(*i)?),
                Some(Selection::Indices(xs)) => {
                    let mut indices = Vec::with_capacity(xs.len());
                    for &i in xs {
                        indices.push(index(i)?);
                    }
                    Selector::Indices(indices)
                }
                None => {
                    let size = view.dimensions[dim];
                    index(size.saturating_sub(1))?;
                    Selector::Indices((0..size).map(|i| i as i64).collect())
                }
            });
        }
        let cf = cf_request(meta).map_err(|e| QueryError::internal(e.to_string()))?;
        let buffer = self
            .handle
            .block_on(self.dataset.session.read_dataset_selection(
                variable,
                &selectors,
                cf.as_ref(),
            ))
            .map_err(map_reader_error)?;
        metadata::values(buffer).map_err(|e| QueryError::internal(e.to_string()))
    }
}

pub fn map_reader_error(error: ReaderError) -> QueryError {
    if let Some(kind) = error.virtual_failure {
        return match kind {
            dynapi_reader::VirtualFailureKind::SlowDown
            | dynapi_reader::VirtualFailureKind::OperationTimeout => {
                QueryError::upstream_unavailable()
            }
            dynapi_reader::VirtualFailureKind::PreparationUnavailable => QueryError {
                status: 503,
                detail: "Server busy; retry after 5 seconds".into(),
                retry_after: Some(5),
                exception_class: None,
            },
            dynapi_reader::VirtualFailureKind::Generic
            | dynapi_reader::VirtualFailureKind::ObjectModified => QueryError {
                exception_class: Some("StorageError"),
                ..QueryError::internal(error.message)
            },
        };
    }
    if error.kind == ErrorKind::Storage {
        let mut query = QueryError::from_storage_failure(error.virtual_chunk_fetch, error.message);
        if query.status == 500 {
            query.exception_class = error.exception_class.or(query.exception_class);
        }
        query
    } else if error.kind == ErrorKind::InvalidSelection {
        QueryError {
            exception_class: Some("IndexError"),
            ..QueryError::internal(error.message)
        }
    } else {
        QueryError::internal(error.message)
    }
}

/// Run one admitted compute job on a dedicated OS thread. The caller's owned
/// permit bounds active threads by compute slots and stays held through cancellation.
/// Reader futures may use Tokio's blocking pool without competing with compute.
pub async fn run_with_permit<T, F, P>(permit: P, job: F) -> Result<T, QueryError>
where
    T: Send + 'static,
    F: FnOnce(Handle) -> Result<T, QueryError> + Send + 'static,
    P: Send + 'static,
{
    let handle = Handle::current();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("dataset-compute".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _permit = permit;
                job(handle)
            }))
            .unwrap_or_else(|_| Err(QueryError::internal("Dataset worker failed: panic")));
            let _ = sender.send(result);
        })
        .map_err(|e| QueryError::internal(format!("Dataset worker failed: {e}")))?;
    receiver
        .await
        .map_err(|e| QueryError::internal(format!("Dataset worker failed: {e}")))?
}

fn query_coordinate(name: &str) -> bool {
    matches!(
        name,
        "latitude"
            | "longitude"
            | "x"
            | "y"
            | "time"
            | "init_time"
            | "lead_time"
            | "ensemble_member"
    )
}
