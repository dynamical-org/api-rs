#![cfg(not(target_family = "wasm"))]
use dynapi_projection::{
    Error, GeographicPoint, GridIndices, GridPoint, HRDPS_WKT, HRRR_WKT, ProjectedGrid, Projection,
};

fn projection() -> Projection {
    Projection::from_wkt(HRRR_WKT).unwrap()
}

#[test]
fn origin_selection_and_canonical_indices_use_stored_axes() {
    let grid = ProjectedGrid::new(
        projection(),
        &[-3000.0, 0.0, 3000.0],
        &[3000.0, 0.0, -3000.0],
    )
    .unwrap();
    let point = grid
        .select_nearest(GeographicPoint {
            longitude: -97.5,
            latitude: 38.5,
        })
        .unwrap();
    assert_eq!(point.indices, GridIndices { x: 1, y: 1 });
    assert_eq!(point.grid, GridPoint { x: 0.0, y: 0.0 });
    assert!((point.geographic.latitude - 38.5).abs() < 1e-12);
    assert!((point.geographic.longitude + 97.5).abs() < 1e-12);
    assert_eq!(grid.point_from_indices(point.indices).unwrap(), point);
    let point = grid
        .select_grid_point(GridPoint {
            x: 3000.0,
            y: 3000.0,
        })
        .unwrap();
    assert_eq!(point.indices.canonical_y_x(), [0, 2]);
    assert_eq!(
        grid.point_from_indices(GridIndices { x: 3, y: 0 }),
        Err(Error::IndexOutOfBounds)
    );
    assert_eq!(
        grid.point_from_indices(GridIndices { x: 0, y: 3 }),
        Err(Error::IndexOutOfBounds)
    );
}

#[test]
fn ascending_descending_and_duplicate_ties_choose_first_array_index() {
    for x in [&[0.0, 2.0, 10.0][..], &[0.0, 0.0, 2.0, 10.0][..]] {
        let grid = ProjectedGrid::new(projection(), x, &[0.0]).unwrap();
        assert_eq!(
            grid.select_grid_point(GridPoint { x: 1.0, y: 0.0 })
                .unwrap()
                .indices
                .x,
            0
        );
    }
    let grid = ProjectedGrid::new(projection(), &[10.0, 10.0, 2.0, 0.0], &[2.0, 0.0]).unwrap();
    let point = grid
        .select_grid_point(GridPoint { x: 6.0, y: 1.0 })
        .unwrap();
    assert_eq!(point.indices, GridIndices { x: 0, y: 0 });
}

#[test]
fn bounds_use_median_unique_spacing_and_are_inclusive() {
    // Unique gaps [4,2,8] have median 4: half-cell bounds are [-6,12].
    for x in [
        &[-4.0, -4.0, 0.0, 2.0, 10.0, 10.0][..],
        &[10.0, 10.0, 2.0, 0.0, -4.0, -4.0][..],
    ] {
        let grid = ProjectedGrid::new(projection(), x, &[0.0]).unwrap();
        for x in [-6.0, 12.0] {
            assert!(grid.select_grid_point(GridPoint { x, y: 0.0 }).is_ok());
        }
        for x in [(-6.0_f64).next_down(), 12.0_f64.next_up()] {
            assert_eq!(
                grid.select_grid_point(GridPoint { x, y: 0.0 }),
                Err(Error::OutsideGrid)
            );
        }
    }
    // Even number of unique gaps [2,8]: median 5, half-cell bounds [-2.5,12.5].
    let grid = ProjectedGrid::new(projection(), &[0.0, 2.0, 10.0], &[0.0]).unwrap();
    for x in [-2.5, 12.5] {
        assert!(grid.select_grid_point(GridPoint { x, y: 0.0 }).is_ok());
    }
    assert_eq!(
        grid.select_grid_point(GridPoint {
            x: 12.5_f64.next_up(),
            y: 0.0
        }),
        Err(Error::OutsideGrid)
    );
}

#[test]
fn singleton_and_constant_axes_have_zero_tolerance() {
    for x in [&[7.0][..], &[7.0, 7.0][..]] {
        let grid = ProjectedGrid::new(projection(), x, &[0.0]).unwrap();
        assert_eq!(
            grid.select_grid_point(GridPoint { x: 7.0, y: 0.0 })
                .unwrap()
                .indices
                .x,
            0
        );
        assert_eq!(
            grid.select_grid_point(GridPoint {
                x: 7.0_f64.next_up(),
                y: 0.0
            }),
            Err(Error::OutsideGrid)
        );
        assert_eq!(
            grid.select_grid_point(GridPoint {
                x: 7.0,
                y: 0.0_f64.next_down()
            }),
            Err(Error::OutsideGrid)
        );
    }
}

#[test]
fn invalid_axes_and_points_fail_before_selection() {
    for (axis, error) in [
        (&[][..], Error::EmptyAxis),
        (&[0.0, f64::NAN][..], Error::NonFiniteAxis),
        (&[f64::INFINITY][..], Error::NonFiniteAxis),
        (&[0.0, 2.0, 1.0][..], Error::NonMonotonicAxis),
        (&[-f64::MAX, f64::MAX][..], Error::InvalidAxisExtent),
        (&[f64::MAX / 2.0, f64::MAX][..], Error::InvalidAxisExtent),
    ] {
        assert_eq!(
            ProjectedGrid::new(projection(), axis, &[0.0]).unwrap_err(),
            error
        );
        assert_eq!(
            ProjectedGrid::new(projection(), &[0.0], axis).unwrap_err(),
            error
        );
    }
    let grid = ProjectedGrid::new(projection(), &[0.0, 1.0], &[0.0]).unwrap();
    for point in [
        GridPoint {
            x: f64::NAN,
            y: 0.0,
        },
        GridPoint {
            x: 0.0,
            y: f64::INFINITY,
        },
    ] {
        assert_eq!(grid.select_grid_point(point), Err(Error::InvalidCoordinate));
    }
}

#[test]
fn rotated_grid_returns_geographic_locations_and_preserves_indices() {
    let projection = Projection::from_wkt(HRDPS_WKT).unwrap();
    let grid =
        ProjectedGrid::new(projection, &[-0.0225, 0.0, 0.0225], &[0.0225, 0.0, -0.0225]).unwrap();
    let point = grid
        .select_nearest(GeographicPoint {
            longitude: -114.694858,
            latitude: 53.91148,
        })
        .unwrap();
    assert_eq!(point.indices, GridIndices { x: 1, y: 1 });
    assert_eq!(point.grid, GridPoint { x: 0.0, y: 0.0 });
    assert!((point.geographic.longitude + 114.694858).abs() < 1e-12);
    assert!((point.geographic.latitude - 53.91148).abs() < 1e-12);
    assert_eq!(grid.point_from_indices(point.indices).unwrap(), point);
    assert_eq!(
        grid.select_grid_point(GridPoint { x: 0.1, y: 0.0 }),
        Err(Error::OutsideGrid)
    );
}

#[test]
fn selected_grid_zero_retains_its_stored_sign() {
    let grid = ProjectedGrid::new(projection(), &[-0.0, 0.0], &[0.0]).unwrap();
    let point = grid
        .select_grid_point(GridPoint { x: 0.0, y: 0.0 })
        .unwrap();
    assert_eq!(point.indices.x, 0);
    assert_eq!(point.grid.x.to_bits(), (-0.0_f64).to_bits());
}

#[test]
fn rotated_axes_must_be_in_angular_ranges() {
    let projection = Projection::from_wkt(HRDPS_WKT).unwrap();
    assert_eq!(
        ProjectedGrid::new(projection, &[181.0], &[0.0]).unwrap_err(),
        Error::InvalidAxisExtent
    );
    assert_eq!(
        ProjectedGrid::new(projection, &[0.0], &[-91.0]).unwrap_err(),
        Error::InvalidAxisExtent
    );
}
