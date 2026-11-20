use super::java_math_pow;

fn pow_bits(x: u64, y: u64) -> u64 {
    java_math_pow(f64::from_bits(x), f64::from_bits(y)).to_bits()
}

/// The page's two near-halfway rows, where glibc rounds down and HotSpot
/// rounds up; and rows the routine settles as the JLS and HotSpot do.
#[test]
fn the_last_ulp_is_hotspots() {
    assert_eq!(pow_bits(0x3fdf_ffff_ffff_ffff, 0xc008_0000_0000_0000), 0x4020_0000_0000_0002);
    assert_eq!(pow_bits(0x3fdf_ffff_ffff_ffff, 0xc01c_0000_0000_0000), 0x4060_0000_0000_0004);
    // The rows HotSpot settles before the stub runs, and `pow(-1, NaN)`.
    assert_eq!(java_math_pow(3.0, 2.0), 9.0);
    assert_eq!(java_math_pow(f64::MAX, 0.5).to_bits(), 0x5fef_ffff_ffff_ffff);
    assert!(java_math_pow(1.0, f64::INFINITY).is_nan());
    assert!(java_math_pow(-1.0, f64::NAN).is_nan());
    assert_eq!(java_math_pow(-2.0, 3.0), -8.0);
    assert_eq!(java_math_pow(0.0, -1.0), f64::INFINITY);
    assert_eq!(java_math_pow(-0.0, -3.0), f64::NEG_INFINITY);
    assert!(java_math_pow(-2.0, 0.5).is_nan());
    assert_eq!(java_math_pow(f64::NAN, 0.0), 1.0);
    // A signalling-NaN base comes back quieted, sign and payload kept.
    assert_eq!(pow_bits(0xfff3_8c7e_6d2d_89ca, 0x0a48_9e7a_dad4_bcc5), 0xfffb_8c7e_6d2d_89ca);
}

/// Replays a HotSpot rows file (`<x bits> <y bits> <Math.pow bits>` per
/// line, hex, as `PowGen` prints it) named by
/// `JR21_POW_ROWS_FILE`; skipped without one.
#[test]
fn a_hotspot_rows_file_replays_bit_for_bit() {
    let Some(path) = std::env::var_os("JR21_POW_ROWS_FILE") else {
        return;
    };
    let text = std::fs::read_to_string(path).expect("rows file");
    let (mut rows, mut wrong, mut bad) = (0u64, 0u64, Vec::new());
    for line in text.lines() {
        let f: Vec<u64> = line
            .split_whitespace()
            .filter_map(|w| u64::from_str_radix(w, 16).ok())
            .collect();
        let [x, y, want] = f[..] else { continue };
        rows += 1;
        let got = pow_bits(x, y);
        if got != want {
            wrong += 1;
            if bad.len() < 20 {
                bad.push(format!("{x:016x} {y:016x} hotspot {want:016x} got {got:016x}"));
            }
        }
    }
    assert!(rows > 0, "no rows");
    assert!(bad.is_empty(), "{wrong} of {rows} rows differ, first:\n{}", bad.join("\n"));
    eprintln!("pow rows replayed: {rows}");
}
