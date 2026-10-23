//! Test-only format oracle: logical bits, triangular pairs and unsigned inverse
//! differencing. It deliberately has no pinned buffering, fast paths or RSI
//! scratch implementation. Complete encoded blocks are truncated to the count.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}
impl Bits<'_> {
    fn get(&mut self, width: u32) -> u32 {
        let mut value = 0;
        for _ in 0..width {
            value = (value << 1)
                | u32::from((self.data[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        value
    }
    fn unary(&mut self) -> u32 {
        let mut value = 0;
        while self.get(1) == 0 {
            value += 1;
            assert!(value <= 65535);
        }
        value
    }
}

pub(super) fn decode(data: &[u8], width: u8, points: usize) -> Vec<f64> {
    let id_bits = match width {
        2 | 3 => 1,
        4 => 2,
        5..=8 => 3,
        9..=16 => 4,
        _ => panic!("unqualified oracle width"),
    };
    let maximum = (1 << width) - 1;
    let mut bits = Bits { data, position: 0 };
    let mut mapped = Vec::new();
    while mapped.len() < points {
        let block = mapped.len() / 32 % 128;
        let reference = usize::from(block == 0);
        let id = bits.get(id_bits);
        if id == (1 << id_bits) - 1 {
            for _ in 0..32 {
                mapped.push(bits.get(u32::from(width)));
            }
        } else {
            let second = id == 0 && bits.get(1) != 0;
            if reference != 0 {
                mapped.push(bits.get(u32::from(width)));
            }
            if id == 0 && !second {
                let code = bits.unary() as usize;
                let blocks = match code {
                    0..=3 => code + 1,
                    4 => (128 - block).min(64 - block % 64),
                    _ => code,
                };
                assert!(blocks <= 128 - block);
                mapped.resize(mapped.len() + blocks * 32 - reference, 0);
            } else if second {
                let mut sample = reference;
                while sample < 32 {
                    // Enumerate pairs by sum, then by their second value.
                    let code = bits.unary();
                    let (first, second) = (0..=12)
                        .flat_map(|sum| (0..=sum).map(move |b| (sum - b, b)))
                        .nth(code as usize)
                        .unwrap();
                    if sample % 2 == 0 {
                        mapped.push(first);
                        sample += 1;
                    }
                    mapped.push(second);
                    sample += 1;
                }
            } else {
                let k = id - 1;
                let quotients: Vec<_> = (reference..32).map(|_| bits.unary()).collect();
                for quotient in quotients {
                    mapped.push((quotient << k) | bits.get(k));
                }
            }
        }
    }
    mapped.truncate(points);
    let mut values = Vec::with_capacity(points);
    for interval in mapped.chunks(4096) {
        let mut previous = interval[0];
        assert!(previous <= maximum);
        values.push(f64::from(previous));
        for &difference in &interval[1..] {
            assert!(difference <= maximum);
            // Unsigned CCSDS inverse mapping, using i64 to make subtract and
            // boundary cases independent of the pinned u32 postprocessor.
            let theta = previous.min(maximum - previous);
            let next = if difference <= 2 * theta {
                i64::from(previous)
                    + if difference % 2 == 0 {
                        i64::from(difference / 2)
                    } else {
                        -i64::from(difference.div_ceil(2))
                    }
            } else if previous <= maximum / 2 {
                i64::from(difference)
            } else {
                i64::from(maximum - difference)
            };
            previous = u32::try_from(next).unwrap();
            values.push(f64::from(previous));
        }
    }
    values
}
