use super::*;
use cratonvm_types::flags::with_thread_overrides;


const POISON: u64 = 0x5EED_C0DE_5EED_C0DE;
const FRAME_SIZES: [i32; 14] = [
    8, 16, 24, 32, 40, 136, 248, 256, 264, 520, 728, 1032, 1280, 2056,
];

/// The AVX form only where the clear may use it.
fn wide_forms() -> Vec<bool> {
    if fresh_frame_clear_avx_supported() {
        vec![false, true]
    } else {
        vec![false]
    }
}


/// r16w7's masks plus the round-25 shapes: claimed pairs between cleared
/// pairs, and an empty mask ("none" as the clear passes it).
fn masks(words: usize) -> Vec<(&'static str, Vec<bool>)> {
    let window_lo = words / 4;
    vec![
        ("empty", Vec::new()),
        ("none", vec![false; words]),
        (
            "a 16-word window",
            (0..words)
                .map(|w| w >= window_lo && w < window_lo + 16)
                .collect(),
        ),
        ("every third word", (0..words).map(|w| w % 3 == 1).collect()),
        ("alternate words", (0..words).map(|w| w % 2 == 0).collect()),
        (
            "alternate pairs",
            (0..words).map(|w| (w / 2) % 2 == 1).collect(),
        ),
        ("one in five kept", (0..words).map(|w| w % 5 != 0).collect()),
        ("the top word", (0..words).map(|w| w == 0).collect()),
        (
            "the bottom word",
            (0..words).map(|w| w + 1 == words).collect(),
        ),
        (
            "all but the middle word",
            (0..words).map(|w| w != words / 2).collect(),
        ),
        ("every word", vec![true; words]),
    ]
}

fn required(skip: &[bool], w: usize) -> bool {
    !skip.get(w).copied().unwrap_or(false)
}








