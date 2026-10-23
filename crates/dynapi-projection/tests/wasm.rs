#![cfg(target_family = "wasm")]
use dynapi_projection::{Error, HRDPS_WKT, HRRR_WKT, ProjectedGrid, Projection};

#[test]
fn projected_selection_is_not_qualified_on_wasm() {
    for wkt in [HRRR_WKT, HRDPS_WKT] {
        let projection = Projection::from_wkt(wkt).unwrap();
        assert_eq!(
            ProjectedGrid::new(projection, &[0.0], &[0.0]).unwrap_err(),
            Error::UnsupportedTarget
        );
    }
}
