// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMS §5.4.6 method selection for `invokevirtual` / `invokeinterface`, and
//! the two §6.5 linkage checks the invoke opcodes owe before selecting: the
//! resolved method's `static` flag against the opcode, and `invokeinterface`'s
//! "the receiver's class implements the resolved interface".
//!
//! # Why a rule of its own, and not `find_method_recursive`
//!
//! `classloading::find_method_recursive` is a *lenient lookup*. It matches on
//! name and descriptor only, so it
//!
//! * treats a `static` or `private` method in a subclass as an override;
//! * ignores §5.4.5 — a package-private method is "overridden" by a
//!   same-signature method in another package;
//! * walks PAST an abstract re-declaration to a concrete ancestor, where
//!   HotSpot throws `AbstractMethodError`;
//! * breaks a tie between unrelated superinterface defaults by BFS order, where
//!   HotSpot throws `IncompatibleClassChangeError`, and lets an abstract
//!   re-declaration in a sub-interface fail to mask the super-interface
//!   default.
//!
//! Several of those leniencies are load-bearing for CratonVM's compatibility
//! shapes — objects stamped with an abstract or interface class whose methods
//! live in registered natives, classes fabricated without class bytes — so the
//! lookup itself is not changed. [`select`] implements the rule and answers
//! [`Selection::Lenient`] whenever it cannot speak for a real JVM:
//!
//! * a class in the receiver's superclass chain or superinterface closure has
//!   no real class bytes (`ClassOrigin::has_real_bytes`), which covers every
//!   compatibility stub, lambda proxy, `$Proxy` and VM-internal shape;
//! * the receiver's class is abstract or an interface — no real JVM has an
//!   instance of one, only a CratonVM compatibility shape can;
//! * the resolved method cannot be found, or is `static` (the static-flag
//!   check below owns that case), or the walk exceeds its node budget.
//!
//! Callers keep `find_method_recursive`'s answer for `Lenient`, so the default
//! `compatible` behaviour for synthetic shapes is byte-for-byte unchanged, and
//! a hierarchy made of real class files gets the HotSpot answer.
//!
//! # Runtime packages: per-VM policy
//!
//! §5.4.5 compares *run-time* packages: package name AND defining loader.
//! `method_override::same_runtime_package_for_override` (in
//! `cratonvm_classloading`, shared with the vtable build) answers it under the
//! store's policy: by package name only under `--compatible` (unchanged), and
//! by name and defining loader under a strict (`--real-jdk`) VM wherever a
//! user-defined loader is involved (the built-in loaders are not told apart;
//! see that module's docs). The policy is a field of the `ClassStore`, and a
//! change of it moves the store's `class_definition_epoch`, which
//! [`SELECT_MEMO`] entries are stamped with.
//!
//! # Memoization
//!
//! Every answer here is a pure function of the class store for fixed
//! `(receiver class, resolved method)`; a class's hierarchy is fixed at load
//! time. Callers therefore consult it at cache-FILL time
//! (`populate_virtual_invoke_cache`, the vtable fast path) or behind the
//! per-thread `IfaceSelectSiteCache`, never per cache hit. The one per-call
//! consumer, the slow invoke path (`try_stackless_invoke`), runs on every call
//! of a site no cache serves, so [`select`] itself is memoized per thread and
//! per `(store, receiver class, resolved method)` — see [`SELECT_MEMO`].

// JVMS §5.4.5, with the per-VM run-time package policy, lives in
// `cratonvm_classloading::method_override` because the vtable build
// (`ClassManager::build_vtable_descriptors_with_overrides`) must answer the
// same question the same way; two copies disagreed before.
use cratonvm_classloading::method_override::can_override;
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;

use crate::classloading::{Class, ClassId, ClassManager, ClassStore};

/// Upper bound on the classes one selection visits. A real hierarchy is far
/// smaller; the bound keeps a corrupt (cyclic) store from spinning, and running
/// out degrades to [`Selection::Lenient`], never to an error.
const MAX_SELECTION_NODES: usize = 512;

/// The outcome of JVMS §5.4.6 selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Selection {
    /// The method declared in this class is the one to invoke.
    Selected(ClassId),
    /// The selected method is abstract (`Some(declaring class)`), or nothing
    /// was selected and no maximally-specific superinterface method is
    /// concrete (`None`): `AbstractMethodError`.
    AbstractMethod(Option<ClassId>),
    /// Several maximally-specific superinterface methods are concrete:
    /// `IncompatibleClassChangeError` ("Conflicting default methods"). The two
    /// named are the first two in closure order.
    ConflictingDefaults(ClassId, ClassId),
    /// An `InterfaceMethodref` whose selection found, in the receiver's class
    /// chain, a method that is neither public nor private (package-private or
    /// protected): `IllegalAccessError` (JVMS 6.5 `invokeinterface`;
    /// HotSpot's `runtime_resolve_interface_method` checks it before
    /// `abstract`). Interpreter round i1 wave 30.
    NotPublic(ClassId),
    /// The strict rule does not apply here (see the module docs); keep the
    /// lenient `find_method_recursive` answer.
    Lenient,
}

/// What the invoke instruction resolved, as far as selection needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedRef {
    /// A `Methodref`: the resolved method is the one declared in this class
    /// (the result of [`resolve_declaring`]). Its access flags and package
    /// decide which methods can override it (§5.4.5).
    Declared(ClassId),
    /// An `InterfaceMethodref`, naming this interface when the caller knows it
    /// (it is used only to word an error message). An interface method is
    /// public (a private one is pinned by the caller before selection), so its
    /// declaring class never decides overriding and does not need resolving.
    Interface(Option<ClassId>),
}

#[inline]
fn is_private(m: &ClassFileMethod) -> bool {
    m.access_flags.contains(MethodAccessFlags::PRIVATE)
}

/// Slots in the per-thread [`hierarchy_is_real`] memo (direct-mapped).
const REAL_MEMO_SLOTS: usize = 128;

/// A store's identity for the per-thread memos below: its layout domain, which
/// `ClassStore::new` draws from a process-wide counter, so no two stores ever
/// share one. The store's ADDRESS is not an identity: a store dropped and a
/// new one built in its place (a second VM on the same thread, or the next
/// unit test a harness thread runs, each numbering its classes from 0) would
/// redeem the first one's answers.
#[inline]
fn store_identity(store: &ClassStore) -> usize {
    store.layout_domain() as usize
}

thread_local! {
    /// `(store identity, class id) -> (class_origin_epoch, answer)`.
    ///
    /// `hierarchy_is_real` walks the receiver's whole superclass chain and
    /// superinterface closure, and the slow invoke path asks it on every call
    /// of a site that is never cached (a megamorphic site past the poly
    /// cache's cap). The answer is a pure function of the hierarchy, which is
    /// fixed at load time; what can change it in place — a stub promoted to
    /// real bytes, or a class defined later that a missing ancestor lookup
    /// could now find — bumps `class_origin_epoch`, which is half the stamp.
    /// The other half is the store's hierarchy stamp
    /// ([`select_memo_hierarchy_stamp`]): an edge change on an existing class
    /// (`set_superclass` / `set_interfaces` re-parenting onto a stub, a
    /// removal) moves neither `class_origin_epoch` nor anything else this memo
    /// read before wave 94, so a stale `true` could let strict selection run
    /// over a hierarchy that now holds a stub (interpreter round i4 wave 94,
    /// lane L7). The store identity ([`store_identity`]) keeps two VMs on one
    /// thread apart. Entry: `(store, class, hierarchy stamp, origin stamp,
    /// answer)`.
    static REAL_MEMO: std::cell::RefCell<Vec<(usize, u32, u64, u64, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Every class and interface reachable from `start` (superclass chain plus
/// superinterface closure) has real class bytes, and none is missing.
/// Memoized per thread; see [`REAL_MEMO`].
fn hierarchy_is_real(store: &ClassStore, start: ClassId) -> bool {
    let store_key = store_identity(store);
    let class_key = start.as_u32();
    // Read BEFORE the walk (a change during it stamps the entry stale).
    let edges = select_memo_hierarchy_stamp(store);
    let epoch = select_memo_origin_stamp(store);
    let slot = (class_key as usize ^ store_key.wrapping_mul(0x9E37)) & (REAL_MEMO_SLOTS - 1);
    let hit = REAL_MEMO.with(|memo| {
        memo.borrow().get(slot).and_then(|&(s, c, h, e, answer)| {
            (s == store_key && c == class_key && h == edges && e == epoch).then_some(answer)
        })
    });
    if let Some(answer) = hit {
        return answer;
    }
    let answer = hierarchy_is_real_walk(store, start);
    if store.select_memo_edge_epoch() && store.get(start).is_none() {
        // A later `add` at `start` moves neither edge-mode stamp.
        return answer;
    }
    REAL_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if memo.is_empty() {
            memo.resize(REAL_MEMO_SLOTS, (0, u32::MAX, u64::MAX, u64::MAX, false));
        }
        memo[slot] = (store_key, class_key, edges, epoch, answer);
    });
    answer
}

fn hierarchy_is_real_walk(store: &ClassStore, start: ClassId) -> bool {
    let mut stack: Vec<ClassId> = vec![start];
    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(id) = stack.pop() {
        if seen.contains(&id) {
            continue;
        }
        if seen.len() >= MAX_SELECTION_NODES {
            return false;
        }
        seen.push(id);
        let Some(class) = store.get(id) else {
            return false;
        };
        if !class.origin.has_real_bytes() {
            return false;
        }
        if let Some(super_id) = class.superclass {
            stack.push(super_id);
        }
        stack.extend_from_slice(&class.interfaces);
    }
    true
}

/// The superinterface closure of `start`: every interface a class in its
/// superclass chain (or, for an interface, `start` itself) lists, transitively.
/// `start` is not included. `None` on a missing class or an exhausted budget.
///
/// The order is HotSpot's `HierarchyVisitor` walk (`defaultMethods.cpp`):
/// depth-first pre-order, a class's superclass before its interfaces, the
/// interfaces in declaration order. It is observable — the "Conflicting
/// default methods" message lists the candidates in this order.
pub(crate) fn superinterface_closure(store: &ClassStore, start: ClassId) -> Option<Vec<ClassId>> {
    let mut stack: Vec<ClassId> = vec![start];
    let mut visited: Vec<ClassId> = Vec::new();
    let mut out: Vec<ClassId> = Vec::new();
    while let Some(id) = stack.pop() {
        if visited.contains(&id) {
            continue;
        }
        if visited.len() >= MAX_SELECTION_NODES {
            return None;
        }
        visited.push(id);
        let class = store.get(id)?;
        if id != start && class.is_interface() {
            out.push(id);
        }
        // LIFO: push the interfaces last-first, and the superclass after
        // them, so the superclass is walked first.
        stack.extend(class.interfaces.iter().rev().copied());
        if !class.is_interface() {
            if let Some(super_id) = class.superclass {
                stack.push(super_id);
            }
        }
    }
    Some(out)
}

/// Is `sup` a strict superinterface of `sub`?
fn is_superinterface_of(store: &ClassStore, sup: ClassId, sub: ClassId) -> bool {
    let mut stack: Vec<ClassId> = match store.get(sub) {
        Some(class) => class.interfaces.clone(),
        None => return false,
    };
    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(id) = stack.pop() {
        if id == sup {
            return true;
        }
        if seen.contains(&id) || seen.len() >= MAX_SELECTION_NODES {
            continue;
        }
        seen.push(id);
        if let Some(class) = store.get(id) {
            stack.extend_from_slice(&class.interfaces);
        }
    }
    false
}

/// JVMS §5.4.3.3: the maximally-specific superinterface methods of `start`
/// matching `(name, desc)` — non-private, non-static declarations in its
/// superinterface closure for which no other candidate is declared in a
/// subinterface. Abstract ones are included (an abstract re-declaration in a
/// sub-interface masks the super-interface default). Each entry is
/// `(interface, is_abstract)`, in closure order.
pub(crate) fn maximally_specific(
    store: &ClassStore,
    start: ClassId,
    name: &str,
    desc: &str,
) -> Option<Vec<(ClassId, bool)>> {
    let closure = superinterface_closure(store, start)?;
    let candidates: Vec<(ClassId, bool)> = closure
        .iter()
        .filter_map(|&id| {
            let m = store.get(id)?.find_method(name, desc)?;
            (!is_private(m) && !m.is_static()).then_some((id, m.is_abstract()))
        })
        .collect();
    Some(
        candidates
            .iter()
            .filter(|&&(id, _)| {
                !candidates
                    .iter()
                    .any(|&(other, _)| other != id && is_superinterface_of(store, id, other))
            })
            .copied()
            .collect(),
    )
}

/// JVMS §5.4.3.3 (a class) / §5.4.3.4 (an interface) method resolution in
/// `owner`: the class declaring the resolved method. `None` when it does not
/// resolve through the class store — `NoSuchMethodError` is decided elsewhere,
/// and a signature-polymorphic reference lands here too.
///
/// Unlike `find_method_recursive` this returns the FIRST declaration on the
/// superclass chain whatever its flags, which is what resolution is: an
/// abstract or static declaration is still the resolved method.
pub(crate) fn resolve_declaring(
    store: &ClassStore,
    owner: ClassId,
    name: &str,
    desc: &str,
) -> Option<ClassId> {
    let owner_class = store.get(owner)?;
    if owner_class.is_interface() {
        if owner_class.find_method(name, desc).is_some() {
            return Some(owner);
        }
        // §5.4.3.4 step 3: a public instance method of `java.lang.Object`.
        if let Some(object_id) = owner_class.superclass {
            if let Some(m) = store.get(object_id).and_then(|o| o.find_method(name, desc)) {
                if m.access_flags.contains(MethodAccessFlags::PUBLIC) && !m.is_static() {
                    return Some(object_id);
                }
            }
        }
    } else {
        let mut cur = Some(owner);
        let mut steps = 0usize;
        while let Some(id) = cur {
            steps += 1;
            if steps > MAX_SELECTION_NODES {
                return None;
            }
            let class = store.get(id)?;
            if class.find_method(name, desc).is_some() {
                return Some(id);
            }
            cur = class.superclass;
        }
    }
    let maximal = maximally_specific(store, owner, name, desc)?;
    maximal
        .iter()
        .find(|&&(_, is_abstract)| !is_abstract)
        .or_else(|| maximal.first())
        .map(|&(id, _)| id)
}

/// An interface method reference that §5.4.3.4 does not resolve although
/// `java.lang.Object` declares the name and descriptor: `Object`'s method is
/// protected or static (`clone()`, `finalize()`), and step 3 takes only a
/// public instance method of `Object`. HotSpot throws `NoSuchMethodError`
/// there; before interpreter round i1 wave 32 `Object.clone` was dispatched.
///
/// `false` for a class owner, for a reference that resolves, and for a name
/// `Object` does not declare (the ordinary `NoSuchMethodError` paths).
pub(crate) fn interface_ref_names_a_non_public_object_method(
    store: &ClassStore,
    owner: ClassId,
    name: &str,
    desc: &str,
) -> bool {
    let Some(owner_class) = store.get(owner) else {
        return false;
    };
    if !owner_class.is_interface() {
        return false;
    }
    let Some(object) = owner_class.superclass.and_then(|id| store.get(id)) else {
        return false;
    };
    let hidden = object
        .find_method(name, desc)
        .is_some_and(|m| !m.access_flags.contains(MethodAccessFlags::PUBLIC) || m.is_static());
    hidden && resolve_declaring(store, owner, name, desc).is_none()
}

/// A method reference that JVMS §5.4.3.3 / §5.4.3.4 does NOT resolve, although
/// a superinterface of `owner` declares a STATIC method of that name and
/// descriptor. Static interface methods are not inherited: neither a class
/// nor a subinterface resolves them (maximally-specific candidates are
/// non-static), so `invokestatic C.m` / `invokestatic J.m` /
/// `invokevirtual C.m` / `invokeinterface J.m` with `m` static in `I` are
/// HotSpot's `NoSuchMethodError: 'int C.m()'`. The lenient walks found the
/// static `I.m` and ran it (interpreter round i4 wave 98, lane L7; probe
/// `L7W98StaticShadowDispatch.java`, rows s1-s4).
///
/// `false` whenever the store cannot decide ([`member_provably_absent`]), for
/// a reference that resolves, and when no superinterface declares such a
/// static: the ordinary `NoSuchMethodError` paths keep those.
pub(crate) fn reference_names_only_an_uninherited_static_interface_method(
    store: &ClassStore,
    owner: ClassId,
    name: &str,
    desc: &str,
) -> bool {
    if name == "<init>" || name == "<clinit>" || !member_provably_absent(store, owner, name, desc) {
        return false;
    }
    superinterface_closure(store, owner).is_some_and(|closure| {
        closure.iter().any(|&id| {
            store
                .get(id)
                .and_then(|c| c.find_method(name, desc))
                .is_some_and(|m| m.is_static())
        })
    })
}

/// What JVMS §6.5 `invokespecial` selects, as HotSpot's
/// `LinkResolver::runtime_resolve_special_method` does, where the interpreter's
/// lenient walk (`find_method_recursive` from
/// `invokespecial_selection_start`) would answer something else.
pub(crate) enum SpecialSelection {
    /// Keep the lenient walk: the shapes agree, or this cannot decide.
    Unchanged,
    /// Start the lookup at this class instead: the first INSTANCE declaration
    /// on the caller's superclass chain (a static one is skipped) or the one
    /// maximally-specific default.
    Owner(ClassId),
    /// `AbstractMethodError`, with HotSpot's message.
    AbstractMethod(String),
    /// `IncompatibleClassChangeError` (conflicting defaults), with HotSpot's
    /// message.
    IncompatibleClassChange(String),
}

/// JVMS §6.5 `invokespecial` selection for `caller` calling
/// `cp_class.name desc` (interpreter round i1 wave 32; item 1 of
/// `i29-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot`).
///
/// * When the reference names a proper superclass of the caller (not an
///   interface, not a constructor), the method is looked up from the caller's
///   direct superclass: the first non-static declaration on that chain,
///   whatever else its flags (a private one too), then the superclass's one
///   maximally-specific default. An abstract declaration is an
///   `AbstractMethodError` naming it; conflicting defaults are the
///   `IncompatibleClassChangeError`; nothing at all is an
///   `AbstractMethodError` naming the resolved method.
/// * Otherwise the resolved method is the selected one, and an abstract one
///   declared in the named class or interface (`I.super.m()` where `I`
///   re-declares `m` abstract) is an `AbstractMethodError`.
///
/// [`SpecialSelection::Unchanged`] for a constructor, a reference that does
/// not resolve here, and any hierarchy with a class that has no real class
/// bytes (a compatibility stub's flags are not evidence). A static resolved
/// method is `static_flag_mismatch`'s, not this function's.
pub(crate) fn select_special(
    store: &ClassStore,
    caller: ClassId,
    cp_class: ClassId,
    is_interface_ref: bool,
    name: &str,
    desc: &str,
) -> SpecialSelection {
    if name == "<init>" || name == "<clinit>" {
        return SpecialSelection::Unchanged;
    }
    let Some(named) = store.get(cp_class) else {
        return SpecialSelection::Unchanged;
    };
    if !hierarchy_is_real(store, caller) || !hierarchy_is_real(store, cp_class) {
        return SpecialSelection::Unchanged;
    }
    let Some(resolved) = resolve_declaring(store, cp_class, name, desc) else {
        return SpecialSelection::Unchanged;
    };
    let quoted = |holder: ClassId| {
        let holder = store
            .get(holder)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        format!("'{}'", external_method_name(&holder, name, desc))
    };
    let redirect = !is_interface_ref
        && !named.is_interface()
        && caller != cp_class
        && is_strict_superclass(store, cp_class, caller);
    if !redirect {
        let abstract_here = resolved == cp_class
            && named
                .find_method(name, desc)
                .is_some_and(|m| m.is_abstract() && !m.is_static());
        return if abstract_here {
            SpecialSelection::AbstractMethod(quoted(resolved))
        } else {
            SpecialSelection::Unchanged
        };
    }
    let Some(start) = store.get(caller).and_then(|c| c.superclass) else {
        return SpecialSelection::Unchanged;
    };
    let mut cur = Some(start);
    let mut steps = 0usize;
    while let Some(id) = cur {
        steps += 1;
        if steps > MAX_SELECTION_NODES {
            return SpecialSelection::Unchanged;
        }
        let Some(class) = store.get(id) else {
            return SpecialSelection::Unchanged;
        };
        if let Some(m) = class.find_method(name, desc) {
            if !m.is_static() {
                return if m.is_abstract() {
                    SpecialSelection::AbstractMethod(quoted(id))
                } else {
                    SpecialSelection::Owner(id)
                };
            }
        }
        cur = class.superclass;
    }
    let Some(maximal) = maximally_specific(store, start, name, desc) else {
        return SpecialSelection::Unchanged;
    };
    let mut concrete = maximal
        .iter()
        .filter(|&&(_, is_abstract)| !is_abstract)
        .map(|&(id, _)| id);
    match (concrete.next(), concrete.next()) {
        (Some(only), None) => SpecialSelection::Owner(only),
        (Some(first), Some(second)) => SpecialSelection::IncompatibleClassChange(
            conflicting_defaults_message(store, start, first, second, name, desc),
        ),
        (None, _) => SpecialSelection::AbstractMethod(quoted(resolved)),
    }
}

/// j27 wave 5 (lane INVHUNT, page
/// `j27-super-special-splice-walks-past-an-abstract-redeclaration-FIXED-20261029.md`):
/// may a compiled `super.m()` (`caller`'s `invokespecial cp_class.name desc`,
/// not an interface reference) run the body `lenient_declaring` declares, the
/// answer of `find_method_recursive(cp_class, ..)`? `false` where
/// [`select_special`] selects another class or ends in an error (an abstract
/// re-declaration on the way: `AbstractMethodError`; conflicting defaults:
/// `IncompatibleClassChangeError`), which only the call raises; `true` where
/// it agrees or cannot decide ([`SpecialSelection::Unchanged`]).
pub(crate) fn super_special_body_agrees(
    store: &ClassStore,
    caller: ClassId,
    cp_class: ClassId,
    name: &str,
    desc: &str,
    lenient_declaring: ClassId,
) -> bool {
    match select_special(store, caller, cp_class, false, name, desc) {
        SpecialSelection::Unchanged => true,
        SpecialSelection::Owner(owner) => owner == lenient_declaring,
        SpecialSelection::AbstractMethod(_) | SpecialSelection::IncompatibleClassChange(_) => false,
    }
}

/// Is `owner.name desc` provably NOT a member that method resolution finds
/// (JVMS §5.4.3.3 / §5.4.3.4, and a constructor declared in `owner` itself)?
/// `false` whenever the store cannot decide: a hierarchy with a class that has
/// no real bytes, and the signature-polymorphic owners
/// (`MethodHandle`, `VarHandle`), whose members resolve by name alone.
pub(crate) fn member_provably_absent(
    store: &ClassStore,
    owner: ClassId,
    name: &str,
    desc: &str,
) -> bool {
    let Some(class) = store.get(owner) else {
        return false;
    };
    if matches!(
        &*class.name,
        "java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle"
    ) || !hierarchy_is_real(store, owner)
    {
        return false;
    }
    if name == "<init>" {
        return class.find_method(name, desc).is_none();
    }
    resolve_declaring(store, owner, name, desc).is_none()
}

/// Is `sup` a proper superclass of `sub` (the superclass chain only)?
fn is_strict_superclass(store: &ClassStore, sup: ClassId, sub: ClassId) -> bool {
    let mut cur = store.get(sub).and_then(|c| c.superclass);
    let mut steps = 0usize;
    while let Some(id) = cur {
        if id == sup {
            return true;
        }
        steps += 1;
        if steps > MAX_SELECTION_NODES {
            return false;
        }
        cur = store.get(id).and_then(|c| c.superclass);
    }
    false
}

/// The [`ResolvedRef`] for a constant-pool method reference whose owner
/// resolved to `owner`: an interface owner needs nothing more, a class owner
/// is resolved to its declaring class. `None` when it does not resolve.
pub(crate) fn resolved_ref_for_owner(
    store: &ClassStore,
    owner: ClassId,
    name: &str,
    desc: &str,
) -> Option<ResolvedRef> {
    if store.get(owner)?.is_interface() {
        return Some(ResolvedRef::Interface(Some(owner)));
    }
    resolve_declaring(store, owner, name, desc).map(ResolvedRef::Declared)
}

/// Slots in the per-thread [`select`] memo (direct-mapped).
const SELECT_MEMO_SLOTS: usize = 256;

/// One memoized [`select`] answer. The name and descriptor are owned copies
/// compared in full on every probe: the slot index samples them, and a wrong
/// hit here would run another method's body.
struct SelectMemoEntry {
    store: usize,
    receiver: u32,
    resolved: ResolvedRef,
    name: Box<str>,
    desc: Box<str>,
    /// `(the store's class_definition_epoch, class_origin_epoch,
    /// class_redefinition_count or 0)` read before the walk (the third is 0
    /// unless [`SELECT_MEMO_STAMPS_REDEFINITION`]). Under
    /// `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH` the first two are the store's
    /// hierarchy-edge epoch and `class_origin_in_place_epoch`
    /// ([`select_memo_hierarchy_stamp`], [`select_memo_origin_stamp`]).
    epochs: (u64, u64, u64),
    answer: Selection,
}

thread_local! {
    /// `(store identity, receiver, resolved method) -> Selection`, per thread.
    ///
    /// The slow invoke path selects on every call of a site no cache serves
    /// (`try_stackless_invoke` step 4), and a megamorphic site past the poly
    /// cache's cap then selects again in `populate_virtual_invoke_cache` —
    /// each a superclass walk with `can_override` per match and, when the
    /// chain has no declaration, a superinterface closure built in fresh
    /// `Vec`s. The answer is a pure function of the hierarchy for a fixed key.
    /// What can change it in place bumps one of the two stamps: a superclass
    /// or interface edge, or a name defined anew (`class_definition_epoch`),
    /// and a class's provenance, as when a stub is promoted to its real
    /// bytes with a new method table (`class_origin_epoch`). A JVMTI redefine
    /// keeps a class's method set, flags and order (`redefine_class` refuses
    /// a change and re-orders the new bodies to the old table), and its
    /// superclass and interfaces, so it cannot change an answer: since wave
    /// 22 it is not stamped ([`SELECT_MEMO_STAMPS_REDEFINITION`]). It used to
    /// bypass the memo for the rest of the process once any class was
    /// redefined (until wave 18), then to retire every thread's memo on every
    /// redefinition (waves 18-21).
    static SELECT_MEMO: std::cell::RefCell<Vec<Option<SelectMemoEntry>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Kill switch (interpreter round i1 wave 18, lane L4): `false` restores the
/// historical bypass of [`SELECT_MEMO`] for the rest of the process after the
/// first redefinition of any class; `true` stamps entries with the
/// redefinition count instead.
const SELECT_MEMO_SURVIVES_REDEFINITION: bool = true;

/// Kill switch (interpreter round i1 wave 22, lane L4): `true` stamps
/// [`SELECT_MEMO`] entries with `class_redefinition_count`, so every
/// redefinition anywhere retires every thread's memo (waves 18-21). `false`:
/// no redefinition stamp. A selection is a function of the hierarchy, the
/// method sets and their modifiers, all of which `ClassManager::redefine_class`
/// refuses to change (superclass, interfaces, the method set and each
/// method's modifiers are compared before any mutation), so a stamp only
/// cost a re-walk of up to [`SELECT_MEMO_SLOTS`] keys per thread per
/// redefinition — a Mockito-inline suite retransforms on every first mock.
const SELECT_MEMO_STAMPS_REDEFINITION: bool = false;

/// Slot index for [`SELECT_MEMO`]: the receiver, the store, the two lengths and
/// a few sampled bytes. Cheap on purpose; the entry compares everything.
#[inline]
fn select_memo_slot(store: usize, receiver: u32, name: &str, desc: &str) -> usize {
    let n = name.as_bytes();
    let d = desc.as_bytes();
    let mut h = (receiver as usize).wrapping_mul(0x9E37_79B9) ^ store.wrapping_mul(0x85EB);
    h ^= n.len().wrapping_shl(3) ^ d.len().wrapping_shl(11);
    if let Some(b) = n.first() {
        h ^= (*b as usize) << 5;
    }
    if let Some(b) = n.last() {
        h ^= (*b as usize) << 13;
    }
    if let Some(b) = d.get(1) {
        h ^= (*b as usize) << 17;
    }
    if let Some(b) = d.last() {
        h ^= (*b as usize) << 19;
    }
    (h ^ (h >> 9)) & (SELECT_MEMO_SLOTS - 1)
}

/// The hierarchy half of the [`SELECT_MEMO`] / [`REAL_MEMO`] stamps: this
/// store's class-definition epoch (every definition and every edge change),
/// or, under `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH`
/// (`ClassStore::select_memo_edge_epoch`; interpreter round i4 wave 94, lane
/// L7), its hierarchy-edge epoch (edge changes only).
///
/// Why the edge epoch is enough. [`select_uncached`] and
/// [`hierarchy_is_real_walk`] read only classes stored in `store`, by id:
/// their superclass and interface edges, method tables and flags, defining
/// loaders and names (inside `can_override`), the store's override policy and
/// split boot packages. They read no name -> class answer. What can change any
/// of that for an id already stored, and its stamp:
///
/// * an edge of an existing class (`set_superclass`, `set_interfaces`), a
///   forward edge coming into existence (`add` of an id some stored class
///   already names), a removal, a policy or split-package change: each runs
///   `class::note_hierarchy_edge_change`, which moves the edge epoch;
/// * the array-class loader re-tag in `ClassManager`: moves the edge epoch;
/// * a stub promoted in place to real bytes (new method table, flags,
///   loader, edges): `set_origin` moves `class_origin_in_place_epoch`, the
///   other half of the stamp ([`select_memo_origin_stamp`]), and the edges it
///   rewrites move the edge epoch;
/// * a JVMTI redefinition: changes no input (see [`SELECT_MEMO`]).
///
/// A brand-new class cannot be in an existing class's hierarchy except
/// through a forward edge, and its own id has no entry, unless an entry was
/// filled for the id while nothing was stored there: [`select`] does not
/// memoise such an answer under the edge stamp ([`select_key_classes_stored`]),
/// and [`hierarchy_is_real_walk`] answers `false` for a missing ancestor only
/// when it is reached through an edge (a forward edge, covered above) or is
/// the start id itself (not memoised either, see [`hierarchy_is_real`]).
#[inline]
fn select_memo_hierarchy_stamp(store: &ClassStore) -> u64 {
    let epochs = cratonvm_classloading::store_epochs(store.layout_domain());
    if store.select_memo_edge_epoch() {
        epochs.hierarchy_edge_epoch()
    } else {
        epochs.class_definition_epoch()
    }
}

/// The provenance half of the [`SELECT_MEMO`] / [`REAL_MEMO`] stamps:
/// `class_origin_epoch` (moves on every definition too, process-wide), or,
/// under `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH`, `class_origin_in_place_epoch`
/// (only provenance writes that may land on a stored class). See
/// [`select_memo_hierarchy_stamp`].
#[inline]
fn select_memo_origin_stamp(store: &ClassStore) -> u64 {
    if store.select_memo_edge_epoch() {
        cratonvm_classloading::store_epochs(store.layout_domain()).class_origin_in_place_epoch()
    } else {
        cratonvm_classloading::class_origin_epoch()
    }
}

/// The ids a [`select`] key names have classes stored behind them (the
/// receiver, and the declaring class of a `Methodref`).
#[inline]
fn select_key_classes_stored(store: &ClassStore, receiver: ClassId, resolved: ResolvedRef) -> bool {
    store.get(receiver).is_some()
        && match resolved {
            ResolvedRef::Declared(declaring) => store.get(declaring).is_some(),
            ResolvedRef::Interface(_) => true,
        }
}

/// JVMS §5.4.6: select the method `receiver`'s class runs for the resolved
/// method `resolved`. See the module docs for when this answers
/// [`Selection::Lenient`].
///
/// Memoized per thread and per receiver class ([`SELECT_MEMO`]); the walk is
/// [`select_uncached`].
pub(crate) fn select(
    store: &ClassStore,
    receiver: ClassId,
    resolved: ResolvedRef,
    name: &str,
    desc: &str,
) -> Selection {
    if !SELECT_MEMO_SURVIVES_REDEFINITION && cratonvm_classloading::any_class_redefined() {
        return select_uncached(store, receiver, resolved, name, desc);
    }
    let store_key = store_identity(store);
    let receiver_key = receiver.as_u32();
    // Read BEFORE the walk, so a change that lands during it stamps the entry
    // stale rather than current. The definition epoch is this store's own
    // (`StoreEpochs`, interpreter round i1 wave 21): every edge change and
    // definition in `store` moves it, and another VM's do not. Entries are
    // keyed by `store_key`, so a stamp is only ever compared with a stamp
    // read from the same slot.
    let epochs = (
        select_memo_hierarchy_stamp(store),
        select_memo_origin_stamp(store),
        if SELECT_MEMO_STAMPS_REDEFINITION {
            cratonvm_classloading::class_redefinition_count()
        } else {
            0
        },
    );
    let idx = select_memo_slot(store_key, receiver_key, name, desc);
    let hit = SELECT_MEMO.with(|memo| {
        let memo = memo.borrow();
        let entry = memo.get(idx)?.as_ref()?;
        (entry.store == store_key
            && entry.receiver == receiver_key
            && entry.resolved == resolved
            && entry.epochs == epochs
            && &*entry.name == name
            && &*entry.desc == desc)
            .then_some(entry.answer)
    });
    if let Some(answer) = hit {
        return answer;
    }
    let answer = select_uncached(store, receiver, resolved, name, desc);
    if store.select_memo_edge_epoch() && !select_key_classes_stored(store, receiver, resolved) {
        // Under the edge stamp a later `add` of the receiver's (or the
        // resolved declaring class's) id moves neither stamp, so an answer
        // about an id with no class behind it is not memoised. Ids are not
        // handed out twice, so this is a race-only case.
        return answer;
    }
    SELECT_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if memo.is_empty() {
            memo.resize_with(SELECT_MEMO_SLOTS, || None);
        }
        memo[idx] = Some(SelectMemoEntry {
            store: store_key,
            receiver: receiver_key,
            resolved,
            name: name.into(),
            desc: desc.into(),
            epochs,
            answer,
        });
    });
    answer
}

/// [`select`]'s walk, unmemoized.
fn select_uncached(
    store: &ClassStore,
    receiver: ClassId,
    resolved: ResolvedRef,
    name: &str,
    desc: &str,
) -> Selection {
    let Some(receiver_class) = store.get(receiver) else {
        return Selection::Lenient;
    };
    if receiver_class.is_interface() || receiver_class.is_abstract() {
        return Selection::Lenient;
    }
    let resolved_method: Option<(&ClassFileMethod, ClassId)> = match resolved {
        ResolvedRef::Declared(declaring) => {
            let Some(declaring_class) = store.get(declaring) else {
                return Selection::Lenient;
            };
            if !declaring_class.origin.has_real_bytes() {
                return Selection::Lenient;
            }
            let Some(m) = declaring_class.find_method(name, desc) else {
                return Selection::Lenient;
            };
            if m.is_static() {
                return Selection::Lenient;
            }
            // Step 1: a private resolved method is selected as resolved.
            if is_private(m) {
                return Selection::Selected(declaring);
            }
            Some((m, declaring))
        }
        ResolvedRef::Interface(_) => None,
    };
    if !hierarchy_is_real(store, receiver) {
        return Selection::Lenient;
    }
    // Step 2: the receiver's class, then its superclasses — the first
    // instance method that can override the resolved one. An abstract one is
    // still selected (and is an `AbstractMethodError`); walking past it is the
    // leniency this module exists to remove.
    let mut cur = Some(receiver);
    let mut steps = 0usize;
    while let Some(id) = cur {
        steps += 1;
        if steps > MAX_SELECTION_NODES {
            return Selection::Lenient;
        }
        let Some(class) = store.get(id) else {
            return Selection::Lenient;
        };
        if let Some(m) = class.find_method(name, desc) {
            let overrides = !m.is_static()
                && !is_private(m)
                && match resolved_method {
                    Some((ma, a_id)) => {
                        let mut budget = MAX_SELECTION_NODES;
                        can_override(store, m, id, ma, a_id, &mut budget)
                    }
                    None => true,
                };
            if overrides {
                // `invokeinterface`: a selected method that is neither public
                // nor private is an `IllegalAccessError`, checked before
                // `abstract` as HotSpot does (interpreter round i1 wave 30).
                if matches!(resolved, ResolvedRef::Interface(_))
                    && !m.access_flags.contains(MethodAccessFlags::PUBLIC)
                {
                    return Selection::NotPublic(id);
                }
                return if m.is_abstract() {
                    Selection::AbstractMethod(Some(id))
                } else {
                    Selection::Selected(id)
                };
            }
        }
        cur = class.superclass;
    }
    // Step 3: the maximally-specific superinterface methods.
    let Some(maximal) = maximally_specific(store, receiver, name, desc) else {
        return Selection::Lenient;
    };
    let mut concrete = maximal
        .iter()
        .filter(|&&(_, is_abstract)| !is_abstract)
        .map(|&(id, _)| id);
    match (concrete.next(), concrete.next()) {
        (Some(only), None) => Selection::Selected(only),
        (Some(first), Some(second)) => Selection::ConflictingDefaults(first, second),
        (None, _) => Selection::AbstractMethod(None),
    }
}

/// The dispatch target a cache-FILL site may record: [`select`]'s answer where
/// it applies, `find_method_recursive`'s where it does not, and `None` when
/// selection ends in an error — the slow path must raise it, so nothing may be
/// cached for that receiver.
///
/// `resolved` is `None` when the caller could not resolve the reference (an
/// unloaded owner); that is [`Selection::Lenient`] by definition.
pub(crate) fn select_or_lenient<'a>(
    store: &'a ClassStore,
    receiver: ClassId,
    resolved: Option<ResolvedRef>,
    name: &str,
    desc: &str,
) -> Option<(&'a ClassFileMethod, ClassId)> {
    let selection = match resolved {
        Some(resolved) => select(store, receiver, resolved, name, desc),
        None => Selection::Lenient,
    };
    match selection {
        Selection::Selected(declaring) => store
            .get(declaring)
            .and_then(|class| class.find_method(name, desc))
            .map(|m| (m, declaring)),
        Selection::AbstractMethod(_)
        | Selection::ConflictingDefaults(..)
        | Selection::NotPublic(_) => None,
        Selection::Lenient => {
            crate::classloading::find_method_recursive(receiver, name, desc, store)
        }
    }
}

/// For a door that selects with the lenient walk and cannot raise §5.4.6's
/// errors (`invoke_on_class_shared_inner`'s receiver retarget): `lenient`
/// replaced by [`select`]'s answer when that answer is a strict `Selected`.
/// `Lenient`, `AbstractMethod` and `ConflictingDefaults` keep `lenient`, so
/// the door never starts failing where it answered before; what changes is
/// that a private, static, or cross-package package-private method in the
/// receiver's chain no longer hides the real override.
///
/// `owner` is the class the call named (what the receiver was retargeted
/// FROM). Fast accept, no second walk: an interface owner's resolved method is
/// public, so any non-private concrete instance method is an override, and
/// when the lenient walk's first hit is such a method declared in a class,
/// §5.4.6 step 2 selects exactly it.
pub(crate) fn refine_lenient_selection<'a>(
    store: &'a ClassStore,
    receiver: ClassId,
    owner: ClassId,
    name: &str,
    desc: &str,
    lenient: Option<(&'a ClassFileMethod, ClassId)>,
) -> Option<(&'a ClassFileMethod, ClassId)> {
    if lenient_is_the_selection(store, owner, lenient) {
        return lenient;
    }
    let Some(resolved) = resolved_ref_for_owner(store, owner, name, desc) else {
        return lenient;
    };
    refine_lenient_selection_for(store, receiver, resolved, name, desc, lenient)
}

/// [`refine_lenient_selection`] for a caller that already holds the resolved
/// reference — `execute_invoke_kind`'s receiver-class fallback, which computed
/// it for `try_stackless_invoke`'s step 4 — so no owner has to be re-resolved.
/// Same contract: only a strict `Selected` replaces `lenient`.
pub(crate) fn refine_lenient_selection_for<'a>(
    store: &'a ClassStore,
    receiver: ClassId,
    resolved: ResolvedRef,
    name: &str,
    desc: &str,
    lenient: Option<(&'a ClassFileMethod, ClassId)>,
) -> Option<(&'a ClassFileMethod, ClassId)> {
    // Fast accepts, no walk: the lenient walk's first hit IS the resolved
    // declaration (nothing below it declares the signature at all), or the
    // interface case of `lenient_is_the_selection`.
    let agrees = match (resolved, lenient) {
        (ResolvedRef::Declared(declared), Some((_, found))) => declared == found,
        (ResolvedRef::Interface(_), Some((m, found))) => {
            store.get(found).is_some_and(|c| !c.is_interface())
                && m.access_flags.contains(MethodAccessFlags::PUBLIC)
                && !m.is_static()
                && !m.is_abstract()
        }
        (_, None) => false,
    };
    if agrees {
        return lenient;
    }
    match select(store, receiver, resolved, name, desc) {
        Selection::Selected(declaring) => store
            .get(declaring)
            .and_then(|class| class.find_method(name, desc))
            .map(|m| (m, declaring))
            .or(lenient),
        Selection::AbstractMethod(_)
        | Selection::ConflictingDefaults(..)
        | Selection::NotPublic(_)
        | Selection::Lenient => lenient,
    }
}

/// [`refine_lenient_selection`]'s fast accept: an interface owner's resolved
/// method is public, so when the lenient walk's first hit is a concrete,
/// non-private instance method declared in a class, §5.4.6 step 2 selects
/// exactly it and a second walk can only agree.
fn lenient_is_the_selection(
    store: &ClassStore,
    owner: ClassId,
    lenient: Option<(&ClassFileMethod, ClassId)>,
) -> bool {
    let Some((m, declaring)) = lenient else {
        return false;
    };
    // `PUBLIC`, not merely non-private: a package-private or protected
    // class method is selected but is an `IllegalAccessError`
    // ([`Selection::NotPublic`], wave 30), so it is not the answer to keep.
    store.get(owner).is_some_and(|c| c.is_interface())
        && store.get(declaring).is_some_and(|c| !c.is_interface())
        && m.access_flags.contains(MethodAccessFlags::PUBLIC)
        && !m.is_static()
        && !m.is_abstract()
}

/// For a compiled call site that dispatches on the receiver's class with the
/// lenient walk — the JIT dispatch helpers in `vm/src/jit/helpers.rs` and the
/// inliner's receiver-guarded resolution in `interpreter/jit_bridge.rs`: the
/// class whose declaration §5.4.6 selects for `receiver` against the method
/// `owner` resolves `name`/`desc` to, when that is NOT the walk's answer
/// `lenient`. `None` when the two agree, when the walk found nothing, and for
/// every answer [`refine_lenient_selection_for`] keeps the walk's for
/// (`Lenient`, `AbstractMethod`, `ConflictingDefaults`), so a compiled site
/// never starts failing where it answered before, and a hierarchy with a
/// compatibility class in it keeps the walk.
///
/// The common shapes answer without the selection walk: the walk's hit is the
/// resolved declaration itself; an interface owner's concrete class method
/// ([`lenient_is_the_selection`]); or a concrete, non-private instance method
/// declared in a class while the resolved method is a public or protected
/// instance method — every non-private instance method overrides one of those
/// (§5.4.5), and the walk returns the first concrete declaration on the
/// receiver's chain, so it is the selection (or an abstract one below it is,
/// which is an `AbstractMethod` answer and keeps the walk anyway).
pub(crate) fn walk_selection_moves_to(
    store: &ClassStore,
    receiver: ClassId,
    owner: ClassId,
    name: &str,
    desc: &str,
    lenient: Option<(&ClassFileMethod, ClassId)>,
) -> Option<ClassId> {
    let (m, found) = lenient?;
    if lenient_is_the_selection(store, owner, lenient) {
        return None;
    }
    let resolved = resolved_ref_for_owner(store, owner, name, desc)?;
    if let ResolvedRef::Declared(declared) = resolved {
        if declared == found {
            return None;
        }
        let resolved_is_visible = store
            .get(declared)
            .and_then(|c| c.find_method(name, desc))
            .is_some_and(|ma| {
                !ma.is_static()
                    && (ma.access_flags.contains(MethodAccessFlags::PUBLIC)
                        || ma.access_flags.contains(MethodAccessFlags::PROTECTED))
            });
        if resolved_is_visible
            && store.get(found).is_some_and(|c| !c.is_interface())
            && !is_private(m)
            && !m.is_static()
            && !m.is_abstract()
        {
            return None;
        }
    }
    let (_, selected) =
        refine_lenient_selection_for(store, receiver, resolved, name, desc, lenient)?;
    (selected != found).then_some(selected)
}

/// [`walk_selection_moves_to`] with the lenient walk from `receiver` taken
/// here.
pub(crate) fn receiver_dispatch_moves_to(
    store: &ClassStore,
    receiver: ClassId,
    owner: ClassId,
    name: &str,
    desc: &str,
) -> Option<ClassId> {
    let lenient = crate::classloading::find_method_recursive(receiver, name, desc, store);
    walk_selection_moves_to(store, receiver, owner, name, desc, lenient)
}

/// The method a receiver-class dispatch runs, for a resolver that used to
/// answer with `find_method_recursive(receiver, ..)` alone: that walk,
/// replaced by §5.4.6's selection where [`walk_selection_moves_to`] says it
/// differs. `owner` is the class the call site's constant pool names (`None`
/// when the caller could not look it up, which keeps the walk).
pub(crate) fn select_for_receiver_dispatch<'a>(
    store: &'a ClassStore,
    receiver: ClassId,
    owner: Option<ClassId>,
    name: &str,
    desc: &str,
) -> Option<(&'a ClassFileMethod, ClassId)> {
    let lenient = crate::classloading::find_method_recursive(receiver, name, desc, store);
    let Some(owner) = owner else {
        return lenient;
    };
    match walk_selection_moves_to(store, receiver, owner, name, desc, lenient) {
        Some(selected) => store
            .get(selected)
            .and_then(|class| class.find_method(name, desc))
            .map(|m| (m, selected))
            .or(lenient),
        None => lenient,
    }
}

/// The §5.4.6 linkage error a strict door must raise instead of running the
/// lenient walk's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectionError {
    /// `AbstractMethodError`, with HotSpot's message.
    AbstractMethod(String),
    /// `IncompatibleClassChangeError` ("Conflicting default methods: ..."),
    /// with HotSpot's message.
    ConflictingDefaults(String),
    /// `IllegalAccessError` for a non-public method an `invokeinterface`
    /// selected, with HotSpot's message (interpreter round i1 wave 30).
    NotPublic(String),
}

/// [`strict_selection_error`] with the lenient walk from `receiver` taken
/// here, for a door that dispatches by the receiver's class and knows the
/// resolved method's `owner` (`Method.invoke`'s virtual door, the JIT's
/// by-name tails; round 12 wave 8 orchestrator).
pub(crate) fn receiver_selection_error(
    store: &ClassStore,
    receiver: ClassId,
    owner: ClassId,
    name: &str,
    desc: &str,
) -> Option<SelectionError> {
    let lenient = crate::classloading::find_method_recursive(receiver, name, desc, store);
    strict_selection_error(store, receiver, owner, name, desc, lenient)
}

/// For `invoke_on_class_shared_inner`'s receiver retarget under a strict
/// (`--real-jdk`) policy: the error §5.4.6 selection raises for `receiver`
/// against the method `owner` resolves `name`/`desc` to, when the lenient walk
/// found a body (`lenient`) that HotSpot would not run. `None` when selection
/// agrees with (or cannot speak against) the lenient answer — including every
/// [`Selection::Lenient`] shape, so a hierarchy with a compatibility class in
/// it never starts failing — and when the lenient walk found nothing (that
/// door's own not-found handling keeps the case).
pub(crate) fn strict_selection_error(
    store: &ClassStore,
    receiver: ClassId,
    owner: ClassId,
    name: &str,
    desc: &str,
    lenient: Option<(&ClassFileMethod, ClassId)>,
) -> Option<SelectionError> {
    if lenient.is_none() || lenient_is_the_selection(store, owner, lenient) {
        return None;
    }
    // JVMS 5.4.6: a resolved method that is private is itself the selected
    // method. The lambda proxies reach the synthetic body of an interface
    // default (`Function.lambda$andThen$0`, a private instance method) through
    // this door with the receiver's own class; selecting it by name in that
    // class finds nothing and used to raise `AbstractMethodError: No
    // qualifying defaults found` for a call HotSpot makes directly.
    if lenient.is_some_and(|(m, _)| is_private(m)) {
        return None;
    }
    let resolved = resolved_ref_for_owner(store, owner, name, desc)?;
    match select(store, receiver, resolved, name, desc) {
        Selection::AbstractMethod(selected) => Some(SelectionError::AbstractMethod(
            abstract_method_message(store, receiver, resolved, selected, name, desc),
        )),
        Selection::ConflictingDefaults(first, second) => Some(SelectionError::ConflictingDefaults(
            conflicting_defaults_message(store, receiver, first, second, name, desc),
        )),
        Selection::NotPublic(selected) => Some(SelectionError::NotPublic(not_public_message(
            store, selected, name, desc,
        ))),
        Selection::Selected(_) | Selection::Lenient => None,
    }
}

/// The resolved reference for a call site, looked up loader-aware from the
/// caller's defining loader WITHOUT loading anything. `None` when the owner is
/// not loaded in that namespace or the reference does not resolve.
pub(crate) fn resolved_ref_from_caller(
    cm: &ClassManager,
    caller: ClassId,
    owner_name: &str,
    name: &str,
    desc: &str,
) -> Option<ResolvedRef> {
    if owner_name.starts_with('[') {
        return None;
    }
    let owner = cm.find_cp_class_for_class(owner_name, caller)?;
    resolved_ref_for_owner(&cm.class_store, owner, name, desc)
}

// ---------------------------------------------------------------------------
// §6.5 linkage checks
// ---------------------------------------------------------------------------

/// `java.lang.String` for `java/lang/String`, keeping a hidden class's
/// `/<suffix>` as HotSpot's `external_name` does.
pub(crate) fn external_class_name(class: &Class) -> String {
    let name: &str = &class.name;
    if class.is_hidden() {
        if let Some(i) = name.rfind('/') {
            return format!("{}{}", name[..i].replace('/', "."), &name[i..]);
        }
    }
    name.replace('/', ".")
}

/// One descriptor token in HotSpot's external spelling: `int`,
/// `java.lang.String`, `int[][]`.
fn external_type(token: &str) -> String {
    let dims = token.bytes().take_while(|&b| b == b'[').count();
    let base = &token[dims..];
    let mut out = match base.as_bytes().first() {
        Some(b'B') => "byte".to_string(),
        Some(b'C') => "char".to_string(),
        Some(b'D') => "double".to_string(),
        Some(b'F') => "float".to_string(),
        Some(b'I') => "int".to_string(),
        Some(b'J') => "long".to_string(),
        Some(b'S') => "short".to_string(),
        Some(b'Z') => "boolean".to_string(),
        Some(b'V') => "void".to_string(),
        Some(b'L') => base
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
            .unwrap_or(base)
            .replace('/', "."),
        _ => base.to_string(),
    };
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

/// The `IllegalAccessError` message for [`Selection::NotPublic`]: HotSpot's
/// `'java.lang.String p.D.m()'` (`runtime_resolve_interface_method`: a quote,
/// the selected method's external name, a quote).
pub(crate) fn not_public_message(
    store: &ClassStore,
    selected: ClassId,
    name: &str,
    desc: &str,
) -> String {
    let holder = store
        .get(selected)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    format!("'{}'", external_method_name(&holder, name, desc))
}

/// HotSpot's `Method::print_external_name`: `void p.Foo.m(int, java.lang.String)`.
pub(crate) fn external_method_name(holder: &str, name: &str, desc: &str) -> String {
    let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(desc);
    let params: Vec<String> = params.iter().map(|p| external_type(p)).collect();
    format!(
        "{} {}.{}({})",
        external_type(ret),
        holder.replace('/', "."),
        name,
        params.join(", ")
    )
}

/// JVMS §6.5: `invokestatic` of an instance method, or
/// `invokevirtual`/`invokespecial`/`invokeinterface` of a static one, is an
/// `IncompatibleClassChangeError`. Returns HotSpot's message
/// (`LinkResolver::resolve_static_call` / `check_method_accessability`
/// family) when the method `owner.name desc` resolves to a method whose
/// `static` flag disagrees with `expect_static`.
///
/// `None` — "no mismatch, or not decidable here" — when the owner or the
/// resolved method's class has no real class bytes (a compatibility stub's
/// flags are not evidence), or the reference does not resolve through the
/// class store. Constructors and class initializers are never asked.
pub(crate) fn static_flag_mismatch(
    store: &ClassStore,
    owner: ClassId,
    name: &str,
    desc: &str,
    expect_static: bool,
) -> Option<String> {
    if name == "<init>" || name == "<clinit>" {
        return None;
    }
    if !store.get(owner)?.origin.has_real_bytes() {
        return None;
    }
    let declaring = resolve_declaring(store, owner, name, desc)?;
    let declaring_class = store.get(declaring)?;
    if !declaring_class.origin.has_real_bytes() {
        return None;
    }
    let m = declaring_class.find_method(name, desc)?;
    if m.is_static() == expect_static {
        return None;
    }
    Some(format!(
        "{} method '{}'",
        if expect_static {
            "Expected static"
        } else {
            "Expecting non-static"
        },
        external_method_name(&declaring_class.name, name, desc)
    ))
}

/// JVMS §6.5 `invokeinterface`: "if the class of objectref does not implement
/// the resolved interface, invokeinterface throws an
/// IncompatibleClassChangeError". Returns HotSpot's message
/// (`LinkResolver::runtime_resolve_interface_method`) when `receiver`
/// provably does not implement `iface`.
///
/// Provably: `None` unless both ends have real class bytes, the receiver's
/// whole hierarchy does too, the receiver's class is concrete, and neither the
/// `ClassId` walk nor the loader-blind name walk (a loader-split second copy of
/// the interface) finds the interface.
pub(crate) fn receiver_does_not_implement(
    cm: &ClassManager,
    receiver: ClassId,
    iface: ClassId,
) -> Option<String> {
    let store = &cm.class_store;
    let iface_class = store.get(iface)?;
    if !iface_class.is_interface() || !iface_class.origin.has_real_bytes() {
        return None;
    }
    if cm.is_subclass_of(receiver, iface) {
        return None;
    }
    let receiver_class = store.get(receiver)?;
    if receiver_class.is_interface() || receiver_class.is_abstract() {
        return None;
    }
    if receiver_class.is_assignable_to_name(&iface_class.name, store) {
        return None;
    }
    if !hierarchy_is_real(store, receiver) {
        return None;
    }
    Some(format!(
        "Class {} does not implement the requested interface {}",
        external_class_name(receiver_class),
        external_class_name(iface_class)
    ))
}

/// [`receiver_does_not_implement`] for an ARRAY receiver whose class is
/// `array_descriptor` (`[I`, `[Ljava/lang/String;`, as
/// `interpreter::array_descriptor_of` spells it). An array class implements
/// `java.lang.Cloneable` and `java.io.Serializable` and no other interface
/// (JLS 10.8, JVMS 5.3.3), so HotSpot's `invokeinterface` on one raises
/// `Class [I does not implement the requested interface ...`. Every invoke
/// door dispatches an array receiver on `java/lang/Object` and sets its
/// header's class id aside (it holds the COMPONENT's class), so the class
/// check never ran for one and a default method of the interface ran instead
/// (interpreter round i1 wave 39, lane L2; item 1 of
/// `i38-L6-compiled-dispatch-leftovers`).
///
/// Provably, as the class form: `None` unless `iface` is an interface with
/// real class bytes.
pub(crate) fn array_receiver_does_not_implement(
    cm: &ClassManager,
    array_descriptor: &str,
    iface: ClassId,
) -> Option<String> {
    let iface_class = cm.class_store.get(iface)?;
    if !iface_class.is_interface() || !iface_class.origin.has_real_bytes() {
        return None;
    }
    if array_interface_name(&iface_class.name) {
        return None;
    }
    Some(format!(
        "Class {} does not implement the requested interface {}",
        external_array_descriptor(array_descriptor),
        external_class_name(iface_class)
    ))
}

/// An array class's descriptor (`[Lp/Foo;`, `[[I`) as HotSpot's
/// `Klass::external_name` spells it: dots for slashes (`[Lp.Foo;`), except
/// that an array of a HIDDEN class keeps the element's `/0x<hex>` suffix
/// (`ObjArrayKlass` whose bottom class is hidden goes through
/// `convert_hidden_name_to_java`: `[Lp.Foo/0x0123;`). Interpreter round i7
/// wave 136, lane L6: the plain `replace('/', ".")` printed `[Lp.Foo.0x0123;`.
pub(crate) fn external_array_descriptor(array_descriptor: &str) -> String {
    let (body, tail) = match array_descriptor.strip_suffix(';') {
        Some(body) => (body, ";"),
        None => (array_descriptor, ""),
    };
    match body.rfind("/0x") {
        Some(at)
            if body.starts_with('[')
                && body.len() > at + 3
                && body[at + 3..].bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            format!("{}{}{tail}", body[..at].replace('/', "."), &body[at..])
        }
        _ => array_descriptor.replace('/', "."),
    }
}

/// Is `name` (internal form) one of the two interfaces every array class
/// implements?
pub(crate) fn array_interface_name(name: &str) -> bool {
    matches!(name, "java/lang/Cloneable" | "java/io/Serializable")
}

/// [`receiver_does_not_implement`] for a receiver whose header carries class
/// id 0, which every invoke door otherwise sets aside as "no usable class"
/// (a native that allocated with the `ClassId::new(0)` fallback) and
/// dispatches by the constant-pool class instead. A plain `new Object()` has
/// that id too: `java/lang/Object` is class 0 of the store, and a no-field
/// object of it carries `class_id = 0`. So an `invokeinterface` on a
/// `java.lang.Object` ran the interface's default method, where HotSpot
/// raises `Class java.lang.Object does not implement the requested interface
/// ...` (interpreter round i1 wave 38, lane L6;
/// `L6W38InterfaceReceiverHot.java`, `object` rows).
///
/// Only a receiver with no field slots (`receiver_slots`, the object's own
/// slot count), and only when class 0 of this store is `java/lang/Object`:
/// a fallback allocation that lost a class with fields keeps today's
/// by-name dispatch. The caller gates on `--real-jdk`.
pub(crate) fn object_receiver_does_not_implement(
    cm: &ClassManager,
    receiver_slots: usize,
    iface: ClassId,
) -> Option<String> {
    if receiver_slots != 0 {
        return None;
    }
    let object = ClassId::new(0);
    if !cm
        .class_store
        .get(object)
        .is_some_and(|c| &*c.name == "java/lang/Object")
    {
        return None;
    }
    receiver_does_not_implement(cm, object, iface)
}

/// Does any interface in `class`'s superinterface closure declare a
/// non-static concrete method? That is HotSpot's
/// `has_nonstatic_concrete_methods`, the condition under which class loading
/// runs `DefaultMethods::generate_default_methods` and so fills an
/// unimplemented interface-method slot with an error-throwing overpass rather
/// than leaving the abstract interface method in it.
fn runs_default_method_processing(store: &ClassStore, class: ClassId) -> bool {
    superinterface_closure(store, class).is_some_and(|closure| {
        closure.iter().any(|&id| {
            store.get(id).is_some_and(|iface| {
                iface
                    .methods
                    .iter()
                    .any(|m| !m.is_static() && !m.is_abstract())
            })
        })
    })
}

/// The `AbstractMethodError` message for [`Selection::AbstractMethod`]
/// `(selected)`, in HotSpot's wording for the same shape:
///
/// * an abstract method selected in the receiver's class chain —
///   `LinkResolver::throw_abstract_method_error`: `Receiver class C does not
///   define or inherit an implementation of the resolved method 'abstract void
///   m()' of abstract class A.`, plus ` Selected method is 'abstract void
///   p.B.m()'.` when the selected method is not the resolved one;
/// * nothing selected in a hierarchy that runs default-method processing —
///   the overpass HotSpot generates: `Method p/C.m()V is abstract`;
/// * nothing selected otherwise — the first form, without the suffix.
pub(crate) fn abstract_method_message(
    store: &ClassStore,
    receiver: ClassId,
    resolved: ResolvedRef,
    selected: Option<ClassId>,
    name: &str,
    desc: &str,
) -> String {
    if selected.is_none() && runs_default_method_processing(store, receiver) {
        // `MethodFamily::determine_target_or_set_exception_message`.
        let has_candidates = maximally_specific(store, receiver, name, desc)
            .is_some_and(|maximal| !maximal.is_empty());
        return match store.get(receiver) {
            Some(class) if has_candidates => {
                format!("Method {}.{name}{desc} is abstract", class.name)
            }
            _ => "No qualifying defaults found".to_string(),
        };
    }
    let receiver_name = store
        .get(receiver)
        .map(external_class_name)
        .unwrap_or_else(|| format!("<class {receiver}>"));
    let resolved_id = match resolved {
        ResolvedRef::Declared(id) => Some(id),
        // The interface named by the reference, or the superinterface that
        // actually declares the method.
        ResolvedRef::Interface(Some(id)) => resolve_declaring(store, id, name, desc).or(Some(id)),
        ResolvedRef::Interface(None) => None,
    };
    let resolved_class = resolved_id.and_then(|id| store.get(id));
    let is_abstract = resolved_class
        .and_then(|c| c.find_method(name, desc))
        .is_some_and(|m| m.is_abstract());
    let selected_suffix = match selected.filter(|&sel| Some(sel) != resolved_id) {
        Some(sel) => match store.get(sel) {
            Some(sel_class) => {
                let sel_abstract = sel_class
                    .find_method(name, desc)
                    .is_some_and(|m| m.is_abstract());
                let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(desc);
                let params: Vec<String> = params.iter().map(|p| external_type(p)).collect();
                format!(
                    " Selected method is '{}{} {}.{name}({})'.",
                    if sel_abstract { "abstract " } else { "" },
                    external_type(ret),
                    external_class_name(sel_class),
                    params.join(", ")
                )
            }
            None => String::new(),
        },
        None => String::new(),
    };
    let owner = match resolved_class {
        Some(c) => {
            let kind = if c.is_interface() {
                "interface"
            } else if c.is_abstract() {
                "abstract class"
            } else {
                "class"
            };
            format!(" of {kind} {}", external_class_name(c))
        }
        None => String::new(),
    };
    let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(desc);
    let params: Vec<String> = params.iter().map(|p| external_type(p)).collect();
    format!(
        "Receiver class {receiver_name} does not define or inherit an implementation of the \
         resolved method '{}{} {name}({})'{owner}.{selected_suffix}",
        if is_abstract { "abstract " } else { "" },
        external_type(ret),
        params.join(", "),
    )
}

/// The `AbstractMethodError` text of an `invokevirtual` whose constant-pool
/// entry an earlier call already LINKED, when [`select`] answers
/// [`Selection::AbstractMethod`]`(selected)`: HotSpot then calls through the
/// vtable into the abstract method's entry, which throws without a receiver
/// (`InterpreterRuntime::throw_AbstractMethodErrorWithMethod` ->
/// `LinkResolver::throw_abstract_method_error(method)`; an INTERPRETED caller
/// only -- compiled callers answer the resolution text, j27 wave 9):
/// `Missing implementation of resolved method 'abstract
/// int m()' of abstract class p.Z.`, naming the abstract method IN THE SLOT
/// (the selected one, or for a miranda slot the maximally-specific abstract
/// interface method), not the resolved one. `None` when the text is not this
/// one: an `invokeinterface` (`ResolvedRef::Interface`, the itable stub passes
/// the receiver), and the default-method overpass (`Method C.m()I is
/// abstract`, the same at a linked site). Measured on HotSpot 25.0.3,
/// `L2W142LinkedAmeText.java`. Interpreter round i7
/// wave 142, lane L2 (`CRATONVM_LINKED_INVOKEVIRTUAL_AME_TEXT`).
pub(crate) fn linked_virtual_abstract_message(
    store: &ClassStore,
    receiver: ClassId,
    resolved: ResolvedRef,
    selected: Option<ClassId>,
    name: &str,
    desc: &str,
) -> Option<String> {
    if !matches!(resolved, ResolvedRef::Declared(_)) {
        return None;
    }
    if selected.is_none() && runs_default_method_processing(store, receiver) {
        return None;
    }
    let holder = match selected {
        Some(id) => id,
        None => maximally_specific(store, receiver, name, desc)?
            .iter()
            .find(|&&(_, is_abstract)| is_abstract)
            .map(|&(id, _)| id)?,
    };
    let class = store.get(holder)?;
    if !class.find_method(name, desc)?.is_abstract() {
        return None;
    }
    let kind = if class.is_interface() {
        "interface"
    } else if class.is_abstract() {
        "abstract class"
    } else {
        "class"
    };
    let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(desc);
    let params: Vec<String> = params.iter().map(|p| external_type(p)).collect();
    Some(format!(
        "Missing implementation of resolved method 'abstract {} {name}({})' of {kind} {}.",
        external_type(ret),
        params.join(", "),
        external_class_name(class)
    ))
}

/// The key of one `invokevirtual` constant-pool entry in the per-VM linked
/// record (`ClassRealm::linked_virtual_sites`), with the caller's class id: a
/// digest of the `Methodref`'s class, name and descriptor. A class file's pool
/// holds one `Methodref` per triple (javac and the ClassFile API deduplicate
/// it), so the triple names the entry; the interpreter knows it from the pool
/// and the JIT from its `JitInvokeInfo`, without a cp index in that struct.
pub(crate) fn linked_site_key(owner: &str, name: &str, desc: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    owner.hash(&mut h);
    name.hash(&mut h);
    desc.hash(&mut h);
    h.finish()
}

/// Record that the `invokevirtual` entry `(owner, name, desc)` of `caller`'s
/// constant pool has linked: a call through it dispatched successfully.
/// HotSpot links the entry at the first successful interpreted resolution and
/// keeps it until the class is redefined (a new constant pool), so the record
/// carries the caller's redefinition generation, and the VM's redefine epoch
/// at the time so that a site recorded before a redefinition is recorded
/// again after it. A site already recorded in the current epoch costs one read
/// lock and one probe: no class-manager read, no write (a megamorphic site
/// takes a recording path on every call). Off
/// (`CRATONVM_LINKED_INVOKEVIRTUAL_AME_TEXT=0`): one per-VM bool read.
/// Interpreter round i7 wave 142, lane L2.
pub(crate) fn note_linked_invokevirtual(
    shared: &crate::vm::SharedVm,
    caller: ClassId,
    owner: &str,
    name: &str,
    desc: &str,
) {
    if !shared.classes.linked_invokevirtual_ame_text {
        return;
    }
    let key = (caller.as_u32(), linked_site_key(owner, name, desc));
    let epoch = shared.jit.jit_cache.redefine_epoch();
    if shared
        .classes
        .linked_virtual_sites
        .read()
        .get(&key)
        .is_some_and(|&(_, recorded_epoch)| recorded_epoch == epoch)
    {
        return;
    }
    let generation = shared
        .classes
        .class_manager
        .read_recursive()
        .class_redefine_generation(caller);
    shared
        .classes
        .linked_virtual_sites
        .write()
        .insert(key, (generation, epoch));
}

/// Has the `invokevirtual` entry `(owner, name, desc)` of `caller`'s pool
/// linked ([`note_linked_invokevirtual`]) in the caller's current
/// redefinition generation? `false` with the switch off. Cold: asked only
/// when an `AbstractMethodError` is about to be raised.
pub(crate) fn invokevirtual_site_linked(
    shared: &crate::vm::SharedVm,
    cm: &ClassManager,
    caller: ClassId,
    owner: &str,
    name: &str,
    desc: &str,
) -> bool {
    if !shared.classes.linked_invokevirtual_ame_text {
        return false;
    }
    let key = linked_site_key(owner, name, desc);
    let recorded = shared
        .classes
        .linked_virtual_sites
        .read()
        .get(&(caller.as_u32(), key))
        .copied();
    recorded.is_some_and(|(generation, _)| generation == cm.class_redefine_generation(caller))
}

/// HotSpot's overpass-method wording for conflicting defaults:
/// `Conflicting default methods: p/I.m p/J.m` (internal names, as HotSpot
/// prints the klass name symbol). HotSpot lists every maximally-specific
/// candidate — abstract ones too — in its hierarchy-walk order
/// ([`superinterface_closure`]); `first` / `second` (the two conflicting
/// defaults [`select`] reported) are the fallback when the closure cannot be
/// recomputed.
pub(crate) fn conflicting_defaults_message(
    store: &ClassStore,
    receiver: ClassId,
    first: ClassId,
    second: ClassId,
    name: &str,
    desc: &str,
) -> String {
    let n = |id: ClassId| {
        store
            .get(id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("<class {id}>"))
    };
    let candidates: Vec<ClassId> = match maximally_specific(store, receiver, name, desc) {
        Some(maximal) if maximal.len() >= 2 => maximal.into_iter().map(|(id, _)| id).collect(),
        _ => vec![first, second],
    };
    let mut message = String::from("Conflicting default methods:");
    for id in candidates {
        message.push(' ');
        message.push_str(&n(id));
        message.push('.');
        message.push_str(name);
    }
    message
}

#[cfg(test)]
mod tests;
