// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `java/lang/String` native surface, and which of it survives into a
//! real-JDK registry.
//!
//! # What this file used to assert, and why it changed
//!
//! WP8.10.9 registered `String.contains(CharSequence)` and
//! `String.startsWith(String,int)` because synthetic-jdk mode raised
//! `NoSuchMethodError` the moment a boot path called
//! `someName.contains("Module")` (the canonical case is
//! `vm/tests/wildfly_boot_fixtures/JBossModulesProbe.java`). This file then
//! asserted those triples were present in the registry of a
//! `VmConfig::default()` VM — which, in the default build (no `synthetic-jdk`
//! feature), is a **real-JDK** registry. The requirement was about synthetic
//! mode; the assertion was taken in the other one.
//!
//! On 2026-08-04 the forced-native `java/lang/String` policy was removed
//! (`forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`).
//! Contract §1.4 — real class bytes are authoritative — is now enforced where
//! it can be enforced once for every dispatch path: `NativeMethodRegistry::
//! register` drops every `java/lang/String` `Bridge` in real-JDK mode, so the
//! real `String.contains` bytecode runs. The WP8.10.9 requirement is untouched:
//! synthetic mode does not set that drop, and the registrations are still made.
//!
//! So this file now asserts the policy rather than the registration, in the one
//! mode it can observe:
//!
//! 1. the `java/lang/String` `Bridge` surface is ABSENT from a real-JDK
//!    registry — including `contains` and `startsWith`, the two WP8.10.9
//!    named;
//! 2. `intern()` survives, because the image declares it `ACC_NATIVE` and a
//!    bridge in front of a genuinely native method is contract §1.5, not a
//!    shadow;
//! 3. the four reviewed `NativeKind::Intrinsic` fast-regex shapes survive,
//!    because §1.4's reviewed exception is decided on kind and needs no name
//!    list at any dispatch site.
//!
//! (2) and (3) are what stop (1) from being a test that a whole class was
//! deleted: the rule is "drop the bridges that shadow bytecode", and a rule
//! that dropped everything would satisfy (1) just as well.

use std::sync::Arc;
use std::sync::OnceLock;

use cratonvm_vm::config::{discover_boot_classpath, JdkMode, VmConfig};
use cratonvm_vm::vm::SharedVm;

/// A registry built by `vm_init`'s REAL-JDK arm — which is what every
/// assertion in this file is about, and says so in its own name.
///
/// Until 2026-09-05 this was `SharedVm::new(VmConfig::default())`. The header
/// above already names the shape of that mistake one layer up ("The
/// requirement was about synthetic mode; the assertion was taken in the other
/// one") and it was the same mistake again, in the other direction:
/// `VmConfig::default()` selects `EMBEDDED_DEFAULT_JDK_MODE` =
/// `JdkMode::Synthetic`. It *read* as a real-JDK registry only because the drop
/// that produces one was keyed on the Cargo feature instead of on the run — and
/// a synthetic run with the real-JDK drop applied is a VM with no
/// `java/lang/String` at all: no bytecode, because there is no JDK image, and
/// no bridge, because the drop took it. `"abcdef".length()` raised
/// `NoSuchMethodError` there. Keying the drop on the run is the fix; asking for
/// the mode by name is this file's half of it.
///
/// `None` when no JDK image is reachable: real-JDK mode has nothing to boot
/// against then. `discover_boot_classpath` is the call `SharedVm::new` itself
/// makes, so the question is asked in the same words it is answered in.
fn shared() -> Option<Arc<SharedVm>> {
    static VM: OnceLock<Option<Arc<SharedVm>>> = OnceLock::new();
    VM.get_or_init(|| {
        if discover_boot_classpath(None).is_empty() {
            return None;
        }
        Some(Arc::new(SharedVm::new(
            VmConfig::default().with_jdk_mode(JdkMode::Real),
        )))
    })
    .clone()
}

/// Skip loudly rather than pass silently. A green run of this file on a box
/// with no JDK would assert nothing at all, which is the failure mode a
/// registry-absence test is least able to notice about itself.
macro_rules! shared_or_skip {
    () => {
        match shared() {
            Some(vm) => vm,
            None => {
                eprintln!(
                    "skipping: no JDK image reachable (JAVA_HOME -> jmods / lib/modules). \
                     Every assertion in this file is about a real-JDK registry."
                );
                return;
            }
        }
    };
}

/// Every `java/lang/String` shape whose real-JDK class bytes carry a `Code`
/// attribute must lose — and be gone from the registry entirely, so no
/// dispatch path has anything to find.
///
/// The list is the shapes the four deleted copies of the policy forced, plus
/// the two WP8.10.9 named.
#[test]
fn real_jdk_registry_has_no_string_bridge_shadowing_bytecode() {
    let shared = shared_or_skip!();
    let registry = &shared.natives.native_methods;

    let shadowing: &[(&str, &str)] = &[
        // WP8.10.9's two.
        ("contains", "(Ljava/lang/CharSequence;)Z"),
        ("startsWith", "(Ljava/lang/String;I)Z"),
        // The h2-bnf five.
        ("substring", "(I)Ljava/lang/String;"),
        ("charAt", "(I)C"),
        ("length", "()I"),
        ("isEmpty", "()Z"),
        ("startsWith", "(Ljava/lang/String;)Z"),
        // The quadratic-parent substring fix.
        ("substring", "(II)Ljava/lang/String;"),
        // The charset-name constructors — the FOURTH copy of the policy, which
        // the record counted as three. They were also wrong: `new
        // String(bytes, "NO-SUCH")` returned a UTF-8 decode instead of raising
        // `UnsupportedEncodingException`, and `"US-ASCII"` decoded as Latin-1.
        ("<init>", "([BLjava/lang/String;)V"),
        ("<init>", "([BIILjava/lang/String;)V"),
        // The Unicode/locale-sensitive residue the cold list forced.
        ("trim", "()Ljava/lang/String;"),
        ("toLowerCase", "(Ljava/util/Locale;)Ljava/lang/String;"),
        ("toUpperCase", "(Ljava/util/Locale;)Ljava/lang/String;"),
        // The plain ones it also forced.
        ("equals", "(Ljava/lang/Object;)Z"),
        ("hashCode", "()I"),
        ("endsWith", "(Ljava/lang/String;)Z"),
        ("indexOf", "(Ljava/lang/String;)I"),
        ("lastIndexOf", "(Ljava/lang/String;)I"),
        ("replace", "(CC)Ljava/lang/String;"),
        ("toString", "()Ljava/lang/String;"),
    ];

    for &(name, descriptor) in shadowing {
        assert!(
            registry
                .find("java/lang/String", name, descriptor)
                .is_none(),
            "java/lang/String.{name}{descriptor} is registered in a real-JDK registry again. \
             The JDK 25 image declares it with a `Code` attribute, so a registered `Bridge` in \
             front of it is contract §1.4's `NativeShadowsBytecode` — and, measured against \
             HotSpot with `probes/StringPolicyMatrixProbe`, these natives were the CAUSE of 57 \
             of 392 divergences (unpaired surrogates decoded to U+FFFD, out-of-range indices \
             with no exception message, `null` arguments answered with a default instead of an \
             NPE). Re-registering one reinstates that. If it must win, review it against the \
             probe and register it `NativeKind::Intrinsic`."
        );
    }
}

/// `intern()` survives the drop: the image declares it `ACC_NATIVE`, so there
/// is no bytecode for it to shadow.
///
/// Without this, the test above would pass just as happily for a change that
/// dropped the whole class — a different and much worse change.
#[test]
fn real_jdk_registry_keeps_the_one_genuine_string_bridge() {
    let shared = shared_or_skip!();
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/String", "intern", "()Ljava/lang/String;")
            .is_some(),
        "java/lang/String.intern()Ljava/lang/String; is gone. It is the ONE `java/lang/String` \
         registration the JDK 25 image declares `ACC_NATIVE` (79 of the other 80 carry a `Code` \
         attribute), so it is a legitimate §1.5 bridge and the real-JDK drop must not take it. \
         If this fails, the drop stopped being 'drop the bridges that shadow bytecode' and \
         became 'drop the class'."
    );
}

/// `String.hashCode()` does NOT survive the drop -- the bytecode won it back.
///
/// It was promoted to `Intrinsic` on 2026-08-05 for CORRECTNESS: the real
/// `String.hashCode()` was wrong for any UTF-16 string, because
/// `ArraysSupport.vectorizedHashCode` read one byte per char under `T_CHAR`
/// instead of pairing them. That defect is fixed, so the registration had to
/// argue on performance again -- and lost.
///
/// Measured A-B-B-A interleaved, three rounds, `probes/StringHashCostProbe`,
/// identical digests (medians, ms):
///
/// ```text
///           cold-latin1  cold-utf16  warm  map-utf16
///   native       89         135        5      110
///   bytecode     95         149        2      111
/// ```
///
/// Faster on the first hash of a distinct string, 2-4x SLOWER on the cached
/// read, a wash on the realistic `HashMap<String,_>` workload. Both sides
/// cache in the same `String.hash` field, so the warm gap is
/// `safe_native_call` overhead on one field read. Contract 1.4's default is
/// the bytecode and a "wash" does not license shadowing it.
///
/// This test is here so re-promoting it is a decision rather than a reflex: if
/// it comes back, it comes back with suite numbers and a `register_with_kind`.
#[test]
fn string_hash_code_is_left_to_the_bytecode() {
    let shared = shared_or_skip!();
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/String", "hashCode", "()I")
            .is_none(),
        "java/lang/String.hashCode()I is registered again in a real-JDK registry. It was          measured (A-B-B-A interleaved, `probes/StringHashCostProbe`) as a wash overall and          2-4x SLOWER than the bytecode on the cached read, which is the case that dominates          real workloads. If new numbers say otherwise, bring them and use          `register_with_kind(.., Intrinsic)` -- a plain `register` here is dropped anyway, so          this failing means somebody added a kind without the measurement."
    );
}

/// The four reviewed `NativeKind::Intrinsic` fast-regex shapes survive.
///
/// They are how §1.4's reviewed exception is expressed now: on the KIND, with
/// no name list at any dispatch site. Registering them `Bridge` — including by
/// omission, since the ambient category at that registration site *is* `Bridge`
/// — silently deletes the SBR-02 fix, which is the failure this pins.
#[test]
fn real_jdk_registry_keeps_the_reviewed_string_intrinsics() {
    let shared = shared_or_skip!();
    let registry = &shared.natives.native_methods;

    // Same default-ON / opt-out reading as the registration site. With the flag
    // off the four are deliberately never registered and the real bytecode
    // runs, so there is nothing to assert — and saying so explicitly keeps a
    // `CRATONVM_NATIVE_STRING_REGEX=0` environment from turning this into a
    // failure that looks like a regression.
    let regex_natives_enabled =
        match cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_STRING_REGEX") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        };
    if !regex_natives_enabled {
        return;
    }

    let reviewed: &[(&str, &str)] = &[
        (
            "replaceAll",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "replaceFirst",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ),
        ("matches", "(Ljava/lang/String;)Z"),
        (
            "replace",
            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
        ),
    ];

    for &(name, descriptor) in reviewed {
        let kind = registry.kind_of("java/lang/String", name, descriptor);
        assert_eq!(
            kind,
            Some(cratonvm_native_api::NativeKind::Intrinsic),
            "java/lang/String.{name}{descriptor} must be registered `Intrinsic`, and is \
             {kind:?}. `Bridge` — which is the AMBIENT category at that registration site, so \
             it is what dropping `register_with_kind` gets you — means the real-JDK drop \
             removes it and the SBR-02 fast-regex fix disappears with no error anywhere. That \
             silent-deletion shape is what the forced-native `String` record exists about."
        );
    }
}

/// The EXACT set of `java/lang/String` registrations that survive into a
/// real-JDK registry -- two-sided, so the set cannot drift in either direction
/// without somebody adjudicating the change.
///
/// # Why a two-sided pin, and not the two one-sided ones above
///
/// The tests above check that a named list is absent and another named list is
/// present. Both passed while the drop was silently deleting **four**
/// registrations nobody had thought to name:
///
/// * `checkBoundsBeginEnd` / `checkBoundsOffCount` / `checkIndex` -- the F4
///   workaround for a generic `Preconditions` override that threw the wrong
///   exception class. Without them `"Hello, World".substring(-1)` raised
///   `ArrayIndexOutOfBoundsException`, which `catch
///   (StringIndexOutOfBoundsException)` does not catch, and `charAt`'s class
///   depended on the SIGN of the argument: a negative index reached
///   `Preconditions` and came back AIOOBE while an index past the end came
///   back SIOOBE. **All three are deliberately gone again** as of the
///   `native-builtins/src/preconditions.rs` fix -- the override honours
///   `SIOOBE_FORMATTER` now, so the real bytecode gets the class *and* the
///   message right, which the bypasses never did for `substring`. If any of
///   them reappears in this list, that override regressed and re-adding a
///   bypass here would hide it from every non-`String` caller again;
/// * `<init>(Ljava/lang/StringBuilder;)V` and its `AbstractStringBuilder`
///   sibling -- DF05. Without them `new String(sb)`, for a builder holding
///   seven characters, returned four: the real ctor's `Arrays.copyOfRange`
///   reads this VM's `char[]`-backed builder one byte at a time. Silent
///   content corruption, no exception anywhere.
///
/// Every one of those had a comment at its registration site saying exactly
/// what breaks without it. A category-wide drop invalidates all such comments
/// at once, and a test that only knows the names its author remembered cannot
/// see that. This one fails on any triple entering or leaving the set, so
/// "should this survive?" has to be answered rather than assumed.
///
/// Updating this list is expected when a `java/lang/String` native is added or
/// retired. Updating it *without* deciding which side of contract 1.4 the
/// triple falls on is the failure it exists to prevent.
#[test]
fn the_surviving_string_registration_set_is_exactly_this() {
    let shared = shared_or_skip!();
    let registry = &shared.natives.native_methods;
    let mut actual: Vec<String> = registry
        .dump_registrations()
        .into_iter()
        .filter(|(class, _, _, _)| *class == "java/lang/String")
        .map(|(_, name, descriptor, kind)| format!("{kind:?} {name}{descriptor}"))
        .collect();
    actual.sort();
    actual.dedup();
    let rendered = actual.join("\n");

    let expected = EXPECTED_SURVIVING_STRING_REGISTRATIONS.trim();
    assert_eq!(
        rendered.trim(),
        expected,
        "\nThe set of `java/lang/String` natives surviving into a real-JDK registry changed.\n\
         \n\
         A triple that DISAPPEARED is now handed to the real bytecode. Before accepting that, \
         read the comment at its registration site: four of these exist because the \
         bytecode's premise does not hold on this VM, and dropping them produced a wrong \
         exception class and, in one case, silently corrupted string content.\n\
         \n\
         A triple that APPEARED is a native standing in front of real bytecode (contract \
         1.4). It needs a review against `probes/StringPolicyMatrixProbe` and \
         `register_with_kind(.., Intrinsic)` at its own site -- not an entry here.\n"
    );
}

/// One line per surviving registration, `Kind name+descriptor`, sorted.
const EXPECTED_SURVIVING_STRING_REGISTRATIONS: &str = "\
Bridge intern()Ljava/lang/String;\n\
Intrinsic <init>(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V\n\
Intrinsic <init>(Ljava/lang/StringBuilder;)V\n\
Intrinsic chars()Ljava/util/stream/IntStream;\n\
Intrinsic codePointAt(I)I\n\
Intrinsic codePointCount(II)I\n\
Intrinsic codePoints()Ljava/util/stream/IntStream;\n\
Intrinsic format(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;\n\
Intrinsic format(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;\n\
Intrinsic formatted([Ljava/lang/Object;)Ljava/lang/String;\n\
Intrinsic indent(I)Ljava/lang/String;\n\
Intrinsic isBlank()Z\n\
Intrinsic lines()Ljava/util/stream/Stream;\n\
Intrinsic matches(Ljava/lang/String;)Z\n\
Intrinsic offsetByCodePoints(II)I\n\
Intrinsic regionMatches(ILjava/lang/String;II)Z\n\
Intrinsic regionMatches(ZILjava/lang/String;II)Z\n\
Intrinsic repeat(I)Ljava/lang/String;\n\
Intrinsic replace(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;\n\
Intrinsic replaceAll(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;\n\
Intrinsic replaceFirst(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;\n\
Intrinsic transform(Ljava/util/function/Function;)Ljava/lang/Object;\n\
Intrinsic valueOf(I)Ljava/lang/String;\n\
Intrinsic valueOf(Ljava/lang/Object;)Ljava/lang/String;";

/// The surviving JIT `StringLatin1.toLowerCase` direct bind is legal only
/// because that triple is a registered `NativeKind::Intrinsic`. Pin it.
///
/// The forced-native `java/lang/String` record asked for BOTH `toLowerCase`
/// ladders to be deleted. One was: `String.toLowerCase(Ljava/util/Locale;)`
/// was the third copy of the policy -- `check_override` forced that name, the
/// warm gate refused it, and the JIT bound it, so one method had three
/// answers depending on where it was called from.
///
/// This one is different in a way that matters and is easy to lose: its triple
/// really is registered `Intrinsic`, so baking a direct call to it is contract
/// 1.4's reviewed exception rather than a native shadowing bytecode. It also
/// accelerates the real `String.toLowerCase(Locale)` bytecode instead of
/// standing in front of it, and its input is Latin-1 by construction, so it
/// cannot reach the unpaired-surrogate cases that made the `String`-level
/// native diverge from HotSpot.
///
/// `jit/src/lib.rs` matches that triple by NAME and cannot check its kind. So
/// re-tagging the native `Bridge` -- including by omission, the ambient
/// category being what it is -- would silently turn the bind into a 1.4
/// violation observable ONLY from compiled frames, which is the hardest place
/// to notice one. This test is the check the JIT cannot make.
#[test]
fn the_jit_latin1_lower_ladder_binds_a_reviewed_intrinsic() {
    let shared = shared_or_skip!();
    let kind = shared.natives.native_methods.kind_of(
        "java/lang/StringLatin1",
        "toLowerCase",
        "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;",
    );
    assert_eq!(
        kind,
        Some(cratonvm_native_api::NativeKind::Intrinsic),
        "java/lang/StringLatin1.toLowerCase(String,byte[],Locale) is {kind:?}, and \
         `jit/src/lib.rs` bakes a direct CALL to it by name. Only an `Intrinsic` may stand \
         in front of concrete bytecode (contract 1.4); as a `Bridge` this bind becomes a \
         violation that only compiled frames can observe. Either restore the kind or delete \
         the ladder -- do not leave them disagreeing, which is exactly the state the \
         forced-native `String` record was filed about."
    );
}
