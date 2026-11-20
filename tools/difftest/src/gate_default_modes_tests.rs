use super::*;
use cratonvm_difftest::matrix::{axes_for, PathAxis};

/// The `--modes` default that `RunArgs` (and therefore the CI `gate` step)
/// falls back on. Parsed from the same string clap uses, so this test moves
/// with the flag rather than restating it.
const GATE_DEFAULT_MODES: &str = "jit-on,nojit,interp-decoded";

fn default_modes() -> Vec<Mode> {
    GATE_DEFAULT_MODES
        .split(',')
        .map(|s| parse_one_mode(s).expect("default mode list must parse"))
        .collect()
}

/// The gate's default must cover BOTH interpreter implementations.
///
/// ARCH-2026-08-04 A4b. The interpreter has two implementations of the same
/// 122 opcodes, selected by `--noverify`. `Mode::InterpDecoded` is the only
/// mode that passes that flag and the only one mapping to
/// `PathAxis::InterpreterDecoded` — and it was absent from this default, so
/// the CI gate compared the fast path against HotSpot and never ran the
/// decoded one.
///
/// `nojit` does NOT cover it: `nojit` is the fast path with the JIT out of
/// the picture (`matrix.rs` maps it to `InterpreterFast`). Anyone reading
/// the mode list quickly will assume otherwise, which is exactly how the
/// axis went unexercised while being fully defined, labelled, and listed in
/// both `Mode::all()` and `execution_paths()`.
#[test]
fn the_gate_default_covers_both_interpreter_implementations() {
    let modes = default_modes();
    let covered: Vec<PathAxis> = modes.iter().flat_map(|m| axes_for(*m).to_vec()).collect();

    assert!(
        covered.contains(&PathAxis::InterpreterFast),
        "the gate default must exercise the raw fast path; modes were {:?}",
        modes.iter().map(|m| m.label()).collect::<Vec<_>>()
    );
    assert!(
        covered.contains(&PathAxis::InterpreterDecoded),
        "the gate default must exercise the DECODED interpreter path — the \
         second of the interpreter's two implementations of the same 122 \
         opcodes. Only Mode::InterpDecoded provides it (it is the only mode \
         passing --noverify); `nojit` is the fast path, not this one. \
         modes were {:?}",
        modes.iter().map(|m| m.label()).collect::<Vec<_>>()
    );
}

/// Exactly one mode in the default supplies the decoded axis, and it is the
/// one that passes `--noverify`.
///
/// Guards the substitution that would silently re-open the gap: swapping
/// `interp-decoded` for another interpreter-ish mode that does not actually
/// disable the raw handlers.
#[test]
fn only_interp_decoded_supplies_the_decoded_axis() {
    let providers: Vec<&'static str> = default_modes()
        .into_iter()
        .filter(|m| axes_for(*m).contains(&PathAxis::InterpreterDecoded))
        .map(|m| m.label())
        .collect();
    assert_eq!(
        providers,
        vec!["interp-decoded"],
        "the decoded axis must come from interp-decoded and nothing else"
    );
}
