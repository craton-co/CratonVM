use super::{
    java_dadd, java_ddiv, java_dmul, java_drem, java_dsub, java_fadd, java_fdiv, java_fmul,
    java_frem, java_fsub,
};

const DOUBLES: [u64; 4] = [
    0x7ff8_0000_0000_0000,
    0xfff8_0000_0000_0123,
    0x7ff0_0000_0000_0001,
    0x3ff8_0000_0000_0000,
];
const FLOATS: [u32; 4] = [0x7fc0_0000, 0xffc0_0123, 0x7f80_0001, 0x3fc0_0000];

const HOTSPOT_DOUBLE: [&str; 16] = [
        "7ff8000000000000,7ff8000000000000,7ff8000000000000,7ff8000000000000,7ff8000000000000",
        "fff8000000000123,7ff8000000000000,fff8000000000123,7ff8000000000000,7ff8000000000123",
        "7ff8000000000001,7ff8000000000000,7ff8000000000001,7ff8000000000000,7ff8000000000001",
        "7ff8000000000000,7ff8000000000000,7ff8000000000000,7ff8000000000000,7ff8000000000000",
        "7ff8000000000000,fff8000000000123,7ff8000000000000,fff8000000000123,7ff8000000000000",
        "fff8000000000123,fff8000000000123,fff8000000000123,fff8000000000123,7ff8000000000123",
        "7ff8000000000001,fff8000000000123,7ff8000000000001,fff8000000000123,7ff8000000000001",
        "fff8000000000123,fff8000000000123,fff8000000000123,fff8000000000123,7ff8000000000123",
        "7ff8000000000000,7ff8000000000001,7ff8000000000000,7ff8000000000001,7ff8000000000000",
        "fff8000000000123,7ff8000000000001,fff8000000000123,7ff8000000000001,7ff8000000000123",
        "7ff8000000000001,7ff8000000000001,7ff8000000000001,7ff8000000000001,7ff8000000000001",
        "7ff8000000000001,7ff8000000000001,7ff8000000000001,7ff8000000000001,7ff8000000000001",
        "7ff8000000000000,7ff8000000000000,7ff8000000000000,7ff8000000000000,7ff8000000000000",
        "fff8000000000123,fff8000000000123,fff8000000000123,fff8000000000123,fff8000000000123",
        "7ff8000000000001,7ff8000000000001,7ff8000000000001,7ff8000000000001,7ff8000000000001",
        "4008000000000000,0,4002000000000000,3ff0000000000000,0",
];

const HOTSPOT_FLOAT: [&str; 16] = [
        "7fc00000,7fc00000,7fc00000,7fc00000,7fc00000",
        "ffc00123,7fc00000,ffc00123,7fc00000,7fc00123",
        "7fc00001,7fc00000,7fc00001,7fc00000,7fc00001",
        "7fc00000,7fc00000,7fc00000,7fc00000,7fc00000",
        "7fc00000,ffc00123,7fc00000,ffc00123,7fc00000",
        "ffc00123,ffc00123,ffc00123,ffc00123,7fc00123",
        "7fc00001,ffc00123,7fc00001,ffc00123,7fc00001",
        "ffc00123,ffc00123,ffc00123,ffc00123,7fc00123",
        "7fc00000,7fc00001,7fc00000,7fc00001,7fc00000",
        "ffc00123,7fc00001,ffc00123,7fc00001,7fc00123",
        "7fc00001,7fc00001,7fc00001,7fc00001,7fc00001",
        "7fc00001,7fc00001,7fc00001,7fc00001,7fc00001",
        "7fc00000,7fc00000,7fc00000,7fc00000,7fc00000",
        "ffc00123,ffc00123,ffc00123,ffc00123,ffc00123",
        "7fc00001,7fc00001,7fc00001,7fc00001,7fc00001",
        "40400000,0,40100000,3f800000,0",
];

fn hex_cells(cell: &str) -> Vec<u64> {
    cell.split(',')
        .map(|c| u64::from_str_radix(c, 16).expect("a hex cell"))
        .collect()
}

#[test]
fn the_double_ops_answer_hotspots_nan_table() {
    let mut k = 0;
    for &x in &DOUBLES {
        for &y in &DOUBLES {
            let (a, b) = (f64::from_bits(x), f64::from_bits(y));
            let got = [
                java_dadd(a, b).to_bits(),
                java_dsub(a, b).to_bits(),
                java_dmul(a, b).to_bits(),
                java_ddiv(a, b).to_bits(),
                java_drem(a, b).to_bits(),
            ];
            assert_eq!(got.to_vec(), hex_cells(HOTSPOT_DOUBLE[k]), "{x:#x} op {y:#x}");
            k += 1;
        }
    }
}

#[test]
fn the_float_ops_answer_hotspots_nan_table() {
    let mut k = 0;
    for &x in &FLOATS {
        for &y in &FLOATS {
            let (a, b) = (f32::from_bits(x), f32::from_bits(y));
            let got = [
                u64::from(java_fadd(a, b).to_bits()),
                u64::from(java_fsub(a, b).to_bits()),
                u64::from(java_fmul(a, b).to_bits()),
                u64::from(java_fdiv(a, b).to_bits()),
                u64::from(java_frem(a, b).to_bits()),
            ];
            assert_eq!(got.to_vec(), hex_cells(HOTSPOT_FLOAT[k]), "{x:#x} op {y:#x}");
            k += 1;
        }
    }
}
/// Wave 7 (HotSpot 25.0.3, every tier, `R22LoopsNaNPayload` rows d4 / f4):
/// a NaN divisor keeps its sign only over a FINITE dividend; an infinite
/// one clears it like a NaN dividend does.
#[test]
fn a_nan_divisor_keeps_its_sign_only_over_a_finite_dividend() {
    let neg = f64::from_bits(0xfff8_0000_0000_0123);
    for (x, want) in [
        (f64::INFINITY, 0x7ff8_0000_0000_0123u64),
        (f64::NEG_INFINITY, 0x7ff8_0000_0000_0123),
        (-0.0, 0xfff8_0000_0000_0123),
        (1.5, 0xfff8_0000_0000_0123),
    ] {
        assert_eq!(java_drem(x, neg).to_bits(), want, "{x} % -NaN");
    }
    let negf = f32::from_bits(0xffc0_0123);
    for (x, want) in [
        (f32::INFINITY, 0x7fc0_0123u32),
        (f32::NEG_INFINITY, 0x7fc0_0123),
        (-0.0, 0xffc0_0123),
        (1.5, 0xffc0_0123),
    ] {
        assert_eq!(java_frem(x, negf).to_bits(), want, "{x} % -NaN");
    }
}
