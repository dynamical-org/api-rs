mod common;
use dynapi_dataset::{DatasetVersion, OpenedDataset, StoreDescriptor};
use dynapi_projection::{HRDPS_WKT, HRRR_WKT, ProjectionKind};
use dynapi_query::{SpatialCrs, point_from_indices, select_nearest_point};
use serde_json::{Value, json};

fn axis(name: &str, dtype: &str, attrs: Value) -> (Value, Vec<u8>) {
    let data: Vec<u8> = match dtype {
        "float64" => [-1f64, 0., 1.]
            .into_iter()
            .flat_map(f64::to_le_bytes)
            .collect(),
        "float32" => [-1f32, 0., 1.]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect(),
        "int64" => [-1i64, 0, 1]
            .into_iter()
            .flat_map(i64::to_le_bytes)
            .collect(),
        _ => unreachable!(),
    };
    (common::axis(name, dtype, 3, 3, attrs), data)
}

async fn open(
    root: &std::path::Path,
    wkt: &str,
    axis_name: &str,
    dtype: &str,
    attrs: Value,
) -> Result<std::sync::Arc<OpenedDataset>, dynapi_dataset::OpenError> {
    let (mut x, x_data) = if axis_name == "x" {
        axis("x", dtype, attrs.clone())
    } else {
        axis("x", "float64", json!({}))
    };
    let (y, y_data) = if axis_name == "y" {
        axis("y", dtype, attrs)
    } else {
        axis("y", "float64", json!({}))
    };
    x["attributes"]["coordinates"] = json!("spatial_ref");
    common::repository(
        root,
        &[
            ("x", x, Some(x_data)),
            ("y", y, Some(y_data)),
            (
                "spatial_ref",
                common::axis("spatial_ref", "int64", 1, 1, json!({"crs_wkt":wkt})),
                None,
            ),
        ],
    )
    .await;
    OpenedDataset::open(
        &StoreDescriptor::Local(root.to_str().unwrap().into()),
        DatasetVersion::Branch("main"),
        &[],
    )
    .await
}

#[tokio::test]
async fn projected_raw_storage_is_validated_before_serving_decoded_axes() {
    for (wkt, kind) in [
        (HRRR_WKT, ProjectionKind::HrrrLambertConformal),
        (HRDPS_WKT, ProjectionKind::HrdpsRotatedPole),
    ] {
        for axis_name in ["x", "y"] {
            for dtype in ["float64", "float32", "int64"] {
                let temp = tempfile::tempdir().unwrap();
                let result = open(
                    &temp.path().join("repository"),
                    wkt,
                    axis_name,
                    dtype,
                    json!({}),
                )
                .await;
                if dtype == "float64" {
                    assert_eq!(
                        result.unwrap().view().spatial_crs,
                        Some(SpatialCrs::Attested(kind))
                    );
                } else {
                    let error = match result {
                        Ok(_) => panic!("accepted {axis_name}/{dtype}"),
                        Err(error) => error,
                    };
                    assert_eq!(
                        error.to_string(),
                        format!("Projected coordinate {axis_name} must have float64 storage")
                    );
                }
            }
        }
    }
    // Integer CF scaling can produce decoded float64 values, but does not attest
    // the original stored dtype used by the native projection qualification.
    for axis_name in ["x", "y"] {
        let temp = tempfile::tempdir().unwrap();
        let result = open(
            &temp.path().join("repository"),
            HRRR_WKT,
            axis_name,
            "int64",
            json!({"scale_factor":0.5}),
        )
        .await;
        assert!(matches!(
            result,
            Err(dynapi_dataset::OpenError::Metadata(_))
        ));
    }
}

#[tokio::test]
async fn supplied_axis_units_are_validated_and_absent_units_keep_wkt_units() {
    for (wkt, latitude, longitude, units) in [
        (HRRR_WKT, 38.5, -97.5, "m"),
        (HRDPS_WKT, 90. - 36.08852, -114.694858, "degrees"),
    ] {
        for axis_name in ["x", "y"] {
            for attrs in [
                json!({}),
                json!({"units":units}),
                json!({"units":"km"}),
                json!({"units":"degrees_east"}),
                json!({"units":if units == "m" { "degrees" } else { "m" }}),
                json!({"units":null}),
            ] {
                let temp = tempfile::tempdir().unwrap();
                let valid = attrs.get("units").is_none_or(|u| u == units);
                let dataset = open(
                    &temp.path().join("repository"),
                    wkt,
                    axis_name,
                    "float64",
                    attrs,
                )
                .await
                .unwrap();
                let selection = select_nearest_point(dataset.view(), latitude, longitude);
                let canonical = point_from_indices(dataset.view(), 1, 1);
                if valid {
                    assert_eq!(selection.unwrap(), canonical.unwrap());
                } else {
                    for error in [selection.unwrap_err(), canonical.unwrap_err()] {
                        assert_eq!(error.status, 500);
                        assert_eq!(
                            error.detail,
                            format!("Projected coordinate {axis_name} has unsupported units")
                        );
                    }
                }
            }
        }
    }
}
