use super::*;

#[test]
fn every_interface_has_distinct_owner_class() {
    let mut seen = std::collections::HashSet::new();
    for iface in SharedSecretsInterface::all() {
        let owner = iface.owner_class();
        assert!(
            seen.insert(owner),
            "{:?} owner class {owner} collides with a prior interface",
            iface
        );
    }
    // Derived, not restated: a literal 15 here was one of the numbers that
    // agreed for the wrong reason while the lists' CONTENTS diverged.
    assert_eq!(seen.len(), SharedSecretsInterface::all().len());
}

#[test]
fn every_interface_has_distinct_factory_method() {
    let mut seen = std::collections::HashSet::new();
    for iface in SharedSecretsInterface::all() {
        let factory = iface.factory_method();
        assert!(
            seen.insert(factory),
            "{:?} factory method {factory} collides",
            iface
        );
    }
}

/// F24-1 (2026-08-13) — REPLACES `factory_method_matches_canonical_shape`,
/// a guard that could not fail and that punished the one correct answer.
///
/// Its body was, in full:
///
/// ```ignore
/// for iface in SharedSecretsInterface::all() {
///     let m = iface.factory_method();
///     assert!(m.starts_with("getJava") && m.ends_with("Access"));
/// }
/// ```
///
/// Every name in the map had been *written* to that shape, so the assertion
/// compared the list against itself. It passed on `getJavaSecurityAccess`
/// (a member JDK 25 does not declare at all) and on `getJavaUtilJarAccess`
/// (a misspelling of `javaUtilJarAccess`) for exactly as long as both were
/// present, and it would have gone RED on the repair — the correct spelling
/// has no `get` prefix.
///
/// The replacement inverts it: the JDK's departure from its own convention
/// is the thing pinned, by name. It fails if someone "restores consistency"
/// (the exception list empties) and it fails if a new non-conforming name
/// appears unreviewed (the list grows).
#[test]
fn factory_method_matches_the_jdk25_surface() {
    let departures: Vec<&str> = SharedSecretsInterface::all()
        .iter()
        .map(|i| i.factory_method())
        .filter(|m| !(m.starts_with("getJava") && m.ends_with("Access")))
        .collect();
    assert_eq!(
        departures,
        vec!["javaUtilJarAccess"],
        "exactly one JDK 25 SharedSecrets accessor departs from the \
         getJava*Access convention. If this list is EMPTY someone has \
         re-broken `javaUtilJarAccess` by adding a `get` prefix the JDK \
         does not have (`javap -p jdk.internal.access.SharedSecrets | \
         grep JarAccess`). If it has GROWN, verify the new name against \
         `javap -p` before widening this assertion."
    );
}

/// The count is derived, not restated: [`SHARED_SECRETS_OWNERS`] is a second
/// hand-written spelling of [`SharedSecretsInterface::all`], and the two
/// drifting is the failure this file already had once.
///
/// F24-1 replaces `all_contains_exactly_fifteen_entries`, which asserted
/// `all().len() == 15` and `SHARED_SECRETS_OWNERS.len() == 15`
/// *independently*. Two length checks against a literal cannot see contents,
/// which is precisely how a 15-entry `all()` containing
/// `JavaObjectInputStream` coexisted with a 15-entry bridge table containing
/// `JavaIOFileDescriptor` instead.
#[test]
fn owners_slice_equals_all() {
    assert_eq!(
        SHARED_SECRETS_OWNERS,
        SharedSecretsInterface::all(),
        "SHARED_SECRETS_OWNERS and all() are the same list written twice \
         and have diverged"
    );
}

/// **Task-2 check: the one that crosses the crate boundary.**
///
/// The `native-builtins` doc comment above its `FACTORIES` table has claimed
/// since WP1.4 that "a compile-time `#[test]` in the vm crate asserts the
/// two lists stay in sync". No such test existed. The two lists had drifted
/// in *both* directions while both held fifteen entries, so every
/// length-based or count-based guard on either side passed:
///
/// | | `all()` (this file) | `FACTORIES` (bridge) |
/// |---|---|---|
/// | `java/io/FileDescriptor$1` | absent | present |
/// | `java/io/ObjectInputStream$1` | present | absent |
///
/// F24-1 adds the missing `JavaIOFileDescriptor` variant, so one row of that
/// table is closed. The other is a deliberate, documented asymmetry and is
/// pinned here by name rather than papered over.
///
/// # Why this can be written at all
///
/// `jdk_baseline` — the external JDK-surface oracle the bridge's own guards
/// use — is `pub(crate)` to `native-builtins`, so this crate cannot reach
/// it. But `shared_secrets_bridge::owner_classes()` is `pub`, and `vm`
/// already depends on `cratonvm-native-builtins`, so the bridge's live owner
/// set IS observable from here. That projection is enough: owner class is
/// the field the two tables disagreed on.
#[test]
fn owner_classes_mirror_the_native_builtins_bridge() {
    use std::collections::BTreeSet;

    let ours: BTreeSet<&'static str> = SharedSecretsInterface::all()
        .iter()
        .map(|i| i.owner_class())
        .collect();
    let bridge: BTreeSet<&'static str> =
        cratonvm_native_builtins::shared_secrets_bridge::owner_classes().collect();

    let only_bridge: Vec<&str> = bridge.difference(&ours).copied().collect();
    assert!(
        only_bridge.is_empty(),
        "native-builtins registers SharedSecrets factories handing out owner \
         classes this file does not list: {only_bridge:?}. Add a \
         SharedSecretsInterface variant for each, verifying the interface, \
         the owner class and the accessor spelling with `javap -p` first."
    );

    let only_ours: Vec<&str> = ours.difference(&bridge).copied().collect();
    assert_eq!(
        only_ours,
        vec!["java/io/ObjectInputStream$1"],
        "the ONLY interface this file may list without a matching bridge \
         factory is JavaObjectInputStream. JDK 25 builds that access object \
         with an `invokedynamic` in `ObjectInputStream.<clinit>` and has no \
         `ObjectInputStream$1`, so the bridge deliberately does not \
         intercept the getter — see the NOTE at that entry in \
         `native-builtins/src/shared_secrets_bridge.rs`. Any other name \
         here is drift, which is what this test exists to catch."
    );
}

#[test]
fn java_nio_owner_is_access_impl_not_formatter() {
    // JDK 25's `java.nio.Buffer$1` is the IOOBE formatter Function.
    // `JavaNioAccess` is implemented by `Buffer$2`; using the formatter as
    // the SharedSecrets singleton makes calls such as scaleShifts(Buffer)
    // dispatch to the wrong receiver class.
    assert_eq!(
        SharedSecretsInterface::JavaNio.owner_class(),
        "java/nio/Buffer$2"
    );
}
