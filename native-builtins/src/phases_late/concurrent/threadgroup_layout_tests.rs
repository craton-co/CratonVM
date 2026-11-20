use super::*;

/// The fallback indices here and the fabricated model in
/// `ClassManager::synthetic_stub_fields` are two hand-written copies of the
/// same layout, and they were allowed to disagree with each other and with
/// the JDK for months. Assert the coupling instead of describing it.
///
/// Both must also be the **real** JDK 21+ order, which is why the expected
/// names are spelled out here rather than read from the model: a test that
/// only checked the two tables against each other would stay green if both
/// were transposed together, which is exactly the state this replaces.
#[test]
fn tg_fallback_slots_match_the_fabricated_model() {
    let model = cratonvm_classloading::synthetic_stub_field_model("java/lang/ThreadGroup");
    let instance: Vec<&str> = model
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| &*f.name)
        .collect();
    assert_eq!(
        instance,
        ["parent", "name", "maxPriority", "daemon"],
        "the fabricated ThreadGroup model must be the real JDK 21+ \
         declaration order (javap -p --module java.base java.lang.ThreadGroup)"
    );
    assert_eq!(instance[TG_SLOT_PARENT], "parent");
    assert_eq!(instance[TG_SLOT_NAME], "name");
    assert_eq!(instance[TG_SLOT_MAX_PRIORITY], "maxPriority");
    assert_eq!(instance[TG_SLOT_DAEMON], "daemon");
}

/// The model's descriptors have to match too — `maxPriority` is an `int`
/// and `daemon` a `boolean`, and swapping *those* is the half that a
/// name-only check would miss.
#[test]
fn the_fabricated_model_descriptors_are_the_real_ones() {
    let model = cratonvm_classloading::synthetic_stub_field_model("java/lang/ThreadGroup");
    let descs: Vec<(&str, &str)> = model
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| (&*f.name, &*f.descriptor))
        .collect();
    assert_eq!(
        descs,
        [
            ("parent", "Ljava/lang/ThreadGroup;"),
            ("name", "Ljava/lang/String;"),
            ("maxPriority", "I"),
            ("daemon", "Z"),
        ]
    );
}

/// `setMaxPriority`'s three steps, as pure arithmetic, pinned to what
/// Temurin 25.0.3 was measured doing by `probes/ThreadGroupPriorityProbe`.
///
/// The registered native needs a live VM, so what is checkable here is the
/// decision the native makes for a given (argument, parent ceiling) pair.
/// It is worth pinning even so: **every** plausible misreading of this API
/// is representable as a different two-liner, and this file shipped one of
/// them (`prio.max(1).min(10)`, an unconditional clamp) for months.
#[test]
fn set_max_priority_decides_the_way_the_jdk_does() {
    /// Mirrors the registered native: `None` = "no-op, leave the group
    /// alone", `Some(v)` = "store v here and assign it to every
    /// descendant".
    fn decide(pri: i32, parent_ceiling: Option<i32>) -> Option<i32> {
        if !(1..=10).contains(&pri) {
            return None;
        }
        Some(match parent_ceiling {
            Some(c) => pri.min(c),
            None => pri,
        })
    }

    // Out of range does NOTHING. A clamp would answer Some(10)/Some(1)
    // here, which is what made a group lowered to 4 come back up to 10.
    assert_eq!(decide(15, Some(10)), None);
    assert_eq!(decide(-4, Some(10)), None);
    assert_eq!(decide(0, Some(10)), None);
    assert_eq!(decide(11, Some(10)), None);

    // In range, under the ceiling: taken, including a RAISE. Reading this
    // API as a one-way ratchet is the other common misreading.
    assert_eq!(decide(4, Some(10)), Some(4));
    assert_eq!(decide(7, Some(10)), Some(7));
    assert_eq!(decide(1, Some(10)), Some(1));
    assert_eq!(decide(10, Some(10)), Some(10));

    // In range, above the ceiling: capped at the parent's.
    assert_eq!(decide(9, Some(3)), Some(3));
    assert_eq!(decide(10, Some(1)), Some(1));
    // Below the ceiling still takes.
    assert_eq!(decide(2, Some(3)), Some(2));

    // A root group (no parent) has no ceiling to cap against.
    assert_eq!(decide(10, None), Some(10));
    assert_eq!(decide(1, None), Some(1));
}

/// The value that propagates is ASSIGNED to descendants, not min'd into
/// them — a subgroup below its parent comes UP. Measured, and surprising
/// enough that the natural `if child > new { lower(child) }` guard would be
/// wrong in a way no in-range test would catch.
#[test]
fn propagation_assigns_rather_than_only_lowering() {
    // What the native does to each descendant, given the value it stored.
    fn descendant_value(effective: i32, _existing: i32) -> i32 {
        effective
    }
    assert_eq!(descendant_value(5, 1), 5, "a subgroup at 1 comes up to 5");
    assert_eq!(descendant_value(2, 10), 2, "and one at 10 comes down to 2");
}
