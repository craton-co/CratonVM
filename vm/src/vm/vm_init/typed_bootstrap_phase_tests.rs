use super::{
    native_census_incomplete_header_json, native_census_invocations_json,
    BootstrapInvariantError, BootstrapPhase, NATIVE_CENSUS_SCHEMA_VERSION,
};

#[test]
fn phase_invariants_fail_at_the_boundary_that_owns_them() {
    assert_eq!(
        BootstrapPhase::begin().classes_ready(0, true).err(),
        Some(BootstrapInvariantError::NoClasses)
    );
    assert_eq!(
        BootstrapPhase::begin().classes_ready(1, false).err(),
        Some(BootstrapInvariantError::CoreObjectMissing)
    );
    let classes = BootstrapPhase::begin()
        .classes_ready(1, true)
        .expect("classes");
    assert_eq!(
        classes.natives_ready(0).err(),
        Some(BootstrapInvariantError::NoNatives)
    );
    let natives = BootstrapPhase::begin()
        .classes_ready(1, true)
        .expect("classes")
        .natives_ready(1)
        .expect("natives");
    assert_eq!(
        natives.runtime_ready(false).err(),
        Some(BootstrapInvariantError::RuntimeNotWired)
    );
}

#[test]
fn valid_bootstrap_can_only_finish_after_all_typed_transitions() {
    let elapsed = BootstrapPhase::begin()
        .classes_ready(1, true)
        .expect("classes")
        .natives_ready(1)
        .expect("natives")
        .runtime_ready(true)
        .expect("runtime")
        .finish();
    assert!(elapsed <= std::time::Duration::from_secs(1));
}

// ───────────────────────── native census, schema 5 ─────────────────────
//
// `G47-1`. The registry has carried a per-slot "this count is a floor" bit
// since 2026-08-17 and 25 slots set it; until schema 5 the census writer
// emitted neither the bit nor its header total, so no reader could see any
// of them. These pin the shape that fixed it.

/// **The tally can never be emitted without its qualifier.**
///
/// This is the whole defect in one assertion: `invocations` alone is
/// unreadable — `0` means "never called" and "called through a path that
/// does not count" equally well — and the fix is that one function emits
/// both or neither. A future edit that deletes the second line has to do it
/// on purpose.
#[test]
fn a_census_rows_invocation_tally_always_carries_its_completeness_bit() {
    let floor = native_census_invocations_json(1_999, false);
    assert!(floor.contains("\"invocations\": 1999,"), "{floor}");
    assert!(
        floor.contains("\"invocations_complete\": false,"),
        "{floor}"
    );
    // JSON booleans, not the strings "false"/"true": a quoted value would
    // parse as truthy in every consumer that does a bare truthiness test,
    // which is the one direction this instrument must not err in.
    assert!(!floor.contains("\"false\""), "{floor}");

    let total = native_census_invocations_json(0, true);
    assert!(total.contains("\"invocations\": 0,"), "{total}");
    assert!(total.contains("\"invocations_complete\": true,"), "{total}");

    // Order matters for a human reading the file top to bottom: the
    // qualifier must follow the number it qualifies, not precede it.
    let i = floor.find("\"invocations\":").expect("tally key");
    let c = floor.find("\"invocations_complete\":").expect("bit key");
    assert!(i < c, "the bit must follow the tally:\n{floor}");
}

/// The header total is a **slot** count and says so; `0` is a real answer
/// (nothing declared itself) and must still be emitted, because an absent
/// key is exactly what schema 4 had and what nobody could read.
#[test]
fn the_header_states_the_incomplete_slot_total_even_when_it_is_zero() {
    let none = native_census_incomplete_header_json(0);
    assert!(
        none.contains("\"slots_with_incomplete_invocations\": 0,"),
        "{none}"
    );
    let some = native_census_incomplete_header_json(25);
    assert!(
        some.contains("\"slots_with_incomplete_invocations\": 25,"),
        "{some}"
    );
    // Header indentation (two spaces), not row indentation (six): it sits
    // beside `counts` and `invocations`, not inside `natives`.
    assert!(some.starts_with("  \""), "{some}");
}

/// **The writer, the doc example and the schema constant cannot drift.**
///
/// `G37-1` §6 N2 measured a binary emitting `schema_version: 3`, `G42-1`
/// §6 N1 measured `4` on a later one, and `--help` documented a third
/// shape — three records disagreeing about one integer, all of them right
/// about the binary they ran. The constant is the single source; this
/// witness is what makes editing the writer's literal impossible.
///
/// Reads this file from the **working tree** rather than `include_str!`,
/// matching `registrar_call_graph_witness`: a compile-time snapshot would
/// keep passing against source that is no longer there.
#[test]
fn the_census_writer_emits_the_schema_constant_and_both_new_keys() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/vm/vm_init/zgc_relocation_permitted.rs"
    ))
    .expect("witness must read zgc_relocation_permitted.rs from the working tree");

    assert_eq!(
        NATIVE_CENSUS_SCHEMA_VERSION, 5,
        "schema 5 is what adds invocations_complete; bumping this constant \
         without moving scripts/jdk-only-bridge-ratchet.py's \
         REQUIRED_CENSUS_SCHEMA (an equality test) turns the bridge ratchet \
         red — see G47-1 NOMINATION 1"
    );

    assert!(
        src.contains("NATIVE_CENSUS_SCHEMA_VERSION"),
        "the writer must stamp the constant, not a literal"
    );
    assert!(
        src.contains("native_census_invocations_json("),
        "the row loop must go through the function that emits both halves"
    );
    assert!(
        src.contains("native_census_incomplete_header_json("),
        "the header must carry slots_with_incomplete_invocations"
    );
    // The published example a reader copies from must show the new keys,
    // or the schema is documented as its predecessor.
    assert!(
        src.contains("\"invocations_complete\": false,"),
        "the doc example must show the bit"
    );
    assert!(
        src.contains("\"slots_with_incomplete_invocations\": 25,"),
        "the doc example must show the header total"
    );
}
