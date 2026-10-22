# dynapi-projection

Dependency-free spherical transforms and native nearest-point selection for the
catalog's exact HRRR Lambert conformal conic and HRDPS rotated-pole WKT inputs.
This is a library; query/dataset adapter and server integration remain separate.

`Projection::from_wkt` attests a coordinate **operation**, not a general CRS
classification. Unknown, malformed, altered or reformatted WKT fails closed.
HRDPS's derived geographic CRS requires rotation; an adapter must choose that
operation before its identity-geographic shortcut. The five other geographic
CRS inputs covered by D12 keep their identity operation. The adapter must also
preserve attribute precedence, coordinate membership and missing-versus-unknown
CRS errors, and verify axis dtype and units. This crate does not read metadata.

`GeographicPoint` uses longitude/latitude degrees in [-180,180]/[-90,90].
Those are strict forward-input ranges. Inverse longitude follows PROJ's
published [adjlon seam convention](https://github.com/OSGeo/PROJ/blob/9.5.1/src/adjlon.cpp): angles with magnitude less than pi + 1e-12
radians remain unchanged before conversion to degrees. Output can therefore
slightly exceed +/-180 degrees (by less than about 5.73e-11 degrees); it is not
silently clipped or automatically accepted as a new forward request. This
longitude-reduction rule adds no tolerance to grid admission or nearest indices.
`GridPoint` uses x/y metres for HRRR and rotated x/y degrees for HRDPS. These are
spherical horizontal operations with radius 6,371,229 m; no geocentric datum
shift, height, epoch or external projection database is involved. Formulas follow
[Snyder, USGS PP 1395](https://pubs.usgs.gov/pp/1395/report.pdf), sections 5 and 15,
with the [GRIB pole convention](https://proj.org/en/stable/development/reference/cpp/operation.html).
Floating-point grouping is deliberate; no rounding tolerance changes a selected
index, domain admission or returned location.

`ProjectedGrid::new(projection, x, y)` borrows the actual stored float64 axes.
Both must be nonempty, finite and monotonic in either direction. Duplicates are
accepted; equal-distance cells always select the first index in original array
order. Constant/singleton axes have zero domain tolerance. Other axes admit the
inclusive interval from minimum minus half the median **unique-coordinate**
spacing to maximum plus that amount. Nonfinite/overflowing extents are rejected;
rotated axes must fit longitude/latitude ranges. No regular-spacing reconstruction,
grid-space longitude wrapping, or epsilon is used. Validation sorts only the
distinct adjacent spacings; nearest selection scans each borrowed axis.

`select_nearest` transforms a geographic request before selecting x/y indices.
`select_grid_point` applies the same selector directly to grid coordinates.
Both return original axis values, indices, and the inverse-transformed selected
geographic location. `point_from_indices` reconstructs that same location without
changing stored indices or values. `GridIndices` names x/y explicitly;
`canonical_y_x()` returns canonical y/x order. Stored signed zeros are preserved;
the LCC north-pole forward easting is positive zero.

Projected selection is **unsupported on WASM**: grid construction returns
`Error::UnsupportedTarget`. The pure transform math can compile there for
experiments, but this is not qualification for projected serving. Native and WASM
math can choose different cells at boundaries, with different weather values.
See D-proj-2 for the future target/selector qualification gate. Native numerical
qualification must identify the release compiler and math runtime; compilation
alone does not establish universal bit-for-bit parity.

Both the reference Python service and this library use HRDPS rotation for
geographic selection and canonical locations while preserving stored indices and
values. D-proj-1 records this alignment. This crate does not enable either
projected family in any service route.

Tests own analytic origins, published spherical equations, inverse/forward and
rotation invariants, exact WKT rejection, pole/dateline behavior, explicit axis
fixtures, ties, neighboring representable bounds, invalid axes and index order.
They contain no reference-service captures, Python or network dependencies.
