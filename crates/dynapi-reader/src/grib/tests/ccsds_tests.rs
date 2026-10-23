use super::*;

fn zero_run(bits: &mut Bits, width: u8, reference: Option<u32>, code: usize) {
    bits.push(0, if width == 8 { 3 } else { 4 });
    bits.push(0, 1);
    if let Some(value) = reference {
        bits.push(value, width);
    }
    bits.unary(code);
}

fn split_block(bits: &mut Bits, width: u8, reference: Option<u32>, k: u8, value: u32) {
    bits.push(u32::from(k + 1), if width == 8 { 3 } else { 4 });
    if let Some(value) = reference {
        bits.push(value, width);
    }
    for _ in 0..32 - usize::from(reference.is_some()) {
        bits.unary((value >> k) as usize);
    }
    if k != 0 {
        for _ in 0..32 - usize::from(reference.is_some()) {
            bits.push(value & ((1 << k) - 1), k);
        }
    }
}

#[test]
fn uncompressed_integer_widths_and_f64_scaling() {
    for width in 8..=16 {
        let reference = if width == 8 { 100 } else { 300 };
        let mut mapped = vec![0; 32];
        mapped[..4].copy_from_slice(&[reference, 2, 1, 4]);
        let mut bytes = ccsds(1, 32, width, ccsds_uncompressed(width, &mapped));
        let p = section_range(&bytes, 5).start;
        bytes[p + 11..p + 15].copy_from_slice(&1.25f32.to_be_bytes());
        bytes[p + 15..p + 17].copy_from_slice(&0x8001u16.to_be_bytes());
        bytes[p + 17..p + 19].copy_from_slice(&1u16.to_be_bytes());
        let mut request = profile(&[1, 32]);
        request.scale_offset = Some(ScaleOffset::new(9.80665, -273.15).unwrap());
        let values = decode(&bytes, request).unwrap().values;
        // Mapped differences 2,+1; 1,-1; 4,+2; then zero differences.
        for (index, &value) in values.iter().enumerate() {
            let original = match index {
                0 | 2 => reference,
                1 => reference + 1,
                _ => reference + 2,
            };
            let grib = (f64::from(original) * 0.5 + 1.25) * 0.1;
            assert_eq!(value.to_bits(), (grib / 9.80665 - 273.15).to_bits());
        }
    }
    let mut mapped = vec![0; 32];
    mapped[..3].copy_from_slice(&[65535, 65535, 65535]);
    let bytes = ccsds(1, 32, 16, ccsds_uncompressed(16, &mapped));
    let values = decode(&bytes, profile(&[1, 32])).unwrap().values;
    assert_eq!(&values[..3], &[65535., 0., 65535.]);
}

#[test]
fn split_fast_incremental_and_second_extension_values() {
    // A short block takes the incremental path; 33 blocks also exercise fast
    // Rice splits and their remainder reads, ending with an uncompressed tail.
    for blocks in [1, 32] {
        let mut bits = Bits::default();
        for block in 0..blocks {
            split_block(
                &mut bits,
                16,
                (block == 0).then_some(300),
                (block % 14) as u8,
                2,
            );
        }
        if blocks > 1 {
            bits.push(15, 4);
            for _ in 0..32 {
                bits.push(0, 16);
            }
        }
        let payload = bits.finish();
        let points = if blocks == 1 { 32 } else { (blocks + 1) * 32 };
        let values = decode(&ccsds(1, points, 16, payload), profile(&[1, points]))
            .unwrap()
            .values;
        for (index, value) in values.iter().enumerate() {
            assert_eq!(*value, 300. + index.min(blocks * 32 - 1) as f64);
        }
    }
    let mut bits = Bits::default();
    bits.push(0, 3);
    bits.push(1, 1);
    bits.push(10, 8);
    for _ in 0..16 {
        bits.unary(0);
    }
    let bytes = ccsds(1, 32, 8, bits.finish());
    assert_eq!(decode(&bytes, profile(&[1, 32])).unwrap().values, [10.; 32]);
}

#[test]
fn zero_runs_reset_references_and_ros_handles_final_short_segment() {
    let mut bits = Bits::default();
    zero_run(&mut bits, 9, Some(20), 4); // ROS: first 64 blocks.
    zero_run(&mut bits, 9, None, 4); // ROS: remaining 64 blocks of RSI.
    zero_run(&mut bits, 9, Some(40), 4); // Final sequence segment: only one block.
    let bytes = ccsds(1, 4128, 9, bits.finish());
    let values = decode(&bytes, profile(&[1, 4128])).unwrap().values;
    assert_eq!(&values[..4096], &[20.; 4096]);
    assert_eq!(&values[4096..], &[40.; 32]);
}

#[test]
fn bitmap_order_and_scale_preserve_missing_values() {
    let mut mapped = vec![2; 32];
    mapped[0] = 300;
    let mut bytes = ccsds(2, 32, 9, ccsds_uncompressed(9, &mapped));
    let p = section_range(&bytes, 5).start;
    u32_at(&mut bytes, p + 5, 32);
    replace(
        &mut bytes,
        6,
        bitmap(&(0..64).map(|i| i % 2 == 0).collect::<Vec<_>>()),
    );
    let p = section_range(&bytes, 3).start;
    bytes[p + 71] = 64;
    let mut request = profile(&[2, 32]);
    request.north_up = true;
    request.adjust_longitude_range = true;
    request.scale_offset = Some(ScaleOffset::new(2., 3.).unwrap());
    let values = decode(&bytes, request).unwrap().values;
    for y in 0..2 {
        for x in 0..32 {
            let source = (1 - y) * 32 + (x + 16) % 32;
            if source % 2 == 0 {
                assert_eq!(values[y * 32 + x], (300. + (source / 2) as f64) / 2. + 3.);
            } else {
                assert!(values[y * 32 + x].is_nan());
            }
        }
    }
}

#[test]
fn zero_bit_qualification_does_not_admit_wrong_nonzero_constants_or_masks() {
    let bytes = ccsds(2, 3, 0, vec![]);
    assert_eq!(decode(&bytes, profile(&[2, 3])).unwrap().values, [0.; 6]);
    for reference in [-7.5f32, 7.5, -0.0] {
        let mut bad = bytes.clone();
        let p = section_range(&bad, 5).start;
        bad[p + 11..p + 15].copy_from_slice(&reference.to_be_bytes());
        assert_eq!(decode(&bad, profile(&[2, 3])).unwrap_err(), Error::Constant);
    }
    let mut bad = bytes.clone();
    let p = section_range(&bad, 5).start;
    u32_at(&mut bad, p + 5, 3);
    replace(
        &mut bad,
        6,
        bitmap(&[true, false, true, false, true, false]),
    );
    assert_eq!(decode(&bad, profile(&[2, 3])).unwrap_err(), Error::Constant);
    let mut bad = bytes;
    replace(&mut bad, 7, section(7, 6));
    assert_eq!(
        decode(&bad, profile(&[2, 3])).unwrap_err(),
        Error::Malformed("CCSDS zero-bit payload")
    );
}

#[test]
fn header_and_stream_counts_are_rejected_before_decoder_allocation() {
    let bytes = ccsds(1, 32, 9, ccsds_uncompressed(9, &[0; 32]));
    for (offset, value) in [
        (21, 0),
        (21, 15),
        (22, 0),
        (22, 16),
        (22, 255),
        (23, 255),
        (24, 0),
        (24, 127),
        (24, 129),
    ] {
        let mut bad = bytes.clone();
        let p = section_range(&bad, 5).start;
        bad[p + offset] = value;
        let measurement = crate::allocation_tests::Measurement::start();
        assert_eq!(
            decode(&bad, profile(&[1, 32])).unwrap_err(),
            Error::Unsupported("CCSDS options14 block32 RSI128")
        );
        assert_eq!(measurement.counts().total, 0);
    }
    for points in [16, 64] {
        let bad = ccsds(1, points, 9, ccsds_uncompressed(9, &[0; 32]));
        assert!(decode(&bad, profile(&[1, points])).is_err());
    }
    let bad = ccsds(1, 32, 9, ccsds_uncompressed(9, &[0; 64]));
    assert_eq!(
        decode(&bad, profile(&[1, 32])).unwrap_err(),
        Error::Malformed("CCSDS trailing bytes")
    );
    for width in [1, 7, 17, 24, 25, 32, 255] {
        let bad = ccsds(1, 32, width, vec![]);
        assert_eq!(
            decode(&bad, profile(&[1, 32])).unwrap_err(),
            Error::Unsupported("CCSDS integer width 8 through 16")
        );
    }
}

#[test]
fn entropy_codes_cannot_cross_rsi_or_exceed_value_widths() {
    let mut bits = Bits::default();
    zero_run(&mut bits, 9, Some(0), 129);
    assert_eq!(
        decode(&ccsds(1, 4096, 9, bits.finish()), profile(&[1, 4096])).unwrap_err(),
        Error::Malformed("CCSDS unary value exceeds profile")
    );
    let mut bits = Bits::default();
    zero_run(&mut bits, 9, Some(0), 1); // Two explicit blocks for a one-block field.
    assert_eq!(
        decode(&ccsds(1, 32, 9, bits.finish()), profile(&[1, 32])).unwrap_err(),
        Error::Malformed("CCSDS zero run exceeds count or RSI")
    );
    let mut bits = Bits::default();
    split_block(&mut bits, 9, Some(0), 10, 512);
    assert_eq!(
        decode(&ccsds(1, 32, 9, bits.finish()), profile(&[1, 32])).unwrap_err(),
        Error::Malformed("CCSDS mapped sample exceeds width")
    );
    let mut bits = Bits::default();
    bits.push(0, 4);
    bits.push(1, 1);
    bits.push(0, 9);
    bits.unary(91);
    assert_eq!(
        decode(&ccsds(1, 32, 9, bits.finish()), profile(&[1, 32])).unwrap_err(),
        Error::Unsupported("CCSDS second-extension table")
    );
    let mut bits = Bits::default();
    for _ in 0..120 {
        bits.push(15, 4);
        for _ in 0..32 {
            bits.push(0, 9);
        }
    }
    zero_run(&mut bits, 9, None, 16); // Sixteen blocks, only eight remain in RSI.
    assert_eq!(
        decode(&ccsds(1, 4096, 9, bits.finish()), profile(&[1, 4096])).unwrap_err(),
        Error::Malformed("CCSDS zero run exceeds count or RSI")
    );
}

#[test]
fn pinned_fast_reader_margins_are_explicit_unsupported_profiles() {
    let mut bits = Bits::default();
    split_block(&mut bits, 16, Some(300), 0, 2047);
    assert_eq!(
        decode(&ccsds(1, 32, 16, bits.finish()), profile(&[1, 32])).unwrap_err(),
        Error::Unsupported("CCSDS short fast unary tail")
    );
    let mut bits = Bits::default();
    split_block(&mut bits, 16, Some(300), 1, 40);
    assert_eq!(
        decode(&ccsds(1, 32, 16, bits.finish()), profile(&[1, 32])).unwrap_err(),
        Error::Unsupported("CCSDS pinned binary input margin")
    );
}

#[test]
fn ccsds_payload_truncation_and_byte_mutations_do_not_panic() {
    let mut zero = Bits::default();
    zero_run(&mut zero, 9, Some(20), 4);
    let mut split = Bits::default();
    split_block(&mut split, 16, Some(300), 3, 2);
    for bytes in [
        ccsds(1, 32, 9, ccsds_uncompressed(9, &[0; 32])),
        ccsds(1, 32, 9, zero.finish()),
        ccsds(1, 32, 16, split.finish()),
    ] {
        let data = section_range(&bytes, 7);
        for end in data.start + 5..data.end {
            let mut bad = bytes.clone();
            let mut truncated = section(7, end - data.start);
            truncated[5..].copy_from_slice(&bytes[data.start + 5..end]);
            replace(&mut bad, 7, truncated);
            assert!(decode(&bad, profile(&[1, 32])).is_err());
        }
        for position in 0..bytes.len() {
            for value in [0, 128, 255] {
                let mut bad = bytes.clone();
                bad[position] = value;
                assert!(
                    std::panic::catch_unwind(|| decode(&bad, profile(&[1, 32]))).is_ok(),
                    "byte {position}={value}"
                );
            }
        }
    }
}

fn measured(bytes: &[u8], shape: &[usize]) -> crate::allocation_tests::Counts {
    use crate::allocation_tests::Measurement;
    let probe = Measurement::start();
    let now = std::time::Instant::now();
    preflight::validate(bytes, profile(shape)).unwrap();
    let preflight_seconds = now.elapsed().as_secs_f64();
    assert_eq!(probe.counts().total, 0);
    drop(probe);
    let measurement = Measurement::start();
    let encoded = bytes.to_vec();
    let mut request = profile(shape);
    request.north_up = true;
    request.adjust_longitude_range = true;
    request.scale_offset = Some(ScaleOffset::new(9.80665, -273.15).unwrap());
    let now = std::time::Instant::now();
    let result = decode(&encoded, request).unwrap().into_le_bytes();
    assert_eq!(result.len(), shape.iter().product::<usize>() * 8);
    let decode_seconds = now.elapsed().as_secs_f64();
    drop(result);
    drop(encoded);
    let counts = measurement.counts();
    let charge = charges(bytes.len() as u64, request).unwrap();
    assert_eq!(counts.live, 0);
    assert!(!counts.foreign_deallocation);
    // F funds retained input plus packed copies/queue; X funds the RSI buffer,
    // decoded bytes, f32/f64 vectors, bitmap/order/scale and final conversion.
    // This total-peak assertion is accompanied by the source phase ledger.
    assert!(counts.peak <= charge.fetch + charge.decode);
    eprintln!(
        "CCSDS E={} D={} peak={} cumulative={} F={} X={} preflight_s={} decode_convert_s={}",
        bytes.len(),
        shape.iter().product::<usize>() * 8,
        counts.peak,
        counts.total,
        charge.fetch,
        charge.decode,
        preflight_seconds,
        decode_seconds
    );
    counts
}

#[test]
fn maximum_decoded_ccsds_byte_f32_f64_and_bitmap_peaks() {
    let (ny, nx) = (1792, 4096);
    let points = ny * nx;
    let bytes = ccsds(ny, nx, 16, ccsds_uncompressed(16, &vec![1; points]));
    measured(&bytes, &[ny, nx]);
    // Half bitmap, with 32-point blocks still exact in the packed stream.
    let mut bytes = ccsds(ny, nx, 9, ccsds_uncompressed(9, &vec![1; points / 2]));
    let p = section_range(&bytes, 5).start;
    u32_at(&mut bytes, p + 5, (points / 2) as u32);
    replace(
        &mut bytes,
        6,
        bitmap(&(0..points).map(|i| i % 2 == 0).collect::<Vec<_>>()),
    );
    measured(&bytes, &[ny, nx]);
    measured(&ccsds(ny, nx, 0, vec![]), &[ny, nx]);
}

#[test]
fn maximum_encoded_ccsds_copies_are_funded_by_f() {
    // Deliberately inefficient but valid Rice k=0 codes fill almost 32 MiB
    // with only 1 MiB decoded. Each mapped value 2047 is within 16 bits.
    // An uncompressed last block supplies the pinned fast reader's input tail.
    let points = 131072;
    let mut bits = Bits::default();
    for block in 0..points / 32 - 1 {
        split_block(&mut bits, 16, (block % 128 == 0).then_some(0), 0, 2047);
    }
    bits.push(15, 4);
    for _ in 0..32 {
        bits.push(0, 16);
    }
    let mut bytes = ccsds(1, points, 16, bits.finish());
    let pad = MAX_ENCODED_BYTES - bytes.len();
    assert!(pad >= 5);
    let insertion = section_range(&bytes, 3).start;
    bytes.splice(insertion..insertion, section(2, pad));
    let len = bytes.len() as u64;
    bytes[8..16].copy_from_slice(&len.to_be_bytes());
    assert_eq!(bytes.len(), MAX_ENCODED_BYTES);
    let counts = measured(&bytes, &[1, points]);
    let charge = charges(bytes.len() as u64, profile(&[1, points])).unwrap();
    // This case makes the packed-copy F term observable rather than hiding it
    // inside a large D allowance. It must not be charged entirely against X.
    assert!(counts.peak > bytes.len() + charge.decode);
}

#[test]
fn ccsds_simultaneous_units_and_tiny_zero_profiles() {
    measured(&ccsds(1, 1, 0, vec![]), &[1, 1]);
    let bytes = std::sync::Arc::new(ccsds(1, 4096, 9, ccsds_uncompressed(9, &[0; 4096])));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let bytes = bytes.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                measured(&bytes, &[1, 4096])
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn second_extension_fast_path_and_structured_payload_mutations() {
    let mut bits = Bits::default();
    for block in 0..64 {
        bits.push(0, 3);
        bits.push(1, 1);
        if block == 0 {
            bits.push(10, 8);
        }
        for _ in 0..16 {
            bits.unary(0);
        }
    }
    let bytes = ccsds(1, 2048, 8, bits.finish());
    assert_eq!(
        decode(&bytes, profile(&[1, 2048])).unwrap().values,
        [10.; 2048]
    );
    let range = section_range(&bytes, 7);
    let mut state = 0x9e3779b97f4a7c15u64;
    let mut admitted = 0;
    for iteration in 0..10000 {
        let mut bad = bytes.clone();
        for _ in 0..1 + iteration % 3 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let index = range.start + 5 + state as usize % (range.len() - 5);
            bad[index] = (state >> 32) as u8;
        }
        let result = std::panic::catch_unwind(|| decode(&bad, profile(&[1, 2048]))).unwrap();
        if let Ok(decoded) = result {
            assert_eq!(decoded.values.len(), 2048);
            admitted += 1;
        }
    }
    eprintln!(
        "CCSDS payload mutations admitted={admitted}, rejected={}",
        10000 - admitted
    );
}
