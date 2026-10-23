//! Checked CCSDS block walk; no field reconstruction or output allocation.
use super::Error;

type Result<T> = std::result::Result<T, Error>;
const BLOCK: usize = 32;
const RSI: usize = 128;

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
    // Match the pinned decoder's byte read-ahead, including its fast-path
    // seven-byte refills. Logical bit bounds alone do not certify that path.
    loaded: usize,
}

impl Reader<'_> {
    fn available(&self) -> usize {
        self.data.len() - self.loaded
    }

    fn read(&mut self, width: usize, fast: bool) -> Result<u32> {
        let buffered = self.loaded * 8 - self.position;
        if buffered < width {
            let loaded = if fast {
                self.loaded + ((63 - buffered) >> 3)
            } else {
                (self.position + width).div_ceil(8)
            };
            if loaded > self.data.len() {
                return Err(Error::Malformed("CCSDS truncated bit field"));
            }
            self.loaded = loaded;
        }
        let mut value = 0;
        for index in self.position..self.position + width {
            value = (value << 1) | u32::from((self.data[index / 8] >> (7 - index % 8)) & 1);
        }
        self.position += width;
        Ok(value)
    }

    fn unary(&mut self, limit: u32, fast: bool) -> Result<u32> {
        let mut value = 0;
        loop {
            if self.position == self.loaded * 8 {
                let refill = if fast { 7 } else { 1 };
                if self.available() < refill {
                    return Err(if fast {
                        Error::Unsupported("CCSDS short fast unary tail")
                    } else {
                        Error::Malformed("CCSDS truncated unary code")
                    });
                }
                self.loaded += refill;
            }
            let offset = self.position % 8;
            let zeros = (self.data[self.position / 8] << offset)
                .leading_zeros()
                .min((8 - offset) as u32);
            value += zeros;
            if value > limit {
                return Err(Error::Malformed("CCSDS unary value exceeds profile"));
            }
            self.position += zeros as usize;
            if zeros < (8 - offset) as u32 {
                self.position += 1;
                return Ok(value);
            }
        }
    }

    fn finish(&self) -> Result<()> {
        if self.data.len() * 8 - self.position > 7 {
            return Err(Error::Malformed("CCSDS trailing bytes"));
        }
        for index in self.position..self.data.len() * 8 {
            if self.data[index / 8] & (1 << (7 - index % 8)) != 0 {
                return Err(Error::Malformed("CCSDS nonzero padding"));
            }
        }
        Ok(())
    }
}

pub(super) fn validate(p: &[u8], data: &[u8], points: usize, bitmap: bool) -> Result<()> {
    if p.len() != 25 {
        return Err(Error::Malformed("CCSDS template length"));
    }
    if p[21] != 14 || p[22] != BLOCK as u8 || p[23..25] != (RSI as u16).to_be_bytes() {
        return Err(Error::Unsupported("CCSDS options14 block32 RSI128"));
    }
    let width = p[19] as usize;
    if width == 0 {
        // The pinned empty-vector/resize path is correct only for an unmasked
        // positive-zero reference and an empty stream. Do not synthesize other
        // constants or lose a bitmap's missing values.
        if p[11..15] != 0u32.to_be_bytes() || bitmap {
            return Err(Error::Constant);
        }
        return if data.is_empty() {
            Ok(())
        } else {
            Err(Error::Malformed("CCSDS zero-bit payload"))
        };
    }
    // The pinned byte-to-f32 intermediate is exact for these integer widths.
    // Its four-byte path interprets bit patterns as f32, not integer samples.
    if !(2..=16).contains(&width) {
        return Err(Error::Unsupported("CCSDS integer width 2 through 16"));
    }
    if cfg!(target_endian = "big") && width > 8 {
        return Err(Error::Unsupported("CCSDS little-endian integer conversion"));
    }
    if points == 0 {
        return Err(Error::Malformed("CCSDS empty packed field"));
    }
    let id_width = match width {
        2..=3 => 1,
        4 => 2,
        5..=8 => 3,
        _ => 4,
    };
    let uncompressed = (1 << id_width) - 1;
    let input_margin = (BLOCK * width + id_width) / 8 + 16;
    let maximum = (1u32 << width) - 1;
    let mut reader = Reader {
        data,
        position: 0,
        loaded: 0,
    };
    let mut done = 0;
    while done < points {
        let block = (done / BLOCK) % RSI;
        let reference = usize::from(block == 0);
        let remaining = points - done;
        let id = reader.read(id_width, reader.available() >= input_margin)?;
        if id == uncompressed {
            let fast = remaining >= BLOCK && reader.available() >= input_margin;
            // UncompCopy emits only r samples in a partial final block. Check
            // its unused encoded sample fields too, with slow reads: they may
            // not turn into another block or arbitrary trailing bytes.
            for _ in 0..BLOCK {
                reader.read(width, fast)?;
            }
        } else if id == 0 {
            let second_extension = reader.read(1, false)? != 0;
            if reference != 0 {
                reader.read(width, false)?;
            }
            if second_extension {
                let fast = reader.available() >= input_margin && points - done - reference >= BLOCK;
                let mut sample = reference;
                while sample < BLOCK {
                    let m = reader.unary(90, fast).map_err(|error| {
                        if error == Error::Malformed("CCSDS unary value exceeds profile") {
                            Error::Unsupported("CCSDS second-extension table")
                        } else {
                            error
                        }
                    })?;
                    // Invert the format's triangular pair index. Narrow
                    // widths also need each emitted mapped value in range.
                    let mut beta = 0;
                    while (beta + 1) * (beta + 2) / 2 <= m {
                        beta += 1;
                    }
                    let second = m - beta * (beta + 1) / 2;
                    if second > maximum || (sample.is_multiple_of(2) && beta - second > maximum) {
                        return Err(Error::Malformed("CCSDS mapped sample exceeds width"));
                    }
                    // SEIncremental stops at r outputs, possibly between the
                    // pair's two values. The remaining unary fields are a
                    // checked suffix of this block, not decoder output.
                    sample += if sample.is_multiple_of(2) { 2 } else { 1 };
                }
            } else {
                let code = reader.unary(RSI as u32, false)? + 1;
                let ros = code == 5;
                let blocks = if ros {
                    (RSI - block).min(64 - block % 64)
                } else if code > 5 {
                    code as usize - 1
                } else {
                    code as usize
                };
                if blocks > RSI - block || (!ros && blocks > remaining.div_ceil(BLOCK)) {
                    return Err(Error::Malformed("CCSDS zero run exceeds count or RSI"));
                }
                // ROS also means the remaining segment at end of sequence.
                // The pinned ZeroOutput path stops at the attested output count.
                done += (blocks * BLOCK).min(remaining);
                continue;
            }
        } else {
            let k = id as usize - 1;
            let fast = remaining >= BLOCK && reader.available() >= input_margin;
            if reference != 0 {
                reader.read(width, fast)?;
            }
            let count = BLOCK - reference;
            let mut bases = [0u32; BLOCK];
            for base in &mut bases[..count] {
                *base = reader.unary(maximum >> k, fast)? << k;
            }
            if fast && k != 0 && reader.available() < k * count / 8 + 9 {
                return Err(Error::Unsupported("CCSDS pinned binary input margin"));
            }
            // SplitFs reads all 32-reference unary codes even for r outputs.
            // SplitOutput consumes only r-reference remainders; validate the
            // remaining fixed-width fields as this final block's suffix.
            for base in &bases[..count] {
                let remainder = if k == 0 { 0 } else { reader.read(k, fast)? };
                if base + remainder > maximum {
                    return Err(Error::Malformed("CCSDS mapped sample exceeds width"));
                }
            }
        }
        done += BLOCK.min(remaining);
    }
    reader.finish()
}
