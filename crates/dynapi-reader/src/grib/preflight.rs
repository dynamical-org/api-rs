//! Constant-space validation of the exact arithmetic used by gribberish 1.5.0.
use super::{Error, MAX_ENCODED_BYTES, Profile};

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub(super) struct Validated {
    pub nx: usize,
    pub ny: usize,
    pub roll: Option<usize>,
    pub flip: bool,
}

fn malformed<T>(message: &'static str) -> Result<T> {
    Err(Error::Malformed(message))
}
fn u16_at(bytes: &[u8], start: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        bytes
            .get(start..start + 2)
            .ok_or(Error::Malformed("short u16"))?
            .try_into()
            .unwrap(),
    ))
}
fn u32_at(bytes: &[u8], start: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .get(start..start + 4)
            .ok_or(Error::Malformed("short u32"))?
            .try_into()
            .unwrap(),
    ))
}
fn signed(value: u32, width: usize) -> i32 {
    let sign = 1u32 << (width - 1);
    let magnitude = (value & !sign) as i32;
    if value & sign == 0 {
        magnitude
    } else {
        -magnitude
    }
}
fn bits(bytes: &[u8], start: usize, width: u8) -> Result<u32> {
    if width > 31 {
        return Err(Error::Unsupported("bit width above 31"));
    }
    let end = start.checked_add(width as usize).ok_or(Error::Arithmetic)?;
    if end > bytes.len() * 8 {
        return malformed("packed bit range");
    }
    let mut value = 0;
    for index in start..end {
        value = (value << 1) | u32::from((bytes[index / 8] >> (7 - index % 8)) & 1);
    }
    Ok(value)
}
fn descriptor(bytes: &[u8], start: usize, octets: usize) -> Result<i32> {
    let slice = bytes
        .get(start..start + octets)
        .ok_or(Error::Malformed("short spatial descriptors"))?;
    let mut value = 0u32;
    for &byte in slice {
        value = (value << 8) | u32::from(byte);
    }
    Ok(signed(value, octets * 8))
}
fn zeros(bytes: &[u8], start: usize, end: usize) -> Result<()> {
    if end > bytes.len() * 8 || start > end {
        return malformed("padding bounds");
    }
    for index in start..end {
        if bytes[index / 8] & (1 << (7 - index % 8)) != 0 {
            return malformed("nonzero padding bits");
        }
    }
    Ok(())
}
fn aligned(count: usize, width: u8) -> Result<usize> {
    let size = count.checked_mul(width as usize).ok_or(Error::Arithmetic)?;
    // This bound makes the pinned decoder's f32 byte alignment exact. Larger
    // mathematically valid tables require separate qualification.
    if size > 1 << 24 {
        return Err(Error::Unsupported("f32 group table alignment"));
    }
    Ok(size.div_ceil(8) * 8)
}
fn scaled_parameters(packing: &[u8]) -> Result<()> {
    let reference = f32::from_bits(u32_at(packing, 11)?);
    let binary = signed(u16_at(packing, 15)? as u32, 16);
    let decimal = signed(u16_at(packing, 17)? as u32, 16);
    let b = 2_f64.powi(binary);
    let d = 10_f64.powi(-decimal);
    if !reference.is_finite() || !b.is_finite() || b == 0.0 || !d.is_finite() || d == 0.0 {
        return Err(Error::Unsupported("finite nonzero GRIB scaling factors"));
    }
    if packing[20] > 1 {
        return Err(Error::Unsupported("original field type"));
    }
    Ok(())
}

fn bitmap(section: &[u8], points: usize, packed: usize) -> Result<()> {
    match section.get(5) {
        Some(255) if section.len() == 6 && points == packed => Ok(()),
        Some(0) => {
            if section.len() != 6 + points.div_ceil(8) {
                return malformed("bitmap length");
            }
            zeros(&section[6..], points, points.div_ceil(8) * 8)?;
            let count: usize = section[6..].iter().map(|v| v.count_ones() as usize).sum();
            if count != packed {
                return malformed("bitmap packed count");
            }
            if count == 0 {
                return Err(Error::Unsupported("empty bitmap stream"));
            }
            Ok(())
        }
        Some(255) => malformed("grid and packed counts differ"),
        _ => Err(Error::Unsupported("previous or predefined bitmap")),
    }
}

fn simple(packing: &[u8], data: &[u8], points: usize) -> Result<()> {
    if packing.len() != 21 {
        return malformed("simple template length");
    }
    let width = packing[19];
    if width == 0 {
        return Err(Error::Constant);
    }
    if width > 31 {
        return Err(Error::Unsupported("simple width above 31"));
    }
    let length = points
        .checked_mul(width as usize)
        .ok_or(Error::Arithmetic)?;
    if data.len() != length.div_ceil(8) {
        return malformed("simple packed length");
    }
    zeros(data, length, data.len() * 8)
}

fn spatial(p: &[u8], data: &[u8], points: usize) -> Result<()> {
    if p.len() != 49 {
        return malformed("spatial template length");
    }
    let groups = u32_at(p, 31)? as usize;
    let octets = p[48] as usize;
    if groups == 0 {
        return Err(Error::Constant);
    }
    if octets == 0 {
        return Err(Error::Unsupported("zero spatial descriptor octets"));
    }
    if groups > points {
        return Err(Error::Unsupported("more groups than packed points"));
    }
    if octets > 4 || !matches!(p[47], 1 | 2) || p[21] != 1 || p[22] > 2 {
        return Err(Error::Unsupported("spatial packing profile"));
    }
    let reference_bits = p[19];
    let width_bits = p[36];
    let length_bits = p[46];
    if [reference_bits, width_bits, length_bits]
        .into_iter()
        .any(|v| v > 31)
    {
        return Err(Error::Unsupported("group width above 31"));
    }
    if width_bits == 0 && p[35] != 0 {
        return Err(Error::Unsupported(
            "pinned decoder ignores zero-bit width reference",
        ));
    }
    if length_bits == 0 && groups > 1 {
        return Err(Error::Unsupported(
            "pinned decoder ignores zero-bit length reference",
        ));
    }
    let order = p[47] as usize;
    let d1 = descriptor(data, 0, octets)?;
    let d2 = if order == 2 {
        descriptor(data, octets, octets)?
    } else {
        0
    };
    let minimum = descriptor(data, order * octets, octets)?;
    let refs_start = (order + 1) * octets * 8;
    let widths_start = refs_start + aligned(groups, reference_bits)?;
    let lengths_start = widths_start + aligned(groups, width_bits)?;
    let mut position = lengths_start + aligned(groups, length_bits)?;
    if position > data.len() * 8 {
        return malformed("group tables exceed payload");
    }
    zeros(
        data,
        refs_start + groups * reference_bits as usize,
        widths_start,
    )?;
    zeros(
        data,
        widths_start + groups * width_bits as usize,
        lengths_start,
    )?;
    // The format includes a last length-table entry even though its true value
    // is in section 5; the pinned decoder ignores that entry's bits.
    zeros(
        data,
        lengths_start + groups * length_bits as usize,
        position,
    )?;
    let all_refs = if reference_bits == 0 {
        0
    } else {
        (1u32 << reference_bits) - 1
    };
    let mut count = 0usize;
    let mut defined = 0usize;
    let mut previous = 0i32;
    let mut previous2 = 0i32;
    for group in 0..groups {
        let reference = bits(
            data,
            refs_start + group * reference_bits as usize,
            reference_bits,
        )?;
        let width = if width_bits == 0 {
            0
        } else {
            bits(data, widths_start + group * width_bits as usize, width_bits)?
                .checked_add(u32::from(p[35]))
                .ok_or(Error::Arithmetic)?
        };
        if width > 31 {
            return Err(Error::Unsupported("effective group width above 31"));
        }
        let length = if group + 1 == groups {
            u32_at(p, 42)?
        } else {
            bits(
                data,
                lengths_start + group * length_bits as usize,
                length_bits,
            )?
            .checked_mul(u32::from(p[41]))
            .and_then(|v| v.checked_add(u32_at(p, 37).ok()?))
            .ok_or(Error::Arithmetic)?
        };
        if length == 0 {
            return Err(Error::Unsupported("zero length group"));
        }
        count = count
            .checked_add(length as usize)
            .ok_or(Error::Arithmetic)?;
        if count > points {
            return malformed("group count exceeds packed points");
        }
        // The multiplication is u32 in the pinned decoder, not usize.
        let group_bits = width.checked_mul(length).ok_or(Error::Arithmetic)? as usize;
        let end = position.checked_add(group_bits).ok_or(Error::Arithmetic)?;
        if end > data.len() * 8 {
            return malformed("group data exceeds payload");
        }
        let all_values = if width == 0 { 0 } else { (1u32 << width) - 1 };
        for index in 0..length as usize {
            let raw = if width == 0 {
                reference
            } else {
                bits(data, position + index * width as usize, width as u8)?
            };
            let missing = if width == 0 {
                (p[22] >= 1 && reference == all_refs)
                    || (p[22] == 2 && reference == all_refs.wrapping_sub(1))
            } else {
                (p[22] >= 1 && raw == all_values) || (p[22] == 2 && raw == all_values - 1)
            };
            if missing {
                continue;
            }
            let stored = if width == 0 {
                reference as i32
            } else {
                (raw as i32)
                    .checked_add(reference as i32)
                    .ok_or(Error::Arithmetic)?
            };
            // Check the exact left-associated i32 operations, even when a
            // different algebraic arrangement would have an in-range answer.
            let value = if defined == 0 {
                d1
            } else if order == 2 && defined == 1 {
                d2
            } else if order == 2 {
                previous
                    .checked_mul(2)
                    .and_then(|v| stored.checked_add(v))
                    .and_then(|v| v.checked_sub(previous2))
                    .and_then(|v| v.checked_add(minimum))
                    .ok_or(Error::Arithmetic)?
            } else {
                stored
                    .checked_add(previous)
                    .and_then(|v| v.checked_add(minimum))
                    .ok_or(Error::Arithmetic)?
            };
            previous2 = previous;
            previous = value;
            defined += 1;
        }
        position = end;
    }
    if count != points {
        return malformed("group count differs from packed points");
    }
    if data.len() != position.div_ceil(8) {
        return malformed("trailing packed bytes");
    }
    zeros(data, position, data.len() * 8)
}

fn wrap(value: f64) -> f64 {
    if value >= 180.0 { value - 360.0 } else { value }
}
fn longitude(start: f64, step: f64, index: usize) -> f64 {
    let value = start + index as f64 * step;
    if value >= 360.0 {
        value - 360.0
    } else if value < 0.0 && start >= 0.0 {
        value + 360.0
    } else {
        value
    }
}
fn reorder(
    grid: &[u8],
    template: u16,
    nx: usize,
    ny: usize,
    profile: Profile<'_>,
) -> Result<Validated> {
    let scan = if template == 0 { grid[71] } else { grid[64] };
    if scan & 0x3f != 0 {
        return Err(Error::Unsupported("non-row-major or staggered scan"));
    }
    let mut roll = None;
    let flip;
    if template == 0 {
        if u32_at(grid, 38)? != 0 || !matches!(u32_at(grid, 42)?, 0 | u32::MAX) {
            return Err(Error::Unsupported("non-microdegree angular units"));
        }
        let unit = 10_f64.powf(-6.0);
        let start = u32_at(grid, 50)? as f64 * unit;
        let increment = u32_at(grid, 63)?;
        let latitude_increment = u32_at(grid, 67)?;
        if increment == 0
            || increment == u32::MAX
            || latitude_increment == 0
            || latitude_increment == u32::MAX
            || start > 360.0
        {
            return Err(Error::Unsupported("grid increments/longitude"));
        }
        let step = increment as f64 * unit * if scan & 128 == 0 { 1.0 } else { -1.0 };
        flip = profile.north_up && scan & 64 != 0;
        if profile.adjust_longitude_range && nx >= 2 {
            let dx = longitude(start, step, 1) - longitude(start, step, 0);
            if dx > 0.0 && (dx * nx as f64 - 360.0).abs() < dx / 4.0 {
                let mut min = f64::INFINITY;
                let mut index = 0;
                for column in 0..nx {
                    let value = wrap(longitude(start, step, column));
                    if value < min {
                        min = value;
                        index = column;
                    }
                }
                roll = Some(index);
            }
        }
    } else {
        // Data ordering depends only on the signed y step. Do not construct a
        // Lambert projector (which can panic) or compute coordinates here.
        let step = signed(u32_at(grid, 59)?, 32);
        if step <= 0 {
            return Err(Error::Unsupported("Lambert y spacing"));
        }
        flip = profile.north_up && scan & 64 != 0;
    }
    Ok(Validated { nx, ny, roll, flip })
}

pub(super) fn validate(encoded: &[u8], profile: Profile<'_>) -> Result<Validated> {
    if encoded.len() > MAX_ENCODED_BYTES {
        return Err(Error::SizeLimit);
    }
    let (ny, nx, decoded) = profile.plane()?;
    let points = decoded / 8;
    if encoded.len() < 20 || &encoded[..4] != b"GRIB" {
        return malformed("GRIB header");
    }
    if encoded[7] != 2 {
        return Err(Error::Unsupported("GRIB edition"));
    }
    let declared = u64::from_be_bytes(encoded[8..16].try_into().unwrap());
    if declared != encoded.len() as u64 || &encoded[encoded.len() - 4..] != b"7777" {
        return malformed("message length or end marker");
    }
    let mut cursor = 16usize;
    let mut sections: [Option<&[u8]>; 8] = [None; 8];
    let mut previous = 0;
    while cursor < encoded.len() - 4 {
        let length = u32_at(encoded, cursor)? as usize;
        let end = cursor.checked_add(length).ok_or(Error::Arithmetic)?;
        if length < 5 || end > encoded.len() - 4 {
            return malformed("section range");
        }
        let section = &encoded[cursor..end];
        let number = section[4] as usize;
        if !(1..=7).contains(&number) || number <= previous {
            return malformed("section order or duplicate field");
        }
        sections[number] = Some(section);
        previous = number;
        cursor = end;
    }
    if cursor != encoded.len() - 4 {
        return malformed("section framing");
    }
    for required in [1, 3, 4, 5, 6, 7] {
        if sections[required].is_none() {
            return malformed("missing section");
        }
    }
    if sections[1].unwrap().len() != 21 || sections[4].unwrap().len() < 9 {
        return malformed("identification/product section length");
    }
    let grid = sections[3].unwrap();
    let template = u16_at(grid, 12)?;
    let expected_length = match template {
        0 => 72,
        30 => 81,
        _ => return Err(Error::Unsupported("grid template")),
    };
    if grid.len() != expected_length {
        return malformed("grid template length");
    }
    if grid[5] != 0 || grid[10] != 0 {
        return Err(Error::Unsupported("optional or external grid definition"));
    }
    if u32_at(grid, 6)? as usize != points
        || u32_at(grid, 30)? as usize != nx
        || u32_at(grid, 34)? as usize != ny
    {
        return malformed("attested grid/shape count mismatch");
    }
    let packing = sections[5].unwrap();
    let template_number = u16_at(packing, 9)?;
    if !matches!(template_number, 0 | 3 | 42) {
        return Err(Error::Unsupported("packing template"));
    }
    if packing.len() < 21 {
        return malformed("short packing template");
    }
    scaled_parameters(packing)?;
    let packed = u32_at(packing, 5)? as usize;
    if packed > points {
        return malformed("packed count exceeds grid");
    }
    bitmap(sections[6].unwrap(), points, packed)?;
    let data = &sections[7].unwrap()[5..];
    match template_number {
        0 => simple(packing, data, packed)?,
        3 => spatial(packing, data, packed)?,
        42 => super::ccsds::validate(packing, data, packed, sections[6].unwrap()[5] == 0)?,
        _ => unreachable!(),
    }
    reorder(grid, template, nx, ny, profile)
}

#[cfg(test)]
pub(super) fn aligned_for_test(count: usize, width: u8) -> Result<usize> {
    aligned(count, width)
}
