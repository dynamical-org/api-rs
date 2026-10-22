use dynapi_projection::{
    Error, GeographicPoint, GridPoint, HRDPS_WKT, HRRR_WKT, Projection, ProjectionKind,
};

fn close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual:?} != {expected:?}"
    );
}

#[test]
fn exact_wkt_attests_an_operation_not_a_geographic_identity() {
    assert_eq!(
        Projection::from_wkt(HRRR_WKT).unwrap().kind(),
        ProjectionKind::HrrrLambertConformal
    );
    assert_eq!(
        Projection::from_wkt(HRDPS_WKT).unwrap().kind(),
        ProjectionKind::HrdpsRotatedPole
    );
    for wkt in [
        "",
        "EPSG:4326",
        "GEOGCS[\"unknown\"]",
        "Lambert_Conformal_Conic_2SP",
    ] {
        assert_eq!(
            Projection::from_wkt(wkt).unwrap_err(),
            Error::UnsupportedCrs
        );
    }
    for wkt in [HRRR_WKT, HRDPS_WKT] {
        assert_eq!(
            Projection::from_wkt(&format!(" {wkt}")).unwrap_err(),
            Error::UnsupportedCrs
        );
        assert_eq!(
            Projection::from_wkt(&wkt.replace("6371229", "6371000")).unwrap_err(),
            Error::UnsupportedCrs
        );
    }
}

#[test]
fn lambert_origin_and_central_meridian_symmetry() {
    let projection = Projection::from_wkt(HRRR_WKT).unwrap();
    let origin = GeographicPoint {
        longitude: -97.5,
        latitude: 38.5,
    };
    let grid = projection.forward(origin).unwrap();
    close(grid.x, 0.0, 1e-8);
    close(grid.y, 0.0, 1e-8);
    let geographic = projection.inverse(GridPoint { x: 0.0, y: 0.0 }).unwrap();
    close(geographic.longitude, origin.longitude, 1e-12);
    close(geographic.latitude, origin.latitude, 1e-12);
    let west = projection
        .forward(GeographicPoint {
            longitude: -107.5,
            latitude: 45.0,
        })
        .unwrap();
    let east = projection
        .forward(GeographicPoint {
            longitude: -87.5,
            latitude: 45.0,
        })
        .unwrap();
    close(west.x, -east.x, 1e-8);
    close(west.y, east.y, 1e-8);
}

#[test]
fn lambert_matches_published_spherical_equations() {
    // Snyder, USGS PP 1395, section 15, equations 15-1 through 15-5.
    // Equal standard parallels give n = sin(phi_1). This is an analytic
    // calculation from explicit angles/radius, not a service capture.
    let projection = Projection::from_wkt(HRRR_WKT).unwrap();
    let parallel = 38.5_f64.to_radians();
    let n = parallel.sin();
    let f = parallel.cos() * (std::f64::consts::FRAC_PI_4 + parallel / 2.0).tan().powf(n) / n;
    let rho0 = 6_371_229.0 * f / (std::f64::consts::FRAC_PI_4 + parallel / 2.0).tan().powf(n);
    for (longitude, latitude) in [(-120.0_f64, 25.0_f64), (-90.0, 50.0), (170.0, 15.0)] {
        let rho = 6_371_229.0 * f
            / (std::f64::consts::FRAC_PI_4 + latitude.to_radians() / 2.0)
                .tan()
                .powf(n);
        let angle = n * ((longitude + 97.5 + 180.0).rem_euclid(360.0) - 180.0).to_radians();
        let actual = projection
            .forward(GeographicPoint {
                longitude,
                latitude,
            })
            .unwrap();
        close(actual.x, rho * angle.sin(), 1e-7);
        close(actual.y, rho0 - rho * angle.cos(), 1e-7);
    }
}

#[test]
fn rotation_origin_and_geographic_pole_follow_the_grib_convention() {
    let projection = Projection::from_wkt(HRDPS_WKT).unwrap();
    let origin = projection.inverse(GridPoint { x: 0.0, y: 0.0 }).unwrap();
    close(origin.longitude, -114.694858, 1e-12);
    close(origin.latitude, 53.91148, 1e-12);
    let grid = projection.forward(origin).unwrap();
    close(grid.x, 0.0, 1e-12);
    close(grid.y, 0.0, 1e-12);
    let north = projection
        .forward(GeographicPoint {
            longitude: -114.694858,
            latitude: 90.0,
        })
        .unwrap();
    close(north.x, 0.0, 1e-12);
    close(north.y, 36.08852, 1e-12);
}

fn unit_vector(point: GeographicPoint) -> [f64; 3] {
    let lon = point.longitude.to_radians();
    let lat = point.latitude.to_radians();
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

#[test]
fn rotation_preserves_spherical_distances() {
    let projection = Projection::from_wkt(HRDPS_WKT).unwrap();
    let points = [
        GeographicPoint {
            longitude: -130.0,
            latitude: 55.0,
        },
        GeographicPoint {
            longitude: -70.0,
            latitude: 45.0,
        },
        GeographicPoint {
            longitude: 179.0,
            latitude: -30.0,
        },
    ];
    let vectors: Vec<_> = points.iter().map(|&point| unit_vector(point)).collect();
    let rotated: Vec<_> = points
        .iter()
        .map(|&point| {
            let grid = projection.forward(point).unwrap();
            unit_vector(GeographicPoint {
                longitude: grid.x,
                latitude: grid.y,
            })
        })
        .collect();
    for i in 0..points.len() {
        for j in 0..points.len() {
            let dot = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).map(|(a, b)| a * b).sum::<f64>();
            close(
                dot(vectors[i], vectors[j]),
                dot(rotated[i], rotated[j]),
                1e-14,
            );
        }
    }
}

#[test]
fn round_trips_and_dateline_equivalence() {
    for wkt in [HRRR_WKT, HRDPS_WKT] {
        let projection = Projection::from_wkt(wkt).unwrap();
        for latitude in [-70.0, -20.0, 0.0, 30.0, 70.0] {
            for longitude in [-179.0, -120.0, -60.0, 0.0, 120.0, 179.0] {
                let point = GeographicPoint {
                    longitude,
                    latitude,
                };
                let inverse = projection
                    .inverse(projection.forward(point).unwrap())
                    .unwrap();
                close(inverse.latitude, latitude, 1e-10);
                close(
                    (inverse.longitude - longitude + 180.0).rem_euclid(360.0) - 180.0,
                    0.0,
                    1e-10,
                );
            }
        }
        let west = projection
            .forward(GeographicPoint {
                longitude: -180.0,
                latitude: 45.0,
            })
            .unwrap();
        let east = projection
            .forward(GeographicPoint {
                longitude: 180.0,
                latitude: 45.0,
            })
            .unwrap();
        close(west.x, east.x, 1e-7);
        close(west.y, east.y, 1e-7);
    }
}

#[test]
fn lambert_poles_and_signed_zero_are_explicit() {
    let projection = Projection::from_wkt(HRRR_WKT).unwrap();
    for longitude in [-180.0, -120.0, 0.0, 120.0, 180.0] {
        let north = projection
            .forward(GeographicPoint {
                longitude,
                latitude: 90.0,
            })
            .unwrap();
        assert_eq!(north.x.to_bits(), 0.0_f64.to_bits());
        close(projection.inverse(north).unwrap().latitude, 90.0, 1e-12);
        assert_eq!(
            projection.forward(GeographicPoint {
                longitude,
                latitude: -90.0
            }),
            Err(Error::OutsideProjectionDomain)
        );
    }
}

#[test]
fn nonfinite_and_out_of_range_points_are_rejected() {
    for wkt in [HRRR_WKT, HRDPS_WKT] {
        let projection = Projection::from_wkt(wkt).unwrap();
        for (longitude, latitude) in [
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
            (181.0, 0.0),
            (0.0, -91.0),
        ] {
            assert_eq!(
                projection.forward(GeographicPoint {
                    longitude,
                    latitude
                }),
                Err(Error::InvalidCoordinate)
            );
        }
        for (x, y) in [(f64::NAN, 0.0), (0.0, f64::NEG_INFINITY)] {
            assert_eq!(
                projection.inverse(GridPoint { x, y }),
                Err(Error::InvalidCoordinate)
            );
        }
    }
    let projection = Projection::from_wkt(HRDPS_WKT).unwrap();
    assert_eq!(
        projection.inverse(GridPoint { x: 0.0, y: 91.0 }),
        Err(Error::InvalidCoordinate)
    );
}

#[test]
fn lambert_opposite_meridian_retains_the_published_seam_band() {
    // PROJ's published adjlon rule preserves |lambda| < pi + 1e-12,
    // including representable neighbors just across the opposite meridian.
    // Expected coordinates use Snyder section 15 with that longitude rule.
    let projection = Projection::from_wkt(HRRR_WKT).unwrap();
    let parallel = 38.5_f64.to_radians();
    let n = parallel.sin();
    let c = parallel.cos() * (std::f64::consts::FRAC_PI_4 + parallel / 2.0).tan().powf(n) / n;
    let rho0 = c
        * (std::f64::consts::FRAC_PI_4 + parallel / 2.0)
            .tan()
            .powf(-n);
    let rho = c * std::f64::consts::FRAC_PI_4.tan().powf(-n);
    for longitude in [82.5_f64.next_down(), 82.5, 82.5_f64.next_up(), 82.5 + 1e-11] {
        let delta = longitude.to_radians() - (-97.5_f64).to_radians();
        assert!(delta.abs() < std::f64::consts::PI + 1e-12);
        let theta = n * delta;
        let point = projection
            .forward(GeographicPoint {
                longitude,
                latitude: 0.0,
            })
            .unwrap();
        assert!(point.x > 0.0);
        close(point.x, 6_371_229.0 * (rho * theta.sin()), 1e-8);
        close(point.y, 6_371_229.0 * (rho0 - rho * theta.cos()), 1e-8);
    }
    assert!(
        projection
            .forward(GeographicPoint {
                longitude: 82.5 + 1e-10,
                latitude: 0.0
            })
            .unwrap()
            .x
            < 0.0
    );
}

#[test]
fn rotated_inverse_preserves_a_dateline_seam_overshoot() {
    // Generate the grid input from an explicit geographic meridian, then
    // evaluate Snyder 5-10b independently, before longitude reduction.
    let projection = Projection::from_wkt(HRDPS_WKT).unwrap();
    let pole = 36.08852_f64.to_radians();
    let mut overshoots = 0;
    for longitude in [-180.0, 180.0] {
        for latitude in [-65.0, -5.0, 0.0, 35.0, 65.0] {
            let base = projection
                .forward(GeographicPoint {
                    longitude,
                    latitude,
                })
                .unwrap();
            for x in [base.x.next_down(), base.x, base.x.next_up()] {
                for y in [base.y.next_down(), base.y, base.y.next_up()] {
                    let longitude = x.to_radians();
                    let latitude = y.to_radians();
                    let delta = (latitude.cos() * longitude.sin()).atan2(
                        pole.sin() * latitude.cos() * longitude.cos() - pole.cos() * latitude.sin(),
                    );
                    let unwrapped = delta + (-114.694858_f64).to_radians();
                    if unwrapped.abs() <= std::f64::consts::PI {
                        continue;
                    }
                    assert!(unwrapped.abs() < std::f64::consts::PI + 1e-12);
                    let geographic = projection.inverse(GridPoint { x, y }).unwrap();
                    assert_eq!(
                        geographic.longitude.to_bits(),
                        unwrapped.to_degrees().to_bits()
                    );
                    assert!(geographic.longitude.abs() > 180.0);
                    // Forward input validation stays strict; output is not clipped.
                    assert_eq!(
                        projection.forward(geographic),
                        Err(Error::InvalidCoordinate)
                    );
                    overshoots += 1;
                }
            }
        }
    }
    assert!(overshoots > 0);
}
