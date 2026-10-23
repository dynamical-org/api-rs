use dynapi_contract::Value;
use dynapi_projection::ProjectionKind;
use dynapi_query::*;

fn grid(kind: ProjectionKind) -> DatasetView {
    let scale = if kind == ProjectionKind::HrrrLambertConformal {
        3000.
    } else {
        1.
    };
    DatasetView {
        dimensions: [("y".into(), 3), ("x".into(), 3)].into(),
        coordinates: [
            ("y".into(), Coordinate::Numeric(vec![scale, 0., -scale])),
            ("x".into(), Coordinate::Numeric(vec![-scale, 0., scale])),
        ]
        .into(),
        variables: ["x", "y"]
            .map(|name| {
                (
                    name.into(),
                    Variable {
                        is_coordinate: true,
                        dimensions: vec![name.into()],
                        attributes: [(
                            "units".into(),
                            Value::from(if kind == ProjectionKind::HrrrLambertConformal {
                                "m"
                            } else {
                                "degrees"
                            }),
                        )]
                        .into(),
                        ..Variable::default()
                    },
                )
            })
            .into(),
        spatial_crs: Some(SpatialCrs::Attested(kind)),
        ..DatasetView::default()
    }
}

#[test]
#[cfg(not(target_family = "wasm"))]
fn lambert_origin_selects_stored_indices_and_round_trips() {
    let view = grid(ProjectionKind::HrrrLambertConformal);
    let point = select_nearest_point(&view, 38.5, -97.5).unwrap();
    assert_eq!(point.indexers, [("y".into(), 1), ("x".into(), 1)]);
    assert!((point.latitude - 38.5).abs() < 1e-12);
    assert!((point.longitude + 97.5).abs() < 1e-12);
    assert_eq!(point_from_indices(&view, 1, 1).unwrap(), point);
}

#[test]
#[cfg(not(target_family = "wasm"))]
fn rotated_origin_uses_rotation_in_both_directions() {
    let view = grid(ProjectionKind::HrdpsRotatedPole);
    // Rotated (0,0) is on the southern-pole meridian, 90 degrees from that pole.
    let point = select_nearest_point(&view, 90. - 36.08852, -114.694858).unwrap();
    assert_eq!(point.indexers, [("y".into(), 1), ("x".into(), 1)]);
    assert!((point.latitude - (90. - 36.08852)).abs() < 1e-12);
    assert!((point.longitude + 114.694858).abs() < 1e-12);
    assert_eq!(point_from_indices(&view, 1, 1).unwrap(), point);
    for y in 0..3 {
        for x in 0..3 {
            let canonical = point_from_indices(&view, y, x).unwrap();
            assert_eq!(
                select_nearest_point(&view, canonical.latitude, canonical.longitude).unwrap(),
                canonical
            );
        }
    }
}

#[test]
#[cfg(not(target_family = "wasm"))]
fn projected_outside_requests_have_owned_error_and_no_geographic_fallback() {
    for kind in [
        ProjectionKind::HrrrLambertConformal,
        ProjectionKind::HrdpsRotatedPole,
    ] {
        let view = grid(kind);
        for (latitude, longitude) in [(0., 0.), (-90., 0.), (90., 180.)] {
            let error = select_nearest_point(&view, latitude, longitude).unwrap_err();
            assert_eq!(error.status, 422);
            assert_eq!(
                error.body(),
                br#"{"detail":"Location is outside the data product grid"}"#
            );
        }
        let error = point_from_indices(&view, 3, 0).unwrap_err();
        assert_eq!(error, QueryError::invalid("y_index is outside the grid"));
        let error = point_from_indices(&view, 0, -1).unwrap_err();
        assert_eq!(error, QueryError::invalid("x_index is outside the grid"));
    }
}

#[test]
fn malformed_projected_coordinates_fail_closed_on_both_paths() {
    for kind in [
        ProjectionKind::HrrrLambertConformal,
        ProjectionKind::HrdpsRotatedPole,
    ] {
        for axis_name in ["x", "y"] {
            for invalid in 0..9 {
                let mut view = grid(kind);
                match invalid {
                    0 => {
                        view.coordinates
                            .insert(axis_name.into(), Coordinate::Float32(vec![-1., 0., 1.]));
                    }
                    1 => {
                        view.coordinates
                            .insert(axis_name.into(), Coordinate::Int64(vec![-1, 0, 1]));
                    }
                    2 => {
                        view.variables
                            .get_mut(axis_name)
                            .unwrap()
                            .attributes
                            .insert("units".into(), Value::from("km"));
                    }
                    3 => {
                        view.variables
                            .get_mut(axis_name)
                            .unwrap()
                            .attributes
                            .insert("units".into(), Value::Null);
                    }
                    4 => {
                        view.coordinates
                            .insert(axis_name.into(), Coordinate::Numeric(vec![0.]));
                    }
                    5 => {
                        view.variables.get_mut(axis_name).unwrap().is_coordinate = false;
                    }
                    6 => {
                        view.variables.get_mut(axis_name).unwrap().dimensions =
                            vec!["other".into()];
                    }
                    7 => {
                        view.variables.remove(axis_name);
                    }
                    8 => {
                        view.variables
                            .get_mut(axis_name)
                            .unwrap()
                            .attributes
                            .insert(
                                "units".into(),
                                Value::from(if kind == ProjectionKind::HrrrLambertConformal {
                                    "degrees"
                                } else {
                                    "m"
                                }),
                            );
                    }
                    _ => unreachable!(),
                }
                assert_eq!(
                    select_nearest_point(&view, 38.5, -97.5).unwrap_err().status,
                    500,
                    "{kind:?}/{axis_name}/{invalid}"
                );
                assert_eq!(
                    point_from_indices(&view, 1, 1).unwrap_err().status,
                    500,
                    "{kind:?}/{axis_name}/{invalid}"
                );
            }
        }
    }
}

#[test]
fn missing_unknown_crs_and_index_precedence_are_preserved() {
    for (crs, expected) in [
        (
            Some(SpatialCrs::Missing),
            QueryError::invalid("Projected grid is missing its CRS"),
        ),
        (
            Some(SpatialCrs::Projected),
            QueryError::internal("Projected-CRS grids are not yet supported"),
        ),
        (
            None,
            QueryError::invalid("Dataset has an unsupported spatial grid"),
        ),
    ] {
        let mut view = grid(ProjectionKind::HrrrLambertConformal);
        view.spatial_crs = crs;
        assert_eq!(
            select_nearest_point(&view, 38.5, -97.5).unwrap_err(),
            expected
        );
        assert_eq!(point_from_indices(&view, 1, 1).unwrap_err(), expected);
        assert_eq!(
            point_from_indices(&view, -1, 1).unwrap_err(),
            QueryError::invalid("y_index is outside the grid")
        );
    }
}

#[test]
fn latitude_longitude_dimensions_keep_precedence_over_projected_xy() {
    let mut view = grid(ProjectionKind::HrdpsRotatedPole);
    view.dimensions
        .extend([("latitude".into(), 1), ("longitude".into(), 1)]);
    view.coordinates.extend([
        ("latitude".into(), Coordinate::Numeric(vec![45.])),
        ("longitude".into(), Coordinate::Numeric(vec![-100.])),
    ]);
    let point = select_nearest_point(&view, 45., -100.).unwrap();
    assert_eq!(
        point.indexers,
        [("latitude".into(), 0), ("longitude".into(), 0)]
    );
    assert_eq!(point_from_indices(&view, 0, 0).unwrap(), point);
}

#[test]
#[cfg(target_family = "wasm")]
fn query_projected_paths_fail_closed_on_unsupported_target() {
    for kind in [
        ProjectionKind::HrrrLambertConformal,
        ProjectionKind::HrdpsRotatedPole,
    ] {
        let view = grid(kind);
        for error in [
            select_nearest_point(&view, 38.5, -97.5).unwrap_err(),
            point_from_indices(&view, 1, 1).unwrap_err(),
        ] {
            assert_eq!(error.status, 500);
            assert_eq!(
                error.detail,
                "Projected grid: projected grid selection is unsupported on WASM"
            );
        }
    }
}
