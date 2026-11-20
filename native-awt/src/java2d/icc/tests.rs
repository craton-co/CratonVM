use super::*;

/// The JDK's own profiles (`jimage extract --include 'regex:.*cmm/profiles.*'`
/// into `$CRATONVM_ICC_PROFILE_DIR`); the test is skipped when they are absent.
fn pf(name: &str) -> Option<Profile> {
    let dir = std::env::var_os("CRATONVM_ICC_PROFILE_DIR")?;
    let bytes = std::fs::read(std::path::Path::new(&dir).join(name)).ok()?;
    Some(Profile::parse(&bytes).unwrap_or_else(|| panic!("{name} does not parse")))
}

fn roundtrip(other: &str, tol: f32) {
    let (Some(srgb), Some(o)) = (pf("sRGB.pf"), pf(other)) else {
        return;
    };
    let fwd = IccStage::new(srgb.clone(), o.clone(), 0);
    let back = IccStage::new(o, srgb, 0);
    for px in [
        [0.0, 0.0, 0.0],
        [1.0, 1.0, 1.0],
        [0.5, 0.5, 0.5],
        [0.9, 0.2, 0.1],
        [0.1, 0.6, 0.3],
        [0.2, 0.3, 0.8],
    ] {
        let mut mid = vec![0f32; fwd.out_channels()];
        fwd.transform(&px, &mut mid);
        let mut out = [0f32; 3];
        back.transform(&mid, &mut out);
        for k in 0..3 {
            assert!(
                (out[k] - px[k]).abs() <= tol,
                "{other}: {px:?} -> {mid:?} -> {out:?}"
            );
        }
    }
}

#[test]
fn srgb_through_ciexyz_comes_back() {
    roundtrip("CIEXYZ.pf", 0.02);
}

#[test]
fn srgb_through_pycc_comes_back() {
    // PYCC.pf's own A2B and B2A tables disagree by ~9% at white and ~4% at black.
    roundtrip("PYCC.pf", 0.1);
}

#[test]
fn srgb_white_is_d50_in_xyz() {
    let Some(srgb) = pf("sRGB.pf") else { return };
    let xyz = srgb.to_xyz(&[1.0, 1.0, 1.0], 0);
    for k in 0..3 {
        assert!((xyz[k] - D50[k]).abs() < 0.01, "{xyz:?}");
    }
}
