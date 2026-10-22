use crate::{Error, GeographicPoint, GridPoint, HRDPS_WKT, HRRR_WKT, ProjectionKind};
use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

const RADIUS: f64 = 6_371_229.0;
const LAMBERT_LONGITUDE: f64 = -97.5;
const SOUTH_POLE_LONGITUDE: f64 = -114.694858;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Operation {
    Lambert { n: f64, c: f64, rho0: f64 },
    Rotated { sin_pole: f64, cos_pole: f64 },
}

fn wrap(angle: f64) -> f64 {
    if angle.abs() <= PI {
        angle
    } else {
        (angle + PI).rem_euclid(2.0 * PI) - PI
    }
}

impl Operation {
    pub(crate) fn from_wkt(wkt: &str) -> Result<Self, Error> {
        match wkt {
            HRRR_WKT => {
                let parallel = 38.5_f64.to_radians();
                let n = parallel.sin();
                let c = parallel.cos() * (FRAC_PI_4 + parallel / 2.0).tan().powf(n) / n;
                let rho0 = c * (FRAC_PI_4 + parallel / 2.0).tan().powf(-n);
                Ok(Self::Lambert { n, c, rho0 })
            }
            HRDPS_WKT => {
                let (sin_pole, cos_pole) = 36.08852_f64.to_radians().sin_cos();
                Ok(Self::Rotated { sin_pole, cos_pole })
            }
            _ => Err(Error::UnsupportedCrs),
        }
    }

    pub(crate) fn kind(self) -> ProjectionKind {
        match self {
            Self::Lambert { .. } => ProjectionKind::HrrrLambertConformal,
            Self::Rotated { .. } => ProjectionKind::HrdpsRotatedPole,
        }
    }

    pub(crate) fn forward(self, point: GeographicPoint) -> Result<GridPoint, Error> {
        if !point.longitude.is_finite()
            || !point.latitude.is_finite()
            || point.longitude.abs() > 180.0
            || point.latitude.abs() > 90.0
        {
            return Err(Error::InvalidCoordinate);
        }
        let latitude = point.latitude.to_radians();
        let (x, y) = match self {
            Self::Lambert { n, c, rho0 } => {
                // Snyder, USGS PP 1395, section 15. Equal standard parallels
                // use n = sin(phi_1). Preserve arithmetic grouping: changing
                // a last bit can change a nearest cell at its boundary.
                let rho = if (latitude.abs() - FRAC_PI_2).abs() < 1e-10 {
                    if latitude < 0.0 {
                        return Err(Error::OutsideProjectionDomain);
                    }
                    0.0
                } else {
                    c * (FRAC_PI_4 + latitude / 2.0).tan().powf(-n)
                };
                let theta = n * wrap(point.longitude.to_radians() - LAMBERT_LONGITUDE.to_radians());
                // The false-easting addition in the reference yields +0 at
                // the north pole, even for negative sin(theta).
                let x = RADIUS * (rho * theta.sin()) + 0.0;
                (x, RADIUS * (rho0 - rho * theta.cos()))
            }
            Self::Rotated { sin_pole, cos_pole } => {
                // Snyder equations 5-7 and 5-8b. GRIB convention maps the
                // southern pole to lon_0 and negates its latitude for o_lat_p.
                let longitude =
                    wrap(point.longitude.to_radians() - SOUTH_POLE_LONGITUDE.to_radians());
                let (sin_lat, cos_lat) = latitude.sin_cos();
                let x = (cos_lat * longitude.sin())
                    .atan2(sin_pole * cos_lat * longitude.cos() + cos_pole * sin_lat);
                let y = (sin_pole * sin_lat - cos_pole * cos_lat * longitude.cos())
                    .clamp(-1.0, 1.0)
                    .asin();
                (wrap(x).to_degrees(), y.to_degrees())
            }
        };
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::OutsideProjectionDomain);
        }
        Ok(GridPoint { x, y })
    }

    pub(crate) fn inverse(self, point: GridPoint) -> Result<GeographicPoint, Error> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(Error::InvalidCoordinate);
        }
        let (longitude, latitude) = match self {
            Self::Lambert { n, c, rho0 } => {
                let x = point.x * (1.0 / RADIUS);
                let y = rho0 - point.y * (1.0 / RADIUS);
                let rho = x.hypot(y);
                let (longitude, latitude) = if rho == 0.0 {
                    (0.0, FRAC_PI_2)
                } else {
                    (
                        x.atan2(y) / n,
                        2.0 * (c / rho).powf(1.0 / n).atan() - FRAC_PI_2,
                    )
                };
                (
                    wrap(longitude + LAMBERT_LONGITUDE.to_radians()).to_degrees(),
                    latitude.to_degrees(),
                )
            }
            Self::Rotated { sin_pole, cos_pole } => {
                if point.x.abs() > 180.0 || point.y.abs() > 90.0 {
                    return Err(Error::InvalidCoordinate);
                }
                // Snyder equations 5-9 and 5-10b: inverse spherical rotation.
                let longitude = point.x.to_radians();
                let (sin_lat, cos_lat) = point.y.to_radians().sin_cos();
                let latitude = (sin_pole * sin_lat + cos_pole * cos_lat * longitude.cos())
                    .clamp(-1.0, 1.0)
                    .asin();
                let longitude = (cos_lat * longitude.sin())
                    .atan2(sin_pole * cos_lat * longitude.cos() - cos_pole * sin_lat);
                (
                    wrap(longitude + SOUTH_POLE_LONGITUDE.to_radians()).to_degrees(),
                    latitude.to_degrees(),
                )
            }
        };
        if !longitude.is_finite() || !latitude.is_finite() {
            return Err(Error::OutsideProjectionDomain);
        }
        Ok(GeographicPoint {
            longitude,
            latitude,
        })
    }
}
