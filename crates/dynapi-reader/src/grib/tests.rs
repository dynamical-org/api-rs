use super::*;
mod fixtures;
use fixtures::*;

#[test]
fn simple_values_and_nonzero_constant_rejection() {
    let bytes = simple(2, 2, 2, 10.0, vec![0b00011011]);
    let decoded = decode(&bytes, profile(&[1, 1, 2, 2])).unwrap();
    assert_eq!(decoded.shape, [1, 1, 2, 2]);
    assert_eq!(decoded.values, [10.0, 11.0, 12.0, 13.0]);
    for reference in [-7.5, 0.0, 7.5] {
        for (binary, decimal) in [(0_u16, 0_u16), (0x8001, 1), (1, 0x8001)] {
            let mut bytes = simple(2, 2, 0, reference, vec![]);
            let p = section_range(&bytes, 5).start;
            bytes[p + 15..p + 17].copy_from_slice(&binary.to_be_bytes());
            bytes[p + 17..p + 19].copy_from_slice(&decimal.to_be_bytes());
            assert_eq!(
                decode(&bytes, profile(&[2, 2])).unwrap_err(),
                Error::Constant
            );
            u32_at(&mut bytes, p + 5, 2);
            let mut bitmap = section(6, 7);
            bitmap[6] = 0b10100000;
            replace(&mut bytes, 6, bitmap);
            assert_eq!(
                decode(&bytes, profile(&[2, 2])).unwrap_err(),
                Error::Constant
            );
        }
    }
}

#[test]
fn first_and_second_order_signed_scales() {
    let groups = [group(0, 0, &[0; 6])];
    for octets in [1, 2, 3, 4] {
        let bytes = spatial(2, 3, &groups, [3, 4, 2], 2, octets, 0);
        assert_eq!(
            decode(&bytes, profile(&[2, 3])).unwrap().values,
            [3., 4., 7., 12., 19., 28.]
        );
        let bytes = spatial(2, 3, &groups, [-3, 0, 2], 1, octets, 0);
        assert_eq!(
            decode(&bytes, profile(&[2, 3])).unwrap().values,
            [-3., -1., 1., 3., 5., 7.]
        );
    }
    let mut bytes = simple(2, 2, 2, 10., vec![0b00011011]);
    let p = section_range(&bytes, 5).start;
    bytes[p + 15..p + 17].copy_from_slice(&0x8001_u16.to_be_bytes());
    bytes[p + 17..p + 19].copy_from_slice(&0x8001_u16.to_be_bytes());
    assert_eq!(
        decode(&bytes, profile(&[2, 2])).unwrap().values,
        [100., 105., 110., 115.]
    );
    let mut bytes = spatial(2, 3, &groups, [3, 4, 2], 2, 1, 0);
    let p = section_range(&bytes, 5).start;
    bytes[p + 11..p + 15].copy_from_slice(&10_f32.to_be_bytes());
    bytes[p + 15..p + 17].copy_from_slice(&1_u16.to_be_bytes());
    assert_eq!(
        decode(&bytes, profile(&[2, 3])).unwrap().values,
        [16., 18., 24., 34., 48., 66.]
    );
}

#[test]
fn primary_secondary_missing_skip_reconstruction_state() {
    // Missing raw codes do not advance the original value sequence [3,4,7,12].
    let groups = [group(0, 3, &[0, 7, 0, 6, 0, 0])];
    let bytes = spatial(2, 3, &groups, [3, 4, 2], 2, 1, 2);
    let values = decode(&bytes, profile(&[2, 3])).unwrap().values;
    assert_eq!(
        [values[0], values[2], values[4], values[5]],
        [3., 4., 7., 12.]
    );
    assert!(values[1].is_nan() && values[3].is_nan());
    let bytes = spatial(
        2,
        3,
        &[group(15, 0, &[0; 3]), group(0, 0, &[0; 3])],
        [3, 4, 2],
        2,
        1,
        1,
    );
    let values = decode(&bytes, profile(&[2, 3])).unwrap().values;
    assert!(values[..3].iter().all(|v| v.is_nan()));
    assert_eq!(&values[3..], &[3., 4., 7.]);
    let bytes = spatial(2, 3, &[group(15, 0, &[0; 6])], [3, 4, 2], 2, 1, 1);
    assert!(
        decode(&bytes, profile(&[2, 3]))
            .unwrap()
            .values
            .iter()
            .all(|v| v.is_nan())
    );
}

#[test]
fn bitmap_expansion_padding_and_empty_stream_rejection() {
    let mut bytes = simple(2, 3, 2, 0., vec![0b00011000]);
    let p = section_range(&bytes, 5).start;
    u32_at(&mut bytes, p + 5, 3);
    let mut bitmap = section(6, 7);
    bitmap[5] = 0;
    bitmap[6] = 0b10101000;
    replace(&mut bytes, 6, bitmap);
    let values = decode(&bytes, profile(&[2, 3])).unwrap().values;
    assert_eq!([values[0], values[2], values[4]], [0., 1., 2.]);
    assert!(values[1].is_nan() && values[3].is_nan() && values[5].is_nan());
    let mut bad = bytes.clone();
    let p = section_range(&bad, 6).start;
    bad[p + 6] |= 1;
    assert!(matches!(
        decode(&bad, profile(&[2, 3])),
        Err(Error::Malformed(_))
    ));
    let mut bad = bytes.clone();
    let p = section_range(&bad, 7).start;
    bad[p + 5] |= 1;
    assert!(matches!(
        decode(&bad, profile(&[2, 3])),
        Err(Error::Malformed(_))
    ));
    let p = section_range(&bytes, 5).start;
    u32_at(&mut bytes, p + 5, 0);
    let p = section_range(&bytes, 6).start;
    bytes[p + 6] = 0;
    assert_eq!(
        decode(&bytes, profile(&[2, 3])).unwrap_err(),
        Error::Unsupported("empty bitmap stream")
    );
}

#[test]
fn constant_and_other_templates_rejected_before_upstream() {
    let mut bytes = spatial(2, 3, &[group(0, 0, &[0; 6])], [3, 4, 2], 2, 1, 0);
    let p = section_range(&bytes, 5).start;
    u32_at(&mut bytes, p + 31, 0);
    assert_eq!(
        decode(&bytes, profile(&[2, 3])).unwrap_err(),
        Error::Constant
    );
    u32_at(&mut bytes, p + 31, 1);
    bytes[p + 48] = 0;
    assert_eq!(
        decode(&bytes, profile(&[2, 3])).unwrap_err(),
        Error::Constant
    );
    for template in [2, 40, 41, 42] {
        bytes[p + 9..p + 11].copy_from_slice(&(template as u16).to_be_bytes());
        assert_eq!(
            decode(&bytes, profile(&[2, 3])).unwrap_err(),
            Error::Unsupported("packing template")
        );
    }
}

#[test]
fn reorder_global_roll_north_up_and_lambert_rows() {
    let mut bytes = simple(2, 4, 3, 0., vec![0b00000101, 0b00111001, 0b01110111]);
    let p = section_range(&bytes, 3).start;
    bytes[p + 71] = 64;
    let mut request = profile(&[2, 4]);
    request.adjust_longitude_range = true;
    request.north_up = true;
    assert_eq!(
        decode(&bytes, request).unwrap().values,
        [6., 7., 4., 5., 2., 3., 0., 1.]
    );
    let mut lambert = section(3, 81);
    lambert[13] = 30;
    u32_at(&mut lambert, 6, 8);
    u32_at(&mut lambert, 30, 4);
    u32_at(&mut lambert, 34, 2);
    u32_at(&mut lambert, 59, 3000000);
    lambert[64] = 64;
    replace(&mut bytes, 3, lambert);
    assert_eq!(
        decode(&bytes, request).unwrap().values,
        [4., 5., 6., 7., 0., 1., 2., 3.]
    );
    let p = section_range(&bytes, 3).start;
    bytes[p + 64] = 32;
    assert!(matches!(
        decode(&bytes, request),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn f64_scale_offset_defaults_precision_and_ieee_policy() {
    use serde_json::json;
    let identity = ScaleOffset::parse(&json!({"name":"scale_offset"})).unwrap();
    let mut data = [273.15, 9.80665, -0.0, f64::NAN, f64::INFINITY];
    identity.apply(&mut data);
    assert_eq!(data[0].to_bits(), 273.15_f64.to_bits());
    assert_eq!(data[2].to_bits(), 0_f64.to_bits());
    assert!(data[3].is_nan() && data[4].is_infinite());
    let mut data = [273.15];
    ScaleOffset::new(1., -273.15).unwrap().apply(&mut data);
    assert_eq!(data, [0.]);
    let mut data = [9.80665];
    ScaleOffset::new(9.80665, 0.).unwrap().apply(&mut data);
    assert_eq!(data, [1.]);
    let mut data = [-0.0];
    ScaleOffset::new(1., -0.).unwrap().apply(&mut data);
    assert_eq!(data[0].to_bits(), (-0_f64).to_bits());
    for scale in [0., -0., f64::NAN, f64::INFINITY] {
        assert!(ScaleOffset::new(scale, 0.).is_err());
    }
    assert!(ScaleOffset::new(1., f64::NEG_INFINITY).is_err());
    for meta in [
        json!({"name":"scale_offset","configuration":{"scale":null}}),
        json!({"name":"scale_offset","configuration":{"scale":0}}),
        json!({"name":"scale_offset","configuration":{"unknown":1}}),
    ] {
        assert!(ScaleOffset::parse(&meta).is_err());
    }
    let mut request = profile(&[2, 2]);
    request.scale_offset = Some(ScaleOffset::new(2., 3.).unwrap());
    assert_eq!(
        decode(&simple(2, 2, 2, 10., vec![0b00011011]), request)
            .unwrap()
            .values,
        [8., 8.5, 9., 9.5]
    );
}

#[test]
fn malformed_mutations_never_repair_counts_or_enter_unchecked_parsers() {
    let bytes = simple(2, 2, 2, 10., vec![0b00011011]);
    for length in 0..bytes.len() {
        assert!(decode(&bytes[..length], profile(&[2, 2])).is_err());
    }
    for number in [1, 3, 4, 5, 6, 7] {
        let start = section_range(&bytes, number).start;
        for length in [0, 4, u32::MAX] {
            let mut bad = bytes.clone();
            u32_at(&mut bad, start, length);
            assert!(decode(&bad, profile(&[2, 2])).is_err());
        }
    }
    let mut bad = bytes.clone();
    let p = section_range(&bad, 5).start;
    u32_at(&mut bad, p + 5, 3);
    assert!(matches!(
        decode(&bad, profile(&[2, 2])),
        Err(Error::Malformed(_))
    ));
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(decode(&bad, profile(&[2, 2])).is_err());
    assert!(decode(&bytes, profile(&[3, 2])).is_err());
    assert!(decode(&bytes, profile(&[2, 2, 2])).is_err());
    assert!(matches!(
        decode(&bytes, profile(&[MAX_DECODED_BYTES / 8 + 1, 1])),
        Err(Error::SizeLimit)
    ));
}

#[test]
fn exact_i32_u32_and_f32_implementation_hazards() {
    let bytes = spatial(
        1,
        3,
        &[group(0, 0, &[0; 3])],
        [1_100_000_000, 1_100_000_000, 0],
        2,
        4,
        0,
    );
    assert_eq!(
        decode(&bytes, profile(&[1, 3])).unwrap_err(),
        Error::Arithmetic
    );
    let mut bytes = spatial(2, 3, &[group(0, 0, &[0; 6])], [3, 4, 2], 2, 1, 0);
    let p = section_range(&bytes, 5).start;
    bytes[p + 19] = 32;
    assert!(matches!(
        decode(&bytes, profile(&[2, 3])),
        Err(Error::Unsupported(_))
    ));
    bytes[p + 19] = 4;
    u32_at(&mut bytes, p + 42, u32::MAX);
    assert!(decode(&bytes, profile(&[2, 3])).is_err());
    assert_eq!(preflight::aligned_for_test(1 << 24, 1).unwrap(), 1 << 24);
    assert!(matches!(
        preflight::aligned_for_test((1 << 24) + 1, 1),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn deterministic_byte_mutations_do_not_panic() {
    let specimens = [
        simple(2, 2, 2, 10., vec![0b00011011]),
        spatial(
            2,
            3,
            &[group(0, 3, &[0, 7, 0, 6, 0, 0])],
            [3, 4, 2],
            2,
            1,
            2,
        ),
    ];
    for (index, bytes) in specimens.iter().enumerate() {
        let shape = if index == 0 { [2, 2] } else { [2, 3] };
        for position in 0..bytes.len() {
            for value in [0, 128, 255] {
                let mut mutated = bytes.clone();
                mutated[position] = value;
                // Catching panic here is a test oracle, never runtime admission.
                assert!(
                    std::panic::catch_unwind(|| decode(&mutated, profile(&shape))).is_ok(),
                    "byte {position}={value}"
                );
            }
        }
    }
}

fn allocation_sample(
    bytes: &[u8],
    shape: &[usize],
    flags: bool,
) -> crate::allocation_tests::Counts {
    use crate::allocation_tests::Measurement;
    let measurement = Measurement::start();
    // Retain a complete, owned input throughout decode and final conversion;
    // this allocation is inside the measurement, not an omitted borrowed F term.
    let encoded = bytes.to_vec();
    let input_capacity = encoded.capacity();
    let mut request = profile(shape);
    request.north_up = flags;
    request.adjust_longitude_range = flags;
    request.scale_offset = Some(ScaleOffset::new(9.80665, -273.15).unwrap());
    let result = decode(&encoded, request).unwrap();
    let result = result.into_le_bytes();
    assert_eq!(result.len(), shape.iter().product::<usize>() * 8);
    drop(result);
    drop(encoded);
    let counts = measurement.counts();
    assert_eq!(measurement.bytes(), counts.total);
    let charge = charges(bytes.len() as u64, request).unwrap();
    assert!(input_capacity <= charge.fetch);
    assert!(counts.peak <= input_capacity + charge.decode);
    counts
}

#[test]
fn peak_live_allocations_cover_input_decode_bitmap_reorder_scale_and_conversion() {
    let mut bytes = simple(256, 256, 8, 0., vec![1; 256 * 256]);
    let p = section_range(&bytes, 3).start;
    bytes[p + 71] = 64;
    let measured = allocation_sample(&bytes, &[1, 1, 256, 256], true);
    let charge = charges(bytes.len() as u64, profile(&[256, 256])).unwrap();
    assert!(!measured.foreign_deallocation);
    assert_eq!(measured.live, 0);
    assert!(measured.peak <= charge.fetch + charge.decode);
    eprintln!(
        "GRIB simple E={} D={} peak={} cumulative={} F={} X={}",
        bytes.len(),
        256 * 256 * 8,
        measured.peak,
        measured.total,
        charge.fetch,
        charge.decode
    );
    let bytes = spatial(
        256,
        256,
        &[Group {
            reference: 15,
            width: 0,
            raw: vec![0; 256 * 256],
        }],
        [3, 4, 2],
        2,
        1,
        1,
    );
    let measured = allocation_sample(&bytes, &[256, 256], true);
    let charge = charges(bytes.len() as u64, profile(&[256, 256])).unwrap();
    assert!(!measured.foreign_deallocation);
    assert_eq!(measured.live, 0);
    assert!(measured.peak <= charge.fetch + charge.decode);
    eprintln!(
        "GRIB all-missing E={} D={} peak={} cumulative={} F={} X={}",
        bytes.len(),
        256 * 256 * 8,
        measured.peak,
        measured.total,
        charge.fetch,
        charge.decode
    );
}

#[test]
fn maximum_qualified_encoded_and_decoded_boundary_peak() {
    // 7,340,032 f64 values = 56 MiB. An optional local-use section brings
    // the complete input to 32 MiB without affecting the packed data recipe.
    let (ny, nx) = (1792, 4096);
    let base = simple(ny, nx, 8, 0., vec![1; ny * nx]);
    let pad = MAX_ENCODED_BYTES - base.len();
    let mut bytes = base;
    let local = section(2, pad);
    let insertion = section_range(&bytes, 3).start;
    bytes.splice(insertion..insertion, local);
    let length = bytes.len() as u64;
    bytes[8..16].copy_from_slice(&length.to_be_bytes());
    assert_eq!(bytes.len(), MAX_ENCODED_BYTES);
    let measured = allocation_sample(&bytes, &[ny, nx], true);
    let charge = charges(bytes.len() as u64, profile(&[ny, nx])).unwrap();
    assert!(!measured.foreign_deallocation);
    assert_eq!(measured.live, 0);
    assert!(measured.peak <= charge.fetch + charge.decode);
    assert_eq!(charge.fetch, 130 * 1024 * 1024);
    assert_eq!((256 * 1024 * 1024) / charge.fetch, 1);
    assert!(charges(MAX_ENCODED_BYTES as u64 + 1, profile(&[1, 1])).is_err());
    assert!(charges(1, profile(&[MAX_DECODED_BYTES / 8 + 1, 1])).is_err());
    eprintln!(
        "GRIB max E={} D={} peak={} cumulative={} F={} X={}",
        bytes.len(),
        MAX_DECODED_BYTES,
        measured.peak,
        measured.total,
        charge.fetch,
        charge.decode
    );
}

#[test]
fn simultaneous_independent_calls_and_bounded_rejection_allocations() {
    let bytes = std::sync::Arc::new(simple(128, 128, 8, 0., vec![1; 128 * 128]));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let bytes = bytes.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                allocation_sample(&bytes, &[128, 128], true)
            })
        })
        .collect();
    for thread in threads {
        let counts = thread.join().unwrap();
        assert!(!counts.foreign_deallocation);
        assert_eq!(counts.live, 0);
        let charge = charges(bytes.len() as u64, profile(&[128, 128])).unwrap();
        assert!(counts.peak <= charge.fetch + charge.decode);
    }
    use crate::allocation_tests::Measurement;
    let measurement = Measurement::start();
    assert!(decode(&bytes[..20], profile(&[128, 128])).is_err());
    let counts = measurement.counts();
    assert_eq!(counts.peak, 0);
    assert_eq!(counts.live, 0);
}

#[test]
fn trailing_singleton_axes_and_non_global_longitudes() {
    let mut request = profile(&[1, 2, 2, 1]);
    request.spatial_axes = [1, 2];
    let mut bytes = simple(2, 2, 2, 0., vec![0b00011011]);
    assert_eq!(decode(&bytes, request).unwrap().values, [0., 1., 2., 3.]);
    request.adjust_longitude_range = true;
    let p = section_range(&bytes, 3).start;
    u32_at(&mut bytes, p + 63, 1_000_000);
    assert_eq!(decode(&bytes, request).unwrap().values, [0., 1., 2., 3.]);
}

#[test]
fn peak_live_bitmap_and_multi_group_profiles() {
    let mut bytes = simple(256, 256, 8, 0., vec![1; 256 * 256 / 2]);
    let p = section_range(&bytes, 5).start;
    u32_at(&mut bytes, p + 5, 256 * 256 / 2);
    let mut bitmap = section(6, 6 + 256 * 256 / 8);
    bitmap[6..].fill(0b10101010);
    replace(&mut bytes, 6, bitmap);
    let counts = allocation_sample(&bytes, &[256, 256], true);
    let charge = charges(bytes.len() as u64, profile(&[256, 256])).unwrap();
    assert!(!counts.foreign_deallocation);
    assert_eq!(counts.live, 0);
    assert!(counts.peak <= bytes.len() + charge.decode);
    eprintln!(
        "GRIB bitmap E={} D={} peak={} cumulative={} F={} X={}",
        bytes.len(),
        256 * 256 * 8,
        counts.peak,
        counts.total,
        charge.fetch,
        charge.decode
    );
    let groups: Vec<_> = (0..4096).map(|_| group(0, 0, &[0; 16])).collect();
    let bytes = spatial(256, 256, &groups, [0, 0, 0], 2, 1, 0);
    let counts = allocation_sample(&bytes, &[256, 256], true);
    let charge = charges(bytes.len() as u64, profile(&[256, 256])).unwrap();
    assert!(!counts.foreign_deallocation);
    assert_eq!(counts.live, 0);
    assert!(counts.peak <= bytes.len() + charge.decode);
    eprintln!(
        "GRIB groups E={} D={} peak={} cumulative={} F={} X={}",
        bytes.len(),
        256 * 256 * 8,
        counts.peak,
        counts.total,
        charge.fetch,
        charge.decode
    );
}

#[test]
fn secondary_missing_constant_groups_and_spatial_bitmap() {
    let bytes = spatial(
        2,
        3,
        &[group(14, 0, &[0; 3]), group(0, 0, &[0; 3])],
        [3, 4, 2],
        2,
        1,
        2,
    );
    let values = decode(&bytes, profile(&[2, 3])).unwrap().values;
    assert!(values[..3].iter().all(|v| v.is_nan()));
    assert_eq!(&values[3..], &[3., 4., 7.]);
    let mut bytes = spatial(2, 3, &[group(0, 0, &[0; 3])], [3, 4, 2], 2, 1, 0);
    let p = section_range(&bytes, 5).start;
    u32_at(&mut bytes, p + 5, 3);
    let mut mask = section(6, 7);
    mask[5] = 0;
    mask[6] = 0b10101000;
    replace(&mut bytes, 6, mask);
    let values = decode(&bytes, profile(&[2, 3])).unwrap().values;
    assert_eq!([values[0], values[2], values[4]], [3., 4., 7.]);
    assert!(values[1].is_nan() && values[3].is_nan() && values[5].is_nan());
}

#[test]
fn checked_group_length_and_stored_addition_overflows() {
    let mut bytes = spatial(
        2,
        3,
        &[group(0, 0, &[0; 3]), group(0, 0, &[0; 3])],
        [3, 4, 2],
        2,
        1,
        0,
    );
    let p = section_range(&bytes, 5).start;
    bytes[p + 46] = 31;
    bytes[p + 41] = 255;
    let mut data = section(7, 5 + 3 + 1 + 2 + 8);
    data[5..8].copy_from_slice(&[3, 4, 2]);
    data[11..15].copy_from_slice(&[255, 255, 255, 254]);
    replace(&mut bytes, 7, data);
    assert_eq!(
        decode(&bytes, profile(&[2, 3])).unwrap_err(),
        Error::Arithmetic
    );
    // A reference and a raw packed value each fit positive i32 but their sum
    // overflows before the pinned decoder substitutes the first descriptor.
    let mut bytes = spatial(1, 2, &[group(0, 31, &[0, 0])], [3, 4, 2], 2, 1, 0);
    let p = section_range(&bytes, 5).start;
    bytes[p + 19] = 31;
    let mut data = section(7, 5 + 3 + 4 + 1 + 2 + 8);
    data[5..8].copy_from_slice(&[3, 4, 2]);
    data[8..12].copy_from_slice(&[255, 255, 255, 254]);
    data[12] = 0b11111000;
    data[13..15].copy_from_slice(&2_u16.to_be_bytes());
    data[15..19].copy_from_slice(&[255, 255, 255, 254]);
    replace(&mut bytes, 7, data);
    assert_eq!(
        decode(&bytes, profile(&[1, 2])).unwrap_err(),
        Error::Arithmetic
    );
}

#[test]
fn zero_group_reference_width_is_not_a_zero_group_constant() {
    let mut bytes = spatial(2, 3, &[group(0, 0, &[0; 6])], [0, 0, 0], 2, 1, 0);
    let p = section_range(&bytes, 5).start;
    bytes[p + 11..p + 15].copy_from_slice(&7.5_f32.to_be_bytes());
    bytes[p + 19] = 0;
    bytes[p + 36] = 0;
    bytes[p + 46] = 0;
    replace(&mut bytes, 7, section(7, 8));
    assert_eq!(decode(&bytes, profile(&[2, 3])).unwrap().values, [7.5; 6]);
}
