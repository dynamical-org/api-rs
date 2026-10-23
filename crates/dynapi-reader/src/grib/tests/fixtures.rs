//! Rust-owned GRIB recipes; values are specified in the calling tests.
use crate::grib::Profile;

pub(super) fn section(number: u8, length: usize) -> Vec<u8> {
    let mut bytes = vec![0; length];
    bytes[..4].copy_from_slice(&(length as u32).to_be_bytes());
    bytes[4] = number;
    bytes
}

pub(super) fn u32_at(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

pub(super) fn assemble(
    grid: Vec<u8>,
    packing: Vec<u8>,
    bitmap: Vec<u8>,
    payload: Vec<u8>,
) -> Vec<u8> {
    let mut bytes = b"GRIB\0\0\0\x02\0\0\0\0\0\0\0\0".to_vec();
    bytes.extend(section(1, 21));
    bytes.extend(grid);
    bytes.extend(section(4, 34));
    bytes.extend(packing);
    bytes.extend(bitmap);
    let mut data = section(7, 5 + payload.len());
    data[5..].copy_from_slice(&payload);
    bytes.extend(data);
    bytes.extend(b"7777");
    let length = bytes.len() as u64;
    bytes[8..16].copy_from_slice(&length.to_be_bytes());
    bytes
}

pub(super) fn grid(ny: usize, nx: usize, scan: u8) -> Vec<u8> {
    let mut bytes = section(3, 72);
    u32_at(&mut bytes, 6, (ny * nx) as u32);
    bytes[14] = 6;
    u32_at(&mut bytes, 30, nx as u32);
    u32_at(&mut bytes, 34, ny as u32);
    u32_at(&mut bytes, 42, u32::MAX);
    u32_at(&mut bytes, 63, (360_000_000 / nx) as u32);
    u32_at(&mut bytes, 67, 1_000_000);
    bytes[71] = scan;
    bytes
}

pub(super) fn lambert(ny: usize, nx: usize, scan: u8) -> Vec<u8> {
    let mut bytes = section(3, 81);
    bytes[13] = 30;
    u32_at(&mut bytes, 6, (ny * nx) as u32);
    u32_at(&mut bytes, 30, nx as u32);
    u32_at(&mut bytes, 34, ny as u32);
    u32_at(&mut bytes, 59, 3_000_000);
    bytes[64] = scan;
    bytes
}

pub(super) fn bitmap(mask: &[bool]) -> Vec<u8> {
    let mut bytes = section(6, 6 + mask.len().div_ceil(8));
    for (index, &present) in mask.iter().enumerate() {
        if present {
            bytes[6 + index / 8] |= 1 << (7 - index % 8);
        }
    }
    bytes
}

pub(super) fn simple(ny: usize, nx: usize, bits: u8, reference: f32, payload: Vec<u8>) -> Vec<u8> {
    let mut packing = section(5, 21);
    u32_at(&mut packing, 5, (nx * ny) as u32);
    packing[11..15].copy_from_slice(&reference.to_be_bytes());
    packing[19] = bits;
    let mut bitmap = section(6, 6);
    bitmap[5] = 255;
    assemble(grid(ny, nx, 0), packing, bitmap, payload)
}

pub(super) fn profile(shape: &[usize]) -> Profile<'_> {
    Profile {
        shape,
        spatial_axes: [shape.len() - 2, shape.len() - 1],
        adjust_longitude_range: false,
        north_up: false,
        scale_offset: None,
    }
}

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    length: usize,
}
impl Bits {
    fn push(&mut self, value: u32, width: u8) {
        for bit in (0..width).rev() {
            if self.length.is_multiple_of(8) {
                self.bytes.push(0);
            }
            self.bytes[self.length / 8] |= (((value >> bit) & 1) as u8) << (7 - self.length % 8);
            self.length += 1;
        }
    }
    fn align(&mut self) {
        self.length = self.length.div_ceil(8) * 8;
    }
}
pub(super) struct Group {
    pub(super) reference: u32,
    pub(super) width: u8,
    pub(super) raw: Vec<u32>,
}
pub(super) fn group(reference: u32, width: u8, raw: &[u32]) -> Group {
    Group {
        reference,
        width,
        raw: raw.to_vec(),
    }
}
pub(super) fn signed_bytes(value: i32, octets: usize) -> Vec<u8> {
    let raw = value.unsigned_abs() | if value < 0 { 1 << (octets * 8 - 1) } else { 0 };
    raw.to_be_bytes()[4 - octets..].to_vec()
}
// Recipe: section 7 stores group references, widths, lengths, then raw group
// values. First/second original values and minimum difference are descriptors.
pub(super) fn spatial(
    ny: usize,
    nx: usize,
    groups: &[Group],
    descriptors: [i32; 3],
    order: u8,
    octets: u8,
    missing: u8,
) -> Vec<u8> {
    let mut packing = section(5, 49);
    u32_at(&mut packing, 5, (ny * nx) as u32);
    packing[10] = 3;
    packing[19] = 4;
    packing[21] = 1;
    packing[22] = missing;
    u32_at(&mut packing, 31, groups.len() as u32);
    packing[36] = 5;
    packing[41] = 1;
    u32_at(
        &mut packing,
        42,
        groups.last().map_or(0, |g| g.raw.len()) as u32,
    );
    packing[46] = 16;
    packing[47] = order;
    packing[48] = octets;
    let mut payload = Vec::new();
    payload.extend(signed_bytes(descriptors[0], octets as usize));
    if order == 2 {
        payload.extend(signed_bytes(descriptors[1], octets as usize));
    }
    payload.extend(signed_bytes(descriptors[2], octets as usize));
    let mut bits = Bits::default();
    for g in groups {
        bits.push(g.reference, 4);
    }
    bits.align();
    for g in groups {
        bits.push(g.width as u32, 5);
    }
    bits.align();
    for g in groups {
        bits.push(g.raw.len() as u32, 16);
    }
    bits.align();
    for g in groups {
        for raw in &g.raw {
            bits.push(*raw, g.width);
        }
    }
    payload.extend(bits.bytes);
    let mut bitmap = section(6, 6);
    bitmap[5] = 255;
    assemble(grid(ny, nx, 0), packing, bitmap, payload)
}
pub(super) fn section_range(bytes: &[u8], number: u8) -> std::ops::Range<usize> {
    let mut p = 16;
    while p < bytes.len() - 4 {
        let n = u32::from_be_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        if bytes[p + 4] == number {
            return p..p + n;
        }
        p += n;
    }
    panic!("fixture section absent")
}
pub(super) fn replace(bytes: &mut Vec<u8>, number: u8, section: Vec<u8>) {
    let range = section_range(bytes, number);
    bytes.splice(range, section);
    let n = bytes.len() as u64;
    bytes[8..16].copy_from_slice(&n.to_be_bytes());
}
