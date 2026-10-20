// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMS §5.4.5 "method overriding" — the one predicate both the vtable build
//! (`ClassManager::build_vtable_descriptors_with_overrides`) and the
//! interpreter's §5.4.6 selection (`vm/src/runtime/resolve/selection.rs`) use.
//!
//! They used to answer it separately and disagreed: the vtable build compared
//! run-time packages by loader AND name, refused an override whenever the NEW
//! method was package-private (§5.4.5 looks only at the overridden method's
//! access), compared against the slot's current occupant rather than the method
//! the slot was introduced for, and had no transitive clause. Which body a call
//! site ran then depended on which route filled its cache first.
//!
//! # Run-time packages: two policies, chosen per VM
//!
//! §5.4.5 compares run-time packages: package name and defining loader.
//! [`same_runtime_package_for_override`] is the predicate [`can_override`]
//! uses, and it has two policies, chosen per store (hence per VM) by
//! `ClassStore::override_packages_by_loader`, which
//! `ClassManager::set_compatibility_mode` installs:
//!
//! * `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`, and a store no VM configured
//!   (`ClassManager::new` before `set_compatibility_mode`): the package NAME
//!   only — [`same_runtime_package`]. CratonVM's `ClassLoaderId` for a
//!   class is not always the loader HotSpot would record, and a false
//!   "different loader" would make a real override stop overriding.
//! * every configured VM, strict and `--compatible` alike (the owner turned it
//!   on for `--compatible` on 2026-09-27): the name, and the defining loader wherever the
//!   recorded loader ids can be trusted to tell two loaders apart — when
//!   either class is from a user-defined loader. A package-private method of
//!   an application class is then not overridden by a same-named package's
//!   class in a child loader (JVMS 5.4.5; the probe `R12Hunt3PkgPrivate`, page
//!   `r12w4-mega3-override-by-package-name-ignores-the-loader`). The three
//!   BUILT-IN loaders are still treated as one for this question: a class
//!   defined against a bootstrap or platform lookup class (a
//!   `Lookup.defineClass` / hidden class, `BoundMethodHandle` species) is
//!   recorded as `Application` by the native boundary's `< 3` collapse
//!   (`lookup_define.rs::inherit_lookup_loader`), and it overrides
//!   package-private methods of its own package (`BoundMethodHandle.copyWith`).
//!   A user-defined id, on the other hand, is allocated per loader object
//!   (`allocate_loader_id`, from 3) and never aliases a built-in loader.
//!
//! The loader-aware rule is on in `--compatible` too: both modes target
//! HotSpot's answer (AGENTS.md), and the switch is the kill switch for either.
//! One built-in pair IS told apart (round 15 wave 5, lane proxy8;
//! `r12w6-override-runtime-package-residuals` item 2): a bootstrap class and
//! an application or platform class in a package that holds classes appended
//! to the bootstrap search (`-Xbootclasspath/a:`, an agent's `Boot-Class-Path`,
//! `Instrumentation.appendToBootstrapClassLoaderSearch`) and that no named
//! module claims — `ClassStore::is_split_boot_package`, switch
//! `CRATONVM_OVERRIDE_SPLIT_BOOT_PACKAGE`. Its one known wrong answer is a
//! class defined by the `--compatible` `Lookup.defineClass` native against an
//! appended lookup class, which `inherit_lookup_loader` still records
//! `Application` (the hidden-class and `defineClass0` doors already define in
//! the bootstrap namespace). Access checks keep the fully loader-aware
//! `access_control::same_runtime_package`; this is not one.

use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;

use crate::{Class, ClassId, ClassLoaderId, ClassStore};

/// Budget for [`can_override`]'s transitive clause when the caller has none of
/// its own: a real hierarchy is far shallower, and running out answers "does
/// override" rather than inventing a non-override.
pub const OVERRIDE_WALK_BUDGET: usize = 512;

#[inline]
fn is_private(m: &ClassFileMethod) -> bool {
    m.access_flags.contains(MethodAccessFlags::PRIVATE)
}

#[inline]
fn is_public_or_protected(m: &ClassFileMethod) -> bool {
    m.access_flags
        .intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PROTECTED)
}

fn package_of(name: &str) -> &str {
    name.rfind('/').map_or("", |i| &name[..i])
}

/// The package part of a class's name. A hidden class (JEP 371) is named
/// `<binary name>/<suffix>`; its package is its host's, so the suffix is cut
/// first.
pub fn class_package(class: &Class) -> &str {
    let name: &str = &class.name;
    if class.is_hidden() {
        if let Some(i) = name.rfind('/') {
            return package_of(&name[..i]);
        }
    }
    package_of(name)
}

/// Same run-time package, by package NAME only — the policy of a store no VM
/// configured and of `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` (either mode);
/// see the module docs for why it does not compare the defining loader.
pub fn same_runtime_package(a: &Class, b: &Class) -> bool {
    class_package(a) == class_package(b)
}

/// Could `a` and `b` have the same defining loader, as far as the recorded
/// loader ids can tell? Two user-defined ids are the same loader only when
/// equal, and a user-defined loader is never a built-in one; the three
/// built-in loaders are not told apart (module docs).
#[inline]
fn same_loader_for_override(a: ClassLoaderId, b: ClassLoaderId) -> bool {
    match (a, b) {
        (ClassLoaderId::UserDefined(x), ClassLoaderId::UserDefined(y)) => x == y,
        (ClassLoaderId::UserDefined(_), _) | (_, ClassLoaderId::UserDefined(_)) => false,
        _ => true,
    }
}

/// [`same_loader_for_override`] for two classes known to share a package
/// NAME, told apart in one more case: a bootstrap class and an application or
/// platform class whose package holds classes appended to this store's
/// bootstrap search (`ClassStore::is_split_boot_package`). HotSpot defines
/// such a package twice, once per loader, so the two are different run-time
/// packages. Every other built-in pair stays "same" (module docs). Round 15
/// wave 5 (lane proxy8; `r12w6-override-runtime-package-residuals` item 2).
#[inline]
fn same_loader_for_override_in(store: &ClassStore, a: &Class, b: &Class) -> bool {
    same_defining_loader_in(store, a.loader_id, b.loader_id, class_package(a))
}

/// [`same_loader_for_override_in`] by loader ids, for two classes of the
/// package NAME `package` (one of them possibly still being defined): could
/// `a` and `b` be the same defining loader, as far as the recorded ids can
/// tell? The three built-in loaders are one (module docs: a class defined
/// against a bootstrap or platform lookup class, or a platform-module class,
/// may be recorded `Application` or `Bootstrap` where HotSpot has one
/// loader), except in a split boot package. The define-time supertype access
/// check and the final-method override check ask it too, so a built-in
/// mislabel never refuses a class HotSpot defines (JIT round 20 lane rev14).
pub fn same_defining_loader_in(
    store: &ClassStore,
    a: ClassLoaderId,
    b: ClassLoaderId,
    package: &str,
) -> bool {
    match (a, b) {
        (ClassLoaderId::Bootstrap, ClassLoaderId::Application | ClassLoaderId::Extension)
        | (ClassLoaderId::Application | ClassLoaderId::Extension, ClassLoaderId::Bootstrap) => {
            !store.is_split_boot_package(package)
        }
        (x, y) => same_loader_for_override(x, y),
    }
}

/// The run-time package comparison §5.4.5 overriding uses in `store`: by name
/// only ([`same_runtime_package`]) unless the store's policy compares loaders
/// (`ClassStore::override_packages_by_loader`, both modes) — see the module
/// docs.
pub fn same_runtime_package_for_override(store: &ClassStore, a: &Class, b: &Class) -> bool {
    let answer = same_runtime_package(a, b)
        && (!store.override_packages_by_loader() || same_loader_for_override_in(store, a, b));
    if runtime_package_crosscheck() {
        report_runtime_package_disagreement(a, b, answer);
    }
    answer
}

/// `CRATONVM_DBG_RUNTIME_PACKAGE_CROSSCHECK` (default off, diagnostic only;
/// proposal PX8-3's first step, `jit-r15w5-proxy8-proposals-RETIRED-20261003.md`). On, every
/// [`same_runtime_package_for_override`] answer is compared with the access
/// checks' `access_control::same_runtime_package` for the same pair, and each
/// disagreement is printed as one `[rtpkg-xcheck]` line on stderr, to size the
/// population a single per-class run-time package identity would change
/// before designing it. Answers are unchanged either way.
fn runtime_package_crosscheck() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_RUNTIME_PACKAGE_CROSSCHECK");
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// The `[rtpkg-xcheck]` line for one pair the two run-time package predicates
/// answer differently; silent when they agree. One line per evaluation (no
/// dedup state: `sort | uniq -c` the output). Round 16 wave 13 (lane annorder).
fn report_runtime_package_disagreement(a: &Class, b: &Class, override_answer: bool) {
    let access_answer = crate::access_control::same_runtime_package(a, b);
    if access_answer != override_answer {
        eprintln!(
            "[rtpkg-xcheck] override={override_answer} access={access_answer} a={} ({:?}) b={} ({:?})",
            a.name, a.loader_id, b.name, b.loader_id
        );
    }
}

/// JVMS §5.4.5: can `mc` (declared in `c_id`) override `ma` (declared in
/// `a_id`)? Name and descriptor are assumed equal (callers look `mc` up by
/// `ma`'s). `budget` bounds the transitive clause; see
/// [`OVERRIDE_WALK_BUDGET`].
///
/// `mc` overrides `ma` iff neither is private, `mc` is not static, and either
/// `ma` is public or protected, or the two share a run-time package, or (the
/// transitive clause) `mc` overrides some `mb` declared in a class strictly
/// between C and A that itself overrides `ma`.
pub fn can_override(
    store: &ClassStore,
    mc: &ClassFileMethod,
    c_id: ClassId,
    ma: &ClassFileMethod,
    a_id: ClassId,
    budget: &mut usize,
) -> bool {
    if is_private(mc) || mc.is_static() || is_private(ma) {
        return false;
    }
    if is_public_or_protected(ma) {
        return true;
    }
    let (Some(c), Some(a)) = (store.get(c_id), store.get(a_id)) else {
        // Unknown classes: do not invent a non-override.
        return true;
    };
    if same_runtime_package_for_override(store, c, a) {
        return true;
    }
    // (b) mC overrides mB, and mB overrides mA, for some mB declared in a
    // class strictly between C and A.
    let mut cur = c.superclass;
    while let Some(b_id) = cur {
        if b_id == a_id {
            break;
        }
        if *budget == 0 {
            return true;
        }
        *budget -= 1;
        let Some(b) = store.get(b_id) else {
            return true;
        };
        if let Some(mb) = b.find_method(&ma.name, &ma.descriptor) {
            if can_override(store, mb, b_id, ma, a_id, budget)
                && can_override(store, mc, c_id, mb, b_id, budget)
            {
                return true;
            }
        }
        cur = b.superclass;
    }
    false
}
