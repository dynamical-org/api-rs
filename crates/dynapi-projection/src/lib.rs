//! Exact-catalog spherical coordinate operations and native grid selection.
//!
//! This library does not classify geographic CRSs or wire service queries.
//! HRDPS is a derived geographic CRS whose axes require a rotated operation.
//! Projected selection is unsupported on WASM; compiling the math is not a
//! qualification to serve grid cells there. See deviations D-proj-1/2.

mod axis;
mod transform;
mod wkt;
pub use wkt::{HRDPS_WKT, HRRR_WKT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    UnsupportedCrs,
    UnsupportedTarget,
    InvalidCoordinate,
    OutsideProjectionDomain,
    EmptyAxis,
    NonFiniteAxis,
    NonMonotonicAxis,
    InvalidAxisExtent,
    OutsideGrid,
    IndexOutOfBounds,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnsupportedCrs => "unsupported coordinate reference system",
            Self::UnsupportedTarget => "projected grid selection is unsupported on WASM",
            Self::InvalidCoordinate => "invalid coordinate",
            Self::OutsideProjectionDomain => "coordinate is outside the projection domain",
            Self::EmptyAxis => "grid axis is empty",
            Self::NonFiniteAxis => "grid axis contains nonfinite coordinates",
            Self::NonMonotonicAxis => "grid axis is not monotonic",
            Self::InvalidAxisExtent => "grid axis has an invalid extent",
            Self::OutsideGrid => "location is outside the data product grid",
            Self::IndexOutOfBounds => "grid index is out of bounds",
        })
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionKind {
    HrrrLambertConformal,
    HrdpsRotatedPole,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeographicPoint {
    /// Input degrees east, in [-180, 180]. Inverse output follows PROJ's
    /// longitude seam rule and may exceed either endpoint by <1e-12 radians.
    pub longitude: f64,
    /// Degrees north, in [-90, 90].
    pub latitude: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridPoint {
    /// Easting in metres (HRRR), or rotated longitude in degrees (HRDPS).
    pub x: f64,
    /// Northing in metres (HRRR), or rotated latitude in degrees (HRDPS).
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridIndices {
    pub x: usize,
    pub y: usize,
}

impl GridIndices {
    /// Canonical request/indexer order is y, x, although Cartesian points are x, y.
    pub fn canonical_y_x(self) -> [usize; 2] {
        [self.y, self.x]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectedPoint {
    pub indices: GridIndices,
    /// Original stored axis values, including their signed zeros.
    pub grid: GridPoint,
    /// Inverse transformation of the selected stored grid values.
    pub geographic: GeographicPoint,
}

#[derive(Clone, Copy, Debug)]
pub struct Projection {
    operation: transform::Operation,
}

impl Projection {
    /// Accept only the two exact catalog inputs. Unknown or reformatted WKT
    /// cannot fall back to identity or supply arbitrary projection parameters.
    pub fn from_wkt(wkt: &str) -> Result<Self, Error> {
        Ok(Self {
            operation: transform::Operation::from_wkt(wkt)?,
        })
    }

    pub fn kind(self) -> ProjectionKind {
        self.operation.kind()
    }

    /// Spherical operation only: no geocentric datum shift, height or epoch.
    pub fn forward(self, point: GeographicPoint) -> Result<GridPoint, Error> {
        self.operation.forward(point)
    }

    /// Inverse longitude preserves PROJ's <1e-12-radian seam overshoot.
    /// Forward request validation still requires longitude in [-180, 180].
    pub fn inverse(self, point: GridPoint) -> Result<GeographicPoint, Error> {
        self.operation.inverse(point)
    }
}

/// Borrowed, validated x/y axes. Units and coordinate membership must be
/// checked by the adapter; WKT alone does not attest the arrays' metadata.
/// Axes must be finite and monotonic in either direction. Duplicates are
/// allowed, and ties always select the first index in original storage order.
#[derive(Debug)]
pub struct ProjectedGrid<'a> {
    projection: Projection,
    x: axis::Axis<'a>,
    y: axis::Axis<'a>,
}

impl<'a> ProjectedGrid<'a> {
    /// WASM construction fails even when the pure transform math compiles.
    pub fn new(projection: Projection, x: &'a [f64], y: &'a [f64]) -> Result<Self, Error> {
        if cfg!(target_family = "wasm") {
            return Err(Error::UnsupportedTarget);
        }
        let x = axis::Axis::new(x)?;
        let y = axis::Axis::new(y)?;
        if projection.kind() == ProjectionKind::HrdpsRotatedPole
            && (x.values.iter().any(|v| v.abs() > 180.0) || y.values.iter().any(|v| v.abs() > 90.0))
        {
            return Err(Error::InvalidAxisExtent);
        }
        Ok(Self { projection, x, y })
    }

    pub fn select_nearest(&self, point: GeographicPoint) -> Result<SelectedPoint, Error> {
        self.select_grid_point(self.projection.forward(point)?)
    }

    /// Select using grid-space distances, without a geographic longitude wrap.
    /// Domain bounds include half the median unique-axis spacing on either side.
    /// Singleton/constant axes have zero tolerance. No selection epsilon is used.
    pub fn select_grid_point(&self, point: GridPoint) -> Result<SelectedPoint, Error> {
        let indices = GridIndices {
            x: self.x.nearest(point.x)?,
            y: self.y.nearest(point.y)?,
        };
        self.point_from_indices(indices)
    }

    pub fn point_from_indices(&self, indices: GridIndices) -> Result<SelectedPoint, Error> {
        let point = GridPoint {
            x: *self
                .x
                .values
                .get(indices.x)
                .ok_or(Error::IndexOutOfBounds)?,
            y: *self
                .y
                .values
                .get(indices.y)
                .ok_or(Error::IndexOutOfBounds)?,
        };
        Ok(SelectedPoint {
            indices,
            grid: point,
            geographic: self.projection.inverse(point)?,
        })
    }
}
