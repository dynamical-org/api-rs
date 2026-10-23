//! The admitted float32 subset of the Zarr scale_offset extension.
use super::{ReaderError, Value, unsupported};

#[derive(Clone, Copy, Debug)]
pub(super) struct ScaleOffset {
    scale: f32,
    offset: f32,
}

impl ScaleOffset {
    pub(super) fn parse(codec: &Value) -> Result<Self, ReaderError> {
        if !super::exact_keys(codec, &["name", "configuration"]) || codec["name"] != "scale_offset"
        {
            return Err(unsupported("Expected configured float32 scale_offset"));
        }
        let config = codec["configuration"]
            .as_object()
            .ok_or_else(|| unsupported("Expected scale_offset configuration object"))?;
        if config
            .keys()
            .any(|key| !matches!(key.as_str(), "scale" | "offset"))
        {
            return Err(unsupported("Unknown scale_offset configuration"));
        }
        let parameter = |name: &str, default: f32| -> Result<f32, ReaderError> {
            let Some(value) = config.get(name) else {
                return Ok(default);
            };
            let value = value
                .as_f64()
                .ok_or_else(|| unsupported("Expected numeric scale_offset parameter"))?
                as f32;
            if !value.is_finite() {
                return Err(unsupported(
                    "Expected finite float32 scale_offset parameter",
                ));
            }
            Ok(value)
        };
        let scale = parameter("scale", 1.0)?;
        if scale == 0.0 {
            return Err(unsupported("scale_offset scale must be nonzero in float32"));
        }
        Ok(Self {
            scale,
            offset: parameter("offset", 0.0)?,
        })
    }

    pub(super) fn decode(self, bytes: &mut [u8]) {
        for bytes in bytes.chunks_exact_mut(4) {
            let input = f32::from_le_bytes(bytes.try_into().expect("float32 width"));
            // Round the division before addition, in the stored dtype. Do not
            // widen or fuse arithmetic; the extension preserves array dtype.
            let divided = input / self.scale;
            bytes.copy_from_slice(&(divided + self.offset).to_le_bytes());
        }
    }
}
