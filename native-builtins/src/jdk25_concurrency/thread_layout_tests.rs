use super::*;

/// Real JDK 21–25 `java.lang.Thread`, instance fields in declaration order.
/// Spelled out rather than derived, so this is a claim about the IMAGE that
/// a JDK upgrade can falsify — not a claim about our own model, which would
/// stay green if the model drifted.
const REAL_THREAD_PREFIX: [(&str, &str); 8] = [
    ("eetop", "J"),
    ("tid", "J"),
    ("name", "Ljava/lang/String;"),
    ("interrupted", "Z"),
    ("contextClassLoader", "Ljava/lang/ClassLoader;"),
    ("holder", "Ljava/lang/Thread$FieldHolder;"),
    ("threadLocals", "Ljava/lang/ThreadLocal$ThreadLocalMap;"),
    (
        "inheritableThreadLocals",
        "Ljava/lang/ThreadLocal$ThreadLocalMap;",
    ),
];

fn model() -> Vec<(String, String)> {
    cratonvm_classloading::synthetic_stub_field_model("java/lang/Thread")
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| (f.name.to_string(), f.descriptor.to_string()))
        .collect()
}

/// Every slot the fabricated model NAMES must sit at the index the real
/// class uses for that same name. The model had `contextClassLoader` at 5
/// — `holder` on every real image — until 2026-08-05.
#[test]
fn every_named_model_slot_is_at_its_real_jdk_index() {
    let m = model();
    assert_eq!(m.len(), 8, "the Thread model changed size");
    let mut named = 0;
    for (i, (name, desc)) in m.iter().enumerate() {
        if name.starts_with("_f") {
            continue;
        }
        named += 1;
        let (real_name, real_desc) = REAL_THREAD_PREFIX[i];
        assert_eq!(
            name, real_name,
            "model slot {i} is named `{name}`; the real class declares \
             `{real_name}` there"
        );
        assert_eq!(desc, real_desc, "model slot {i} descriptor");
    }
    assert!(
        named >= 3,
        "the model stopped naming anything, so this test asserts nothing"
    );
}

/// The fabricated-only slots must not collide with a slot the model names,
/// because a named slot is shared with the image and a fabricated one is
/// not. The virtual flag sat on index 4 — `contextClassLoader` — until the
/// same change moved it.
#[test]
fn fabricated_slots_do_not_collide_with_named_ones() {
    let m = model();
    for slot in [
        THREAD_FIELD_NAME,
        THREAD_FIELD_PRIORITY,
        2, // tid, by the fabricated convention
        THREAD_FIELD_TARGET,
        THREAD_FIELD_VIRTUAL,
    ] {
        assert!(
            m[slot].0.starts_with("_f"),
            "fabricated slot {slot} overlaps the model's named `{}` — a \
             named slot is shared with the real image and a fabricated one \
             is not",
            m[slot].0
        );
    }
}

/// A synthetic `Thread` must be allocated with room for the flag, or the
/// write is silently discarded by `set_field` (five bugs of exactly that
/// shape were found in two days — see `pad_to`'s doc comment).
#[test]
fn the_synthetic_allocation_covers_the_virtual_flag() {
    assert!(THREAD_SYNTHETIC_NUM_FIELDS > SYNTHETIC_THREAD_VIRTUAL_SLOT);
    assert!(
        cratonvm_classloading::synthetic_stub_instance_field_count("java/lang/Thread")
            >= THREAD_SYNTHETIC_NUM_FIELDS
    );
}
