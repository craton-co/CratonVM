// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests against HotSpot 25's libjpeg output (fixtures.rs).

use super::enc::{CompSpec, EncParams, Encoder};
use super::fixtures::CASES;
use super::std_tables::*;
use super::{linear_quality, scale_qtable, HuffSpec};

fn gen(x: usize, y: usize, c: usize) -> u8 {
    ((x * 13 + y * 29 + c * 71 + ((x * y) % 17) * 5) & 0xff) as u8
}

fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn params(w: usize, h: usize, quality: f32, gray: bool) -> EncParams {
    let lin = linear_quality(quality);
    let q0 = scale_qtable(&K1, lin, true);
    let q1 = scale_qtable(&K2, lin, true);
    let comps = if gray {
        vec![CompSpec {
            id: 1,
            h: 1,
            v: 1,
            tq: 0,
            td: 0,
            ta: 0,
        }]
    } else {
        vec![
            CompSpec {
                id: 1,
                h: 2,
                v: 2,
                tq: 0,
                td: 0,
                ta: 0,
            },
            CompSpec {
                id: 2,
                h: 1,
                v: 1,
                tq: 1,
                td: 1,
                ta: 1,
            },
            CompSpec {
                id: 3,
                h: 1,
                v: 1,
                tq: 1,
                td: 1,
                ta: 1,
            },
        ]
    };
    EncParams {
        width: w,
        height: h,
        in_comps: if gray { 1 } else { 3 },
        rgb_to_ycc: !gray,
        comps,
        qtables: [Some(q0), Some(q1), None, None],
        dc_tables: [
            Some(HuffSpec::from_lengths(
                STDDCLUMINANCE_LENGTHS,
                STDDCLUMINANCE_VALUES,
            )),
            Some(HuffSpec::from_lengths(
                STDDCCHROMINANCE_LENGTHS,
                STDDCCHROMINANCE_VALUES,
            )),
            None,
            None,
        ],
        ac_tables: [
            Some(HuffSpec::from_lengths(
                STDACLUMINANCE_LENGTHS,
                STDACLUMINANCE_VALUES,
            )),
            Some(HuffSpec::from_lengths(
                STDACCHROMINANCE_LENGTHS,
                STDACCHROMINANCE_VALUES,
            )),
            None,
            None,
        ],
        write_dqt: true,
        write_dht: true,
        restart_interval: 0,
        optimize: false,
    }
}

#[test]
fn rgb_encoder_matches_hotspot() {
    let mut checked = 0;
    for case in CASES.iter().filter(|c| !c.gray) {
        let expected = from_hex(case.jpeg_hex);
        let enc = Encoder::new(params(case.w, case.h, case.quality, false));
        let rows: Vec<Vec<u8>> = (0..case.h)
            .map(|y| {
                let mut r = Vec::new();
                for x in 0..case.w {
                    for c in 0..3 {
                        r.push(gen(x, y, c));
                    }
                }
                r
            })
            .collect();
        let mut out = enc.soi();
        // JFIF APP0 written by the Java side.
        out.extend_from_slice(&expected[2..20]);
        out.extend(enc.frame_header());
        out.extend(enc.scan_header());
        out.extend(enc.encode_scan(&rows));
        out.extend(enc.eoi());
        assert_eq!(
            out.len(),
            expected.len(),
            "{}x{} q={} length",
            case.w,
            case.h,
            case.quality
        );
        let first_diff = out.iter().zip(expected.iter()).position(|(a, b)| a != b);
        assert_eq!(
            first_diff, None,
            "{}x{} q={} first differing byte",
            case.w, case.h, case.quality
        );
        checked += 1;
    }
    assert!(checked >= 5);
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn rgb_decoder_matches_hotspot() {
    use super::dec::{ColorConvert, Decoder, HeaderOutcome, SliceSource};
    let mut checked = 0;
    for case in CASES.iter().filter(|c| !c.gray) {
        let bytes = from_hex(case.jpeg_hex);
        let mut src = SliceSource {
            data: &bytes,
            pos: 0,
        };
        let mut d = Decoder::new();
        assert_eq!(d.read_header(&mut src).unwrap(), HeaderOutcome::Image);
        d.decode_scans(&mut src).unwrap();
        let rows = d.reconstruct(ColorConvert::YccToRgb, true);
        let mut argb = Vec::new();
        for r in &rows {
            for x in 0..case.w {
                argb.extend_from_slice(&[0xff, r[x * 3], r[x * 3 + 1], r[x * 3 + 2]]);
            }
        }
        assert_eq!(
            crc32(&argb),
            case.dec_crc,
            "{}x{} q={} decode crc",
            case.w,
            case.h,
            case.quality
        );
        checked += 1;
    }
    assert!(checked >= 5);
}

#[test]
fn progressive_decoder_matches_hotspot() {
    use super::dec::{ColorConvert, Decoder, HeaderOutcome, SliceSource};
    for &(w, h, crc, hex) in super::fixtures::PROGRESSIVE {
        let bytes = from_hex(hex);
        let mut src = SliceSource {
            data: &bytes,
            pos: 0,
        };
        let mut d = Decoder::new();
        assert_eq!(d.read_header(&mut src).unwrap(), HeaderOutcome::Image);
        assert!(d.progressive);
        d.decode_scans(&mut src).unwrap();
        let rows = d.reconstruct(ColorConvert::YccToRgb, true);
        let mut argb = Vec::new();
        for r in &rows {
            for x in 0..w {
                argb.extend_from_slice(&[0xff, r[x * 3], r[x * 3 + 1], r[x * 3 + 2]]);
            }
        }
        assert_eq!(crc32(&argb), crc, "progressive {w}x{h}");
        assert_eq!(rows.len(), h);
    }
}

#[test]
fn optimized_encoder_matches_hotspot() {
    for &(w, h, hex) in super::fixtures::OPTIMIZED {
        let expected = from_hex(hex);
        let mut p = params(w, h, 0.75, false);
        p.optimize = true;
        p.write_dht = true;
        let enc = Encoder::new(p);
        let rows: Vec<Vec<u8>> = (0..h)
            .map(|y| {
                let mut r = Vec::new();
                for x in 0..w {
                    for c in 0..3 {
                        r.push(gen(x, y, c));
                    }
                }
                r
            })
            .collect();
        let (scan_header, data) = enc.encode(&rows);
        let mut out = enc.soi();
        out.extend_from_slice(&expected[2..20]);
        out.extend(enc.frame_header());
        out.extend(scan_header);
        out.extend(data);
        out.extend(enc.eoi());
        assert_eq!(out.len(), expected.len(), "{w}x{h} length");
        assert_eq!(
            out.iter().zip(expected.iter()).position(|(a, b)| a != b),
            None,
            "{w}x{h} first differing byte"
        );
    }
}
