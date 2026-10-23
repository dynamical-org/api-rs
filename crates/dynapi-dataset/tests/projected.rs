mod common;
use dynapi_dataset::{DatasetVersion, OpenedDataset, StoreDescriptor};
use dynapi_projection::{HRDPS_WKT, HRRR_WKT, ProjectionKind};
use dynapi_query::{SpatialCrs, point_from_indices, select_nearest_point};
use serde_json::{Value, json};

async fn open(
    root: &std::path::Path,
    wkt: &str,
    dtype: &str,
    attrs: Value,
) -> Result<std::sync::Arc<OpenedDataset>, dynapi_dataset::OpenError> {
    let mut x = common::axis("x", dtype, 3, 3, attrs);
    x["attributes"]["coordinates"] = json!("spatial_ref");
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
    common::repository(
        root,
        &[
            ("x", x, Some(data)),
            (
                "y",
                common::axis("y", "float64", 3, 3, json!({})),
                Some(
                    [-1f64, 0., 1.]
                        .into_iter()
                        .flat_map(f64::to_le_bytes)
                        .collect(),
                ),
            ),
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
        for dtype in ["float64", "float32", "int64"] {
            let temp = tempfile::tempdir().unwrap();
            let result = open(&temp.path().join("repository"), wkt, dtype, json!({})).await;
            if dtype == "float64" {
                assert_eq!(
                    result.unwrap().view().spatial_crs,
                    Some(SpatialCrs::Attested(kind))
                );
            } else {
                let error = match result {
                    Ok(_) => panic!("accepted {dtype}"),
                    Err(error) => error,
                };
                assert_eq!(
                    error.to_string(),
                    "Projected coordinate x must have float64 storage"
                );
            }
        }
    }
    // Integer CF scaling can produce decoded float64 values, but does not attest
    // the original stored dtype used by the native projection qualification.
    let temp = tempfile::tempdir().unwrap();
    let result = open(
        &temp.path().join("repository"),
        HRRR_WKT,
        "int64",
        json!({"scale_factor":0.5}),
    )
    .await;
    assert!(matches!(
        result,
        Err(dynapi_dataset::OpenError::Metadata(_))
    ));
}

#[tokio::test]
async fn supplied_axis_units_are_validated_and_absent_units_keep_wkt_units() {
    for (wkt, latitude, longitude, units) in [
        (HRRR_WKT, 38.5, -97.5, "m"),
        (HRDPS_WKT, 90. - 36.08852, -114.694858, "degrees"),
    ] {
        for attrs in [
            json!({}),
            json!({"units":units}),
            json!({"units":"km"}),
            json!({"units":"degrees_east"}),
            json!({"units":null}),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let valid = attrs.get("units").is_none_or(|u| u == units);
            let dataset = open(&temp.path().join("repository"), wkt, "float64", attrs)
                .await
                .unwrap();
            let selection = select_nearest_point(dataset.view(), latitude, longitude);
            let canonical = point_from_indices(dataset.view(), 1, 1);
            if valid {
                assert_eq!(selection.unwrap(), canonical.unwrap());
            } else {
                for error in [selection.unwrap_err(), canonical.unwrap_err()] {
                    assert_eq!(error.status, 500);
                    assert_eq!(error.detail, "Projected coordinate x has unsupported units");
                }
            }
        }
    }
}
