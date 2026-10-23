//! Private, unwired GRIB2 data decoder. No transport or product admission.

mod ccsds;
mod preflight;

pub(super) const MAX_ENCODED_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_DECODED_BYTES: usize = 56 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Malformed(&'static str),
    Unsupported(&'static str),
    Constant,
    SizeLimit,
    Arithmetic,
    Decoder,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Profile<'a> {
    pub shape: &'a [usize],
    pub spatial_axes: [usize; 2],
    pub adjust_longitude_range: bool,
    pub north_up: bool,
    pub scale_offset: Option<ScaleOffset>,
}

impl Profile<'_> {
    fn plane(self) -> Result<(usize, usize, usize), Error> {
        let [y, x] = self.spatial_axes;
        if self.shape.len() < 2
            || self.shape.len() > 16
            || y >= self.shape.len()
            || x >= self.shape.len()
            || x != y + 1
            || self.shape.contains(&0)
            || self
                .shape
                .iter()
                .enumerate()
                .any(|(axis, n)| axis != x && axis != y && *n != 1)
        {
            return Err(Error::Unsupported(
                "full padded f64 plane with singleton other axes",
            ));
        }
        let decoded = self.shape[y]
            .checked_mul(self.shape[x])
            .and_then(|v| v.checked_mul(8))
            .ok_or(Error::SizeLimit)?;
        if decoded > MAX_DECODED_BYTES {
            return Err(Error::SizeLimit);
        }
        Ok((self.shape[y], self.shape[x], decoded))
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ScaleOffset {
    scale: f64,
    offset: f64,
}

impl ScaleOffset {
    pub(super) fn new(scale: f64, offset: f64) -> Result<Self, Error> {
        if !scale.is_finite() || scale == 0.0 || !offset.is_finite() {
            return Err(Error::Unsupported(
                "finite nonzero f64 scale and finite offset",
            ));
        }
        Ok(Self { scale, offset })
    }

    pub(super) fn parse(codec: &serde_json::Value) -> Result<Self, Error> {
        let object = codec
            .as_object()
            .ok_or(Error::Unsupported("scale_offset object"))?;
        if codec["name"] != "scale_offset"
            || object
                .keys()
                .any(|k| !matches!(k.as_str(), "name" | "configuration"))
        {
            return Err(Error::Unsupported("scale_offset metadata"));
        }
        let Some(config) = object.get("configuration") else {
            return Self::new(1.0, 0.0);
        };
        let config = config
            .as_object()
            .ok_or(Error::Unsupported("scale_offset configuration"))?;
        if config
            .keys()
            .any(|k| !matches!(k.as_str(), "scale" | "offset"))
        {
            return Err(Error::Unsupported("scale_offset configuration field"));
        }
        let parameter = |name: &str, default| {
            config.get(name).map_or(Ok(default), |v| {
                v.as_f64()
                    .ok_or(Error::Unsupported("f64 scale_offset parameter"))
            })
        };
        Self::new(parameter("scale", 1.0)?, parameter("offset", 0.0)?)
    }

    fn apply(self, values: &mut [f64]) {
        for value in values {
            // Preserve f64, round the division before addition, and never fuse.
            let divided = *value / self.scale;
            *value = divided + self.offset;
        }
    }
}

#[derive(Debug)]
pub(super) struct Decoded {
    pub shape: Vec<usize>,
    pub values: Vec<f64>,
}

impl Decoded {
    pub(super) fn into_le_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.values.len() * 8);
        for value in self.values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Charges {
    pub fetch: usize,
    pub decode: usize,
}

/// Check the complete logical reference length before allocating its payload.
/// The caller must also fund its retained input backing allocation. These are
/// qualification targets, not reservations or a process-memory guarantee.
pub(super) fn charges(encoded: u64, profile: Profile<'_>) -> Result<Charges, Error> {
    if encoded == 0 || encoded > MAX_ENCODED_BYTES as u64 {
        return Err(Error::SizeLimit);
    }
    let (_, _, decoded) = profile.plane()?;
    let round = |bytes: usize| bytes.div_ceil(65536) * 65536;
    Ok(Charges {
        fetch: round(4 * encoded as usize + 2 * 1024 * 1024),
        decode: round(8 * decoded + 65536),
    })
}

/// Borrow a complete message, validate before any upstream constructor, then
/// return one owned padded f64 plane. Original input backing stays caller-owned;
/// its lifetime/capacity must be accounted alongside the returned allocation.
pub(super) fn decode(encoded: &[u8], profile: Profile<'_>) -> Result<Decoded, Error> {
    let checked = preflight::validate(encoded, profile)?;
    let message = gribberish::message::Message::from_data(encoded, 0).ok_or(Error::Decoder)?;
    let mut values = message.data().map_err(|_| Error::Decoder)?;
    // Preflight establishes exact streamed counts or the explicit true-zero
    // profile whose resize is qualified. This check is defensive, not admission.
    if values.len() != checked.nx * checked.ny {
        return Err(Error::Decoder);
    }
    if let Some(roll) = checked.roll {
        for row in values.chunks_exact_mut(checked.nx) {
            row.rotate_left(roll);
        }
    }
    if checked.flip {
        for row in 0..checked.ny / 2 {
            for col in 0..checked.nx {
                values.swap(
                    row * checked.nx + col,
                    (checked.ny - 1 - row) * checked.nx + col,
                );
            }
        }
    }
    if let Some(scale) = profile.scale_offset {
        scale.apply(&mut values);
    }
    Ok(Decoded {
        shape: profile.shape.to_vec(),
        values,
    })
}

#[cfg(test)]
mod tests;
