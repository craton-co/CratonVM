#[allow(unused_imports)]
use super::*;

/// Record debugger metadata for every class defined since the last call —
/// all of them at attach — and return the new classes as `(wire class id,
/// JNI signature, type tag)`.
///
/// Wave 7: classes defined after the debugger attached used to have no
/// metadata at all until a re-attach (metadata was read once, at attach), so
/// `VirtualMachine.ClassesBySignature` could not find them, a breakpoint could
/// not be set in them and a step in them fell back to the line-start rule.
/// The JDWP server now calls this on every poll; class ids are never reused,
/// so the classes new since the last call are the store slots past
/// [`DebugState::class_slots_seen`], and an unchanged store costs one
/// comparison. Takes the class-manager lock, then the debug-state lock (the
/// order `interpreter::deliver_breakpoint_if_set` respects).
/// Make the array classes HotSpot creates at start-up, when a debugger
/// attaches (interpreter round i1 wave 44, lane L1): the eight primitive
/// arrays (`Universe::genesis`), `Object[]`, and `String[]` (the launcher
/// resolves `main(String[])`). This VM makes an array class when Java code
/// first names it; `main`'s arguments are allocated with the element class
/// alone (`vm_exec`'s `GetInitiatedClasses` list makes the same ten, for the
/// same reason). JDI lists every class once, at its first
/// `AllClasses`, and learns of no array class after it (no JVMTI
/// `ClassPrepare` is posted for one), so `classesByName("java.lang.String[]")`
/// found nothing where HotSpot finds the class
/// (`tools/probes/interp/L1/L1W43JdiSourceDebugExtension.java`, `== an array
/// class`: `no String[]`). Once per attach, before the class listing; an
/// array class already made is left as it is.
pub(in crate::debug) fn ensure_startup_array_classes(shared: &crate::vm::SharedVm) {
    use cratonvm_types::ClassLoaderId;
    const STARTUP_ARRAYS: [&str; 10] = [
        "[Z",
        "[C",
        "[F",
        "[D",
        "[B",
        "[S",
        "[I",
        "[J",
        "[Ljava/lang/Object;",
        "[Ljava/lang/String;",
    ];
    let missing = {
        let cm = shared.classes.class_manager.read();
        STARTUP_ARRAYS.iter().any(|name| {
            cm.loaded_class_under_exact_key(name, ClassLoaderId::Bootstrap)
                .is_none()
        })
    };
    if missing {
        let mut cm = shared.classes.class_manager_write_settling();
        for name in STARTUP_ARRAYS {
            let _ = cm.load_array_class_for_loader(name, ClassLoaderId::Bootstrap);
        }
    }
}

pub(in crate::debug) fn populate_class_metadata(shared: &crate::vm::SharedVm) -> Vec<(u64, String, u8)> {
    let added = populate_stored_class_metadata(shared);
    register_lambda_proxy_metadata(shared);
    added
}

/// Record debugger metadata for the lambda proxy classes minted since the
/// last call (the i54-L1 retirement,
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// the VM mints a lambda's class outside the class store, so the session's
/// tables never described one, and a listing that names its method's frame
/// (`interpreter::ListingSplice`) named a class no `ReferenceType` command
/// knew. Each is described from its facts record
/// (`runtime::lambda_proxy::lambda_proxy_facts`), as HotSpot's back end
/// answers the hidden class (MEASURED, `tools/probes/interp/L1/L1K54JdiLambdaProxyFrames.java`):
/// its signature, `0x1030`, `java.lang.Object` as its superclass, its
/// interfaces, methods (with their spun code's lengths) and `arg$N` fields,
/// no source file, class-file version 69, status VERIFIED | PREPARED |
/// INITIALIZED; `AllClasses` lists it. An id minted but not yet linked to
/// its call site is retried at the next call. Not reported by
/// `ClassPrepare`. Nothing but one lock and a compare when no proxy is new.
pub(in crate::debug) fn register_lambda_proxy_metadata(shared: &crate::vm::SharedVm) {
    let next = shared
        .classes
        .next_lambda_id
        .load(std::sync::atomic::Ordering::Acquire);
    let (from, mut ids) = {
        let ds = shared.debug.debug_state.lock();
        if ds.lambda_ids_seen >= next && ds.lambda_ids_pending.is_empty() {
            return;
        }
        (ds.lambda_ids_seen, ds.lambda_ids_pending.clone())
    };
    ids.extend(from..next);
    let object = shared
        .classes
        .class_manager
        .read()
        .resolve_fast_path_class_id("java/lang/Object")
        .map(|id| u64::from(id.as_u32()));
    let mut described = Vec::new();
    let mut pending = Vec::new();
    for raw in ids {
        let id = crate::classloading::ClassId::new(raw);
        let Some(facts) = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, id) else {
            pending.push(raw);
            continue;
        };
        let lengths: Vec<Option<u64>> = (0..facts.methods.len())
            .map(|index| {
                crate::runtime::lambda_proxy::lambda_proxy_code(shared, id, index)
                    .and_then(|code| u64::try_from(code.len()).ok())
            })
            .collect();
        described.push((id, facts, lengths));
    }
    let mut ds = shared.debug.debug_state.lock();
    for (id, facts, lengths) in described {
        add_lambda_proxy_metadata(&mut ds, id, &facts, &lengths, object);
    }
    ds.lambda_ids_seen = ds.lambda_ids_seen.max(next);
    ds.lambda_ids_pending = pending;
}

/// JDWP `ClassStatus` of a lambda proxy class: VERIFIED | PREPARED |
/// INITIALIZED (it exists once its call site is linked; measured 7, as
/// JVMTI's `GetClassStatus` answers it).
pub(in crate::debug) const LAMBDA_PROXY_CLASS_STATUS: u32 = CLASS_STATUS_LINKED | CLASS_STATUS_INITIALIZED;

/// Record lambda proxy `id`'s metadata from its `facts`
/// ([`register_lambda_proxy_metadata`]); `lengths` the spun code length of
/// each method, `object` the wire id of `java.lang.Object`.
pub(in crate::debug) fn add_lambda_proxy_metadata(
    ds: &mut DebugState,
    id: crate::classloading::ClassId,
    facts: &crate::runtime::lambda_proxy::LambdaProxyFacts,
    lengths: &[Option<u64>],
    object: Option<u64>,
) {
    let class_id = u64::from(id.as_u32());
    ds.class_type_tags.insert(class_id, 1);
    ds.class_signatures.insert(class_id, facts.signature(id));
    if let Some(object) = object {
        ds.class_superclass.insert(class_id, object);
    }
    ds.class_statuses
        .insert(class_id, LAMBDA_PROXY_CLASS_STATUS);
    let mut details = ClassDetails {
        modifiers: u32::from(crate::runtime::lambda_proxy::LAMBDA_PROXY_CLASS_MODIFIERS),
        generic_signature: None,
        interfaces: facts
            .interfaces
            .iter()
            .map(|i| u64::from(i.as_u32()))
            .collect(),
        // The spun class's version: the running JDK's latest (measured 69).
        version: Some((69, 0)),
        ..ClassDetails::default()
    };
    let mut methods = Vec::with_capacity(facts.methods.len());
    for (index, m) in facts.methods.iter().enumerate() {
        let method_id = jdwp_method_id(&m.name, &m.descriptor);
        methods.push(commands::MethodInfo {
            method_id: ids::MethodId(method_id),
            name: m.name.to_string(),
            signature: m.descriptor.to_string(),
            // Never synthetic (measured).
            mod_bits: jdwp_member_modifiers(
                u32::from(m.access_flags),
                JVM_RECOGNIZED_METHOD_MODIFIERS,
                false,
            ),
        });
        if let Some(length) = lengths.get(index).copied().flatten() {
            details.code_lengths.insert(method_id, length);
        }
    }
    ds.class_methods.insert(class_id, methods);
    let fields = facts
        .fields
        .iter()
        .enumerate()
        .map(|(i, (name, descriptor))| commands::FieldInfo {
            field_id: ids::FieldId(jdwp_field_id(id, i)),
            name: name.clone(),
            signature: descriptor.clone(),
            mod_bits: u32::from(crate::runtime::lambda_proxy::LAMBDA_PROXY_FIELD_MODIFIERS),
        })
        .collect();
    ds.class_fields.insert(class_id, fields);
    ds.class_details.insert(class_id, details);
}

pub(in crate::debug) fn populate_stored_class_metadata(shared: &crate::vm::SharedVm) -> Vec<(u64, String, u8)> {
    let cm = shared.classes.class_manager.read();
    let slots = cm.class_store.slot_count();
    let mut ds = shared.debug.debug_state.lock();
    let from = ds.class_slots_seen;
    let mut added = Vec::new();
    // zt t18/df7 (x13 item 21a): a define whose name is not bound (its
    // `addClass` upcall in flight, or thrown) is in no dictionary, so no
    // `AllClasses` / `ClassesBySignature` lists it. Its slot is kept aside
    // and described once its name is bound; a slot whose class is gone
    // (unloaded) is dropped.
    if !ds.class_slots_nameless.is_empty() {
        let pending = std::mem::take(&mut ds.class_slots_nameless);
        for raw in pending {
            let id = crate::classloading::ClassId::new(raw);
            match cm.class_store.get(id) {
                Some(_) if cm.class_store.is_nameless(id) => ds.class_slots_nameless.push(raw),
                Some(class) => added.push(add_class_metadata(&mut ds, class)),
                None => {}
            }
        }
    }
    if slots <= from {
        return added;
    }
    for slot in from..slots {
        let Ok(raw) = u32::try_from(slot) else {
            break;
        };
        let id = crate::classloading::ClassId::new(raw);
        if let Some(class) = cm.class_store.get(id) {
            if cm.class_store.is_nameless(id) {
                ds.class_slots_nameless.push(raw);
                continue;
            }
            added.push(add_class_metadata(&mut ds, class));
        }
    }
    ds.class_slots_seen = slots;
    added
}

/// JDWP `ClassStatus` bits: `VERIFIED` (1), `PREPARED` (2), `INITIALIZED`
/// (4), `ERROR` (8).
pub(in crate::debug) const CLASS_STATUS_LINKED: u32 = 1 | 2;
pub(in crate::debug) const CLASS_STATUS_INITIALIZED: u32 = 4;
pub(in crate::debug) const CLASS_STATUS_ERROR: u32 = 8;

/// The JDWP `ClassStatus` of `class` (interpreter round i1 wave 15), as
/// HotSpot's back end maps `InstanceKlass::jvmti_class_status`: a linked
/// class is VERIFIED | PREPARED, an initialized one INITIALIZED too, an
/// erroneous one (linked, its initialization failed) ERROR too; a class not
/// linked yet, or whose linking failed, has no bit; an array class has none
/// either (JVMTI's `ARRAY` status, which JDWP does not carry). Every class
/// answered VERIFIED | PREPARED.
pub(crate) fn jdwp_class_status(class: &crate::classloading::Class) -> u32 {
    use crate::classloading::ClassState;
    if class.name.starts_with('[') {
        return 0;
    }
    match class.state {
        ClassState::Loading | ClassState::Loaded | ClassState::Verifying => 0,
        ClassState::Verified
        | ClassState::Preparing
        | ClassState::Prepared
        | ClassState::Initializing => CLASS_STATUS_LINKED,
        ClassState::Initialized => CLASS_STATUS_LINKED | CLASS_STATUS_INITIALIZED,
        ClassState::InitializationError => CLASS_STATUS_LINKED | CLASS_STATUS_ERROR,
    }
}

/// Is `class` loaded but not prepared, so that HotSpot's back end would leave
/// it out of `AllClassesWithGeneric` and `ClassesBySignature` and post no
/// `ClassPrepare` for it (interpreter round i5 wave 103, lane L1)? An array
/// class never is (JVMTI's `ARRAY` status keeps it listed). A class being
/// defined (`Loading`), being linked or whose link failed (`Verifying`,
/// [`crate::vm::LINK_FAILED_STATE`]) is. A `Loaded` class is, unless linking
/// it is trivial ([`crate::vm::class_link_is_trivial`]): such a class (a
/// bootstrap JDK class) may be instantiated without ever being linked while
/// no `ClassPrepare` is observed (`vm::class_linked_or_link_trivial`), so
/// its code may run while it reads `Loaded`; it stays listed, as every class
/// was before wave 103. A class whose link is not trivial is never used
/// before its link moves it on.
///
/// Interpreter round i5 wave 104 (lane L1): a `Loaded` class with a trivial
/// link is unprepared too when it was defined while a JDWP `ClassPrepare`
/// request was in force (`watermark`, [`DebugState::class_prepare_watermark`],
/// and one is in force now): `vm::class_linked_or_link_trivial` answered no
/// for it all along, so no code used it unlinked, and HotSpot hides it (a JDK
/// class loaded by `Class.forName(n, false, null)`). A trivially linked class
/// defined before the request stays listed (it may have been used unlinked).
pub(crate) fn jdwp_class_unprepared(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    watermark: Option<usize>,
) -> bool {
    use crate::classloading::ClassState;
    if class.name.starts_with('[') {
        return false;
    }
    match class.state {
        ClassState::Loading | ClassState::Verifying => true,
        ClassState::Loaded => {
            !crate::vm::class_link_is_trivial(shared, cm, class)
                || defined_while_class_prepare_observed(shared, class, watermark)
                || never_used_while_class_prepare_observed(shared, class, watermark)
        }
        _ => false,
    }
}

/// Interpreter round i5 wave 106 (lane L1; the residue of
/// `interpreter-i102-L1-jdwp-lists-unprepared-classes-FIXED-20261016.md`): is `class`, a
/// `Loaded` class with a trivial link defined BEFORE the watermark, one that
/// no path has used unlinked ([`UsedUnlinkedClasses`]), with a JDWP
/// `ClassPrepare` request in force? Then it has run no code: the only paths
/// that use a class without linking it (`jit_bridge::jit_new_target_init_walk`,
/// `exceptions::stackless_exception_class`) record the use first, and while
/// the request is in force they refuse a never-linked class
/// (`vm::class_linked_or_link_trivial`), so a later use initializes it on a
/// Java thread, which prepares it and posts its `ClassPrepare`. HotSpot hides
/// such a class (`Class.forName(n, false, null)` before the attach).
/// `CRATONVM_JDWP_HIDE_UNUSED_UNLINKED_CLASSES=0`: never (listed, as before).
pub(in crate::debug) fn never_used_while_class_prepare_observed(
    shared: &crate::vm::SharedVm,
    class: &crate::classloading::Class,
    watermark: Option<usize>,
) -> bool {
    watermark.is_some()
        && !shared.debug.used_unlinked_classes.contains(class.id)
        && shared.debug.debugger_gates.class_prepare_armed()
}

/// The classes some path used while they were still never linked (`Loaded`,
/// or `Verifying` after a failed link) because linking them is trivial
/// (`vm::class_link_is_trivial`): one bit per class-store slot, set once and
/// never cleared (interpreter round i5 wave 106, lane L1). Only the JDWP
/// listing reads it ([`never_used_while_class_prepare_observed`]); a set bit
/// keeps the class listed, so an over-approximation (a bit set by a walk whose
/// answer then did not use the class, a slot reused by another class) only
/// lists what wave 104 listed. A slot past the capacity reads as used.
///
/// Per VM (`DebugRealm::used_unlinked_classes`), read-only after creation
/// apart from the bits; `CRATONVM_JDWP_HIDE_UNUSED_UNLINKED_CLASSES=0` makes
/// every class read as used and records nothing.
pub struct UsedUnlinkedClasses {
    /// `None` with the switch off.
    pub(in crate::debug) words: Option<Box<[std::sync::atomic::AtomicU64]>>,
}

/// Class-store slots [`UsedUnlinkedClasses`] keeps a bit for (16 KiB per VM).
pub(in crate::debug) const USED_UNLINKED_CAPACITY: usize = 1 << 17;

impl UsedUnlinkedClasses {
    /// The VM's set, empty; the switch read once.
    pub fn new() -> Self {
        let on = true;
        Self {
            words: on.then(|| {
                (0..USED_UNLINKED_CAPACITY / 64)
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect()
            }),
        }
    }

    /// Has `id` been used unlinked (or is it past the capacity, or the switch
    /// off)? One relaxed load.
    #[inline]
    pub fn contains(&self, id: crate::classloading::ClassId) -> bool {
        let Some(words) = self.words.as_deref() else {
            return true;
        };
        // Widening: a class id is a `u32` store slot.
        let slot = id.as_u32() as usize;
        match words.get(slot / 64) {
            Some(word) => {
                word.load(std::sync::atomic::Ordering::Relaxed) & (1u64 << (slot % 64)) != 0
            }
            None => true,
        }
    }

    /// Record `id`. Cold: once per class.
    #[cold]
    #[inline(never)]
    pub(in crate::debug) fn insert(&self, id: crate::classloading::ClassId) {
        let Some(words) = self.words.as_deref() else {
            return;
        };
        let slot = id.as_u32() as usize;
        if let Some(word) = words.get(slot / 64) {
            word.fetch_or(1u64 << (slot % 64), std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl Default for UsedUnlinkedClasses {
    fn default() -> Self {
        Self::new()
    }
}

/// `class` is about to be used although it was never linked, its link being
/// trivial (`vm::class_linked_or_link_trivial` answered yes for it): record
/// it ([`UsedUnlinkedClasses`]). A linked class is not recorded. One relaxed
/// load once recorded. Interpreter round i5 wave 106, lane L1; called by
/// `jit_bridge::jit_new_target_init_walk` for each class of the chain it lets
/// a resolved `new` row skip.
#[inline]
pub(crate) fn note_class_used_unlinked(
    shared: &crate::vm::SharedVm,
    class: &crate::classloading::Class,
) {
    use crate::classloading::ClassState;
    if matches!(class.state, ClassState::Loaded | ClassState::Verifying)
        && !shared.debug.used_unlinked_classes.contains(class.id)
    {
        shared.debug.used_unlinked_classes.insert(class.id);
    }
}

/// `leaf` and the never-linked classes above it are about to be used without
/// being initialized (`exceptions::stackless_exception_class`, a fast-throw
/// instance of a class not initialized): record each never-linked class of
/// the chain, and `leaf` itself, so the next call returns after ONE relaxed
/// load (the leaf's bit). The walk stops at a linked class (its supertypes
/// were linked with it). Interpreter round i5 wave 106, lane L1.
#[inline]
pub(crate) fn note_chain_used_unlinked(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
    leaf: crate::classloading::ClassId,
) {
    if shared.debug.used_unlinked_classes.contains(leaf) {
        return;
    }
    note_chain_used_unlinked_cold(shared, cm, leaf);
}

#[cold]
#[inline(never)]
pub(in crate::debug) fn note_chain_used_unlinked_cold(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
    leaf: crate::classloading::ClassId,
) {
    use crate::classloading::ClassState;
    // A hierarchy is shallow; a longer walk is a malformed graph.
    const MAX_WALK: usize = 256;
    let mut todo = vec![leaf];
    let mut walked = 0usize;
    while let Some(id) = todo.pop() {
        walked += 1;
        if walked > MAX_WALK {
            break;
        }
        let Some(class) = cm.get_class(id) else {
            continue;
        };
        if !matches!(class.state, ClassState::Loaded | ClassState::Verifying) {
            continue;
        }
        shared.debug.used_unlinked_classes.insert(id);
        todo.extend(class.superclass);
        todo.extend(class.interfaces.iter().copied());
    }
    // The leaf's bit is the fast path's answer, linked or not.
    shared.debug.used_unlinked_classes.insert(leaf);
}

/// Was `class` defined at or after `watermark` (a slot of the class store,
/// [`DebugState::class_prepare_watermark`]), with a JDWP `ClassPrepare`
/// request still in force (interpreter round i5 wave 104, lane L1)?
pub(in crate::debug) fn defined_while_class_prepare_observed(
    shared: &crate::vm::SharedVm,
    class: &crate::classloading::Class,
    watermark: Option<usize>,
) -> bool {
    // Widening: a class id is a `u32` store slot.
    watermark.is_some_and(|w| class.id.as_u32() as usize >= w)
        && shared.debug.debugger_gates.class_prepare_armed()
}

/// The exception classes HotSpot 25's `Threads::initialize_java_lang_classes`
/// initializes at boot, before any debugger can attach, so their chains
/// (`Throwable`, `Exception`, `RuntimeException`, `Error`, ...) are always
/// prepared there.
pub(in crate::debug) const HOTSPOT_BOOT_INITIALIZED_EXCEPTIONS: &[&str] = &[
    "java/lang/OutOfMemoryError",
    "java/lang/NullPointerException",
    "java/lang/ClassCastException",
    "java/lang/ArrayStoreException",
    "java/lang/ArithmeticException",
    "java/lang/ArrayIndexOutOfBoundsException",
    "java/lang/StackOverflowError",
    "java/lang/IllegalMonitorStateException",
    "java/lang/IllegalArgumentException",
    "java/lang/InternalError",
];

/// Interpreter round i7 (final JDI battery, `L1W25JdiStopMonitors --nojit`:
/// `classesByName("java.lang.RuntimeException")` was empty): this VM loads
/// [`HOTSPOT_BOOT_INITIALIZED_EXCEPTIONS`] at boot but leaves them `Loaded`
/// until a throw needs one. With the JIT on, a fast-throw or `new` row records
/// their chains ([`note_chain_used_unlinked`]) early; under `--nojit` nothing
/// did, so with a `ClassPrepare` request in force
/// [`never_used_while_class_prepare_observed`] hid them, where HotSpot lists
/// them. Record their chains as used when the watermark is first set.
pub(in crate::debug) fn note_hotspot_boot_initialized_classes_used(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
) {
    for name in HOTSPOT_BOOT_INITIALIZED_EXCEPTIONS {
        if let Some(id) =
            cm.find_class_by_name_for_loader(name, crate::classloading::ClassLoaderId::Bootstrap)
        {
            note_chain_used_unlinked(shared, cm, id);
        }
    }
}

/// Keep [`DebugState::class_prepare_watermark`] (interpreter round i5 wave
/// 104, lane L1): set to the class store's slot count when a JDWP
/// `ClassPrepare` request is first found in force, cleared once none is.
/// The slot count is read after the request was armed, so a class defined
/// between the two is counted as defined before it (listed: the safe side).
/// Called by the server after every command, with no lock held; one load and
/// one lock when nothing changes. Lock order: the class manager, released,
/// then the debug state.
pub(in crate::debug) fn note_class_prepare_watermark(shared: &crate::vm::SharedVm) {
    let armed = shared.debug.debugger_gates.class_prepare_armed();
    {
        let mut ds = shared.debug.debug_state.lock();
        if !armed {
            ds.class_prepare_watermark = None;
            return;
        }
        if ds.class_prepare_watermark.is_some() || !ds.hide_unprepared_classes {
            return;
        }
    }
    let slots = {
        let cm = shared.classes.class_manager.read();
        note_hotspot_boot_initialized_classes_used(shared, &cm);
        cm.class_store.slot_count()
    };
    let mut ds = shared.debug.debug_state.lock();
    if ds.class_prepare_watermark.is_none() && shared.debug.debugger_gates.class_prepare_armed() {
        ds.class_prepare_watermark = Some(slots);
    }
}

/// Refresh [`DebugState::class_statuses`] for every class the session knows
/// (wave 15). Before the commands that answer a class's status
/// ([`reads_class_status`]). Takes the class-manager lock, then the
/// debug-state lock. Wave 103: and [`DebugState::unprepared_classes`].
pub(in crate::debug) fn refresh_class_statuses(shared: &crate::vm::SharedVm) {
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    let hide = ds.hide_unprepared_classes;
    let watermark = ds.class_prepare_watermark;
    let mut unprepared: HashSet<u64> = HashSet::new();
    let statuses: HashMap<u64, u32> = ds
        .class_signatures
        .keys()
        .filter_map(|&id| {
            let raw = u32::try_from(id).ok()?;
            match cm.class_store.get(crate::classloading::ClassId::new(raw)) {
                Some(class) => {
                    if hide && jdwp_class_unprepared(shared, &cm, class, watermark) {
                        unprepared.insert(id);
                    }
                    Some((id, jdwp_class_status(class)))
                }
                // A lambda proxy class (the i54-L1 retirement) is never in
                // the store: initialized since its call site was linked.
                None if raw >= crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE => {
                    Some((id, LAMBDA_PROXY_CLASS_STATUS))
                }
                None => None,
            }
        })
        .collect();
    ds.class_statuses = statuses;
    ds.unprepared_classes = unprepared;
}

/// Refresh the statuses the command in hand answers (interpreter round i1
/// wave 24): `ReferenceType.Status` the one class it names,
/// `ClassesBySignature` the classes of that signature, and only
/// `AllClassesWithGeneric` every class ([`refresh_class_statuses`]). Every
/// one of them refreshed every class the session knows — thousands of
/// class-store reads and a map rebuild for each `isPrepared()` a debugger
/// asks, and JDI asks it of every class it lists by name.
pub(in crate::debug) fn refresh_class_statuses_for(
    shared: &crate::vm::SharedVm,
    command_set: u8,
    command: u8,
    data: &[u8],
) {
    use commands::{
        CMD_RT_FIELDS, CMD_RT_FIELDS_WITH_GENERIC, CMD_RT_INTERFACES, CMD_RT_METHODS,
        CMD_RT_METHODS_WITH_GENERIC, CMD_RT_STATUS, CMD_VM_CLASSES_BY_SIGNATURE, CS_REF_TYPE,
        CS_VM,
    };
    let named: Vec<u64> = match (command_set, command) {
        // Wave 103 (interpreter round i5, lane L1): the member listings too,
        // which refuse an unprepared class (`commands::read_prepared_class`).
        (
            CS_REF_TYPE,
            CMD_RT_STATUS
            | CMD_RT_FIELDS
            | CMD_RT_FIELDS_WITH_GENERIC
            | CMD_RT_METHODS
            | CMD_RT_METHODS_WITH_GENERIC
            | CMD_RT_INTERFACES,
        ) => match protocol::PayloadReader::new(data).read_u64_be() {
            Ok(id) => vec![id],
            Err(_) => return,
        },
        (CS_VM, CMD_VM_CLASSES_BY_SIGNATURE) => {
            let Ok(signature) = protocol::PayloadReader::new(data).read_string() else {
                return;
            };
            // Interpreter round i5 wave 103 hand-back (lane L1;
            // `L1W103JdiInstances` on w103b: `classesByName("Item[]")` was
            // empty): this VM makes an array class only on demand, so an
            // array signature's classes are made first, as
            // `VirtualMachine.VisibleClasses` makes the primitive ones.
            if signature.starts_with('[') {
                inspect::materialize_array_classes(shared, &signature);
            }
            let ds = shared.debug.debug_state.lock();
            ds.class_signatures
                .iter()
                .filter(|(_, sig)| **sig == signature)
                .map(|(&id, _)| id)
                .collect()
        }
        _ => return refresh_class_statuses(shared),
    };
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    let hide = ds.hide_unprepared_classes;
    for id in named {
        let class = u32::try_from(id)
            .ok()
            .and_then(|raw| cm.class_store.get(crate::classloading::ClassId::new(raw)));
        if let Some(class) = class {
            ds.class_statuses.insert(id, jdwp_class_status(class));
            // Wave 103: `ClassesBySignature` leaves an unprepared class out.
            if hide && jdwp_class_unprepared(shared, &cm, class, ds.class_prepare_watermark) {
                ds.unprepared_classes.insert(id);
            } else {
                ds.unprepared_classes.remove(&id);
            }
        }
    }
}

/// The commands that answer a class's status, before which the server
/// refreshes it ([`refresh_class_statuses`], wave 15):
/// `VirtualMachine.ClassesBySignature` and `AllClassesWithGeneric`, and
/// (wave 23) `ReferenceType.Status`.
pub(in crate::debug) fn reads_class_status(command_set: u8, command: u8) -> bool {
    use commands::{
        CMD_RT_FIELDS, CMD_RT_FIELDS_WITH_GENERIC, CMD_RT_INTERFACES, CMD_RT_METHODS,
        CMD_RT_METHODS_WITH_GENERIC, CMD_RT_STATUS, CMD_VM_ALL_CLASSES_WITH_GENERIC,
        CMD_VM_CLASSES_BY_SIGNATURE, CS_REF_TYPE, CS_VM,
    };
    matches!(
        (command_set, command),
        (
            CS_VM,
            CMD_VM_CLASSES_BY_SIGNATURE | CMD_VM_ALL_CLASSES_WITH_GENERIC
        ) | (
            CS_REF_TYPE,
            CMD_RT_STATUS
                | CMD_RT_FIELDS
                | CMD_RT_FIELDS_WITH_GENERIC
                | CMD_RT_METHODS
                | CMD_RT_METHODS_WITH_GENERIC
                | CMD_RT_INTERFACES
        )
    )
}

/// Record one class's debugger metadata (see [`populate_class_metadata`]).
pub(in crate::debug) fn add_class_metadata(
    ds: &mut DebugState,
    class: &crate::classloading::Class,
) -> (u64, String, u8) {
    {
        let class_id = class.id.as_u32() as u64;
        // An array class's name is already its JNI signature (`[I`,
        // `[Ljava/lang/String;`) and its JDWP type tag is ARRAY (wave 8: it
        // was registered as `L[I;`, a CLASS, so `ClassesBySignature("[I")`
        // found nothing and an array's `ReferenceType` named a class no
        // debugger could describe).
        let is_array = class.name.starts_with('[');
        let type_tag = if is_array {
            3u8
        } else if class.is_interface() {
            2u8
        } else {
            1u8
        };
        let jni_sig = if is_array {
            class.name.to_string()
        } else {
            format!("L{};", class.name)
        };

        ds.class_type_tags.insert(class_id, type_tag);
        ds.class_signatures.insert(class_id, jni_sig.clone());

        if let Some(ref sf) = class.source_file {
            ds.class_source_files.insert(class_id, sf.to_string());
        }
        // `ClassType.Superclass` read this map, and nothing filled it: every
        // class answered "no superclass". An interface has none (interpreter
        // round i1 wave 43: its class file's `java/lang/Object` was
        // answered; HotSpot answers null, as JVMTI `GetSuperclass` does).
        if let Some(sup) = class.superclass.filter(|_| !class.is_interface()) {
            ds.class_superclass
                .insert(class_id, u64::from(sup.as_u32()));
        }

        // Wave 23: the modifiers, generic signatures, superinterfaces,
        // class-file version and code lengths the JDI session's other
        // `ReferenceType` / `Method` commands answer from.
        let mut details = ClassDetails {
            modifiers: jdwp_class_modifiers(class),
            generic_signature: class.signature.clone(),
            interfaces: if is_array {
                Vec::new()
            } else {
                class
                    .interfaces
                    .iter()
                    .map(|i| u64::from(i.as_u32()))
                    .collect()
            },
            version: (!is_array).then_some((class.version.major, class.version.minor)),
            ..ClassDetails::default()
        };

        // Populate methods
        let mut methods = Vec::new();
        for (_i, m) in class.methods.iter().enumerate() {
            let method_id = jdwp_method_id(&m.name, &m.descriptor);
            let flags = u32::from(m.access_flags.bits());
            // Wave 23: the modifiers HotSpot's back end writes — JVMTI's
            // recognised method modifiers, and `MOD_SYNTHETIC` for a
            // synthetic method (`canGetSyntheticAttribute` is advertised).
            // The raw flags went out, and no method was ever synthetic to JDI.
            let synthetic = flags & ACC_SYNTHETIC != 0 || has_synthetic_attribute(&m.attributes);
            methods.push(commands::MethodInfo {
                method_id: ids::MethodId(method_id),
                name: m.name.to_string(),
                signature: m.descriptor.to_string(),
                mod_bits: jdwp_member_modifiers(flags, JVM_RECOGNIZED_METHOD_MODIFIERS, synthetic),
            });
            if let Some(generic) = signature_attribute(&m.attributes) {
                details.method_generics.insert(method_id, generic);
            }
            // `Method.LineTable` (interpreter round i1 wave 5, lane L1): the
            // table jdb resolves `stop at Class:line` through, and that a LINE
            // step is shown against. It was declared and read here but never
            // filled, so every method answered an empty table.
            if let Some(code) = m.code() {
                // Widening: a method's code is shorter than 64 KiB.
                details
                    .code_lengths
                    .insert(method_id, code.code.len() as u64);
                let generic_vars: Vec<(usize, u64, String)> = code
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        cratonvm_reader::attribute::Attribute::LocalVariableTypeTable(entries) => {
                            Some(entries)
                        }
                        _ => None,
                    })
                    .flatten()
                    .filter_map(|e| {
                        Some((
                            usize::from(e.index),
                            u64::from(e.start_pc),
                            class.constant_pool.get_utf8(e.signature_index)?.to_string(),
                        ))
                    })
                    .collect();
                if !generic_vars.is_empty() {
                    details.variable_generics.insert(method_id, generic_vars);
                }
                let mut lines: Vec<(u64, i32)> = code
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        cratonvm_reader::attribute::Attribute::LineNumberTable(entries) => {
                            Some(entries)
                        }
                        _ => None,
                    })
                    .flatten()
                    .map(|e| (u64::from(e.start_pc), i32::from(e.line_number)))
                    .collect();
                if !lines.is_empty() {
                    lines.sort_by_key(|l| l.0);
                    ds.method_line_tables.insert((class_id, method_id), lines);
                }
                // `Method.VariableTable` (wave 6): read by the command handler
                // and, like the line tables were, never filled, so jdb's
                // `locals` / `print x` answered "Local variable information
                // not available" for every method compiled with `-g`.
                let vars: Vec<VariableInfo> = code
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        cratonvm_reader::attribute::Attribute::LocalVariableTable(entries) => {
                            Some(entries)
                        }
                        _ => None,
                    })
                    .flatten()
                    .filter_map(|e| {
                        Some(VariableInfo {
                            code_index: u64::from(e.start_pc),
                            name: class.constant_pool.get_utf8(e.name_index)?.to_string(),
                            signature: class
                                .constant_pool
                                .get_utf8(e.descriptor_index)?
                                .to_string(),
                            length: usize::from(e.length),
                            slot: usize::from(e.index),
                        })
                    })
                    .collect();
                if !vars.is_empty() {
                    ds.method_variables.insert((class_id, method_id), vars);
                }
            }
        }
        ds.class_methods.insert(class_id, methods);

        // Populate fields. A field id names the field VM-wide
        // ([`jdwp_field_id`], wave 8): it was the field's index in its class,
        // which every class shares.
        let mut fields = Vec::new();
        for (i, f) in class.fields.iter().enumerate() {
            let field_id = jdwp_field_id(class.id, i);
            let flags = u32::from(f.access_flags.bits());
            let synthetic = flags & ACC_SYNTHETIC != 0 || has_synthetic_attribute(&f.attributes);
            fields.push(commands::FieldInfo {
                field_id: ids::FieldId(field_id),
                name: f.name.to_string(),
                signature: f.descriptor.to_string(),
                mod_bits: jdwp_member_modifiers(flags, JVM_RECOGNIZED_FIELD_MODIFIERS, synthetic),
            });
            if let Some(generic) = signature_attribute(&f.attributes) {
                details.field_generics.insert(field_id, generic);
            }
        }
        ds.class_fields.insert(class_id, fields);
        ds.class_details.insert(class_id, details);
        (class_id, jni_sig, type_tag)
    }
}

/// Class-file access flags the JDWP modifiers below are built from.
pub(in crate::debug) const ACC_PUBLIC: u32 = 0x0001;
pub(in crate::debug) const ACC_FINAL: u32 = 0x0010;
pub(in crate::debug) const ACC_SUPER: u32 = 0x0020;
pub(in crate::debug) const ACC_ABSTRACT: u32 = 0x0400;
pub(in crate::debug) const ACC_SYNTHETIC: u32 = 0x1000;
/// HotSpot's `JVM_RECOGNIZED_METHOD_MODIFIERS` / `JVM_RECOGNIZED_FIELD_MODIFIERS`
/// / `JVM_ACC_WRITTEN_FLAGS`: what JVMTI's `GetMethodModifiers`,
/// `GetFieldModifiers` and `GetClassModifiers` keep of the class file's flags.
pub(in crate::debug) const JVM_RECOGNIZED_METHOD_MODIFIERS: u32 = 0x1DFF;
pub(crate) const JVM_RECOGNIZED_FIELD_MODIFIERS: u32 = 0x50DF;
pub(in crate::debug) const JVM_ACC_WRITTEN_FLAGS: u32 = 0x7FFF;
/// JDWP's synthetic marker in a field's or method's `modBits` (HotSpot's
/// back end `MOD_SYNTHETIC`; JDI `VMModifiers.SYNTHETIC`).
pub(crate) const MOD_SYNTHETIC: u32 = 0xF000_0000;

/// A field's or method's JDWP `modBits` (interpreter round i1 wave 23): the
/// class file's `flags` that JVMTI recognises (`recognized`), with
/// [`MOD_SYNTHETIC`] for a synthetic member, as HotSpot's back end writes
/// them (`writeMethodInfo` / `writeFieldInfo`).
pub(in crate::debug) fn jdwp_member_modifiers(flags: u32, recognized: u32, synthetic: bool) -> u32 {
    let modifiers = flags & recognized;
    if synthetic {
        modifiers | MOD_SYNTHETIC
    } else {
        modifiers
    }
}

/// Does a member carry the `Synthetic` attribute (JVMS §4.7.8)? HotSpot's
/// class-file parser turns it into `ACC_SYNTHETIC`.
pub(crate) fn has_synthetic_attribute(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
) -> bool {
    attributes.iter().any(|a| {
        matches!(
            a.as_decoded(),
            Some(cratonvm_reader::attribute::Attribute::Synthetic)
        )
    })
}

/// A member's `Signature` attribute (its generic signature), if any.
pub(crate) fn signature_attribute(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
) -> Option<String> {
    attributes.iter().find_map(|a| match a.as_decoded() {
        Some(cratonvm_reader::attribute::Attribute::Signature(s)) => Some(s.to_string()),
        _ => None,
    })
}

/// `ReferenceType.Modifiers` of `class` (interpreter round i1 wave 23), as
/// HotSpot's JVMTI `GetClassModifiers` answers it: the flags of the class's
/// own `InnerClasses` entry when it has one (a member, local or anonymous
/// class: `static`, `private`, ... live there), else the class file's
/// flags, limited to the written flags, with `ACC_SUPER` as the class file
/// has it; an array class is `public final abstract` (HotSpot takes the
/// visibility of a reference array's element class; this answers `public`
/// for every array). It was not served at all (`NOT_IMPLEMENTED`), which JDI
/// asks as soon as it lists a class's methods. Wave 42 (lane L1): an array
/// class has `ACC_SUPER` too, `0x431` for `String[]` and `int[]` alike on
/// HotSpot 25.0.3 (`tools/probes/interp/L1/L1W42RawJdwpErrorAnswers.java`);
/// it answered `0x411`. Interpreter round i1 wave 52 (lane L1): an interface
/// has `ACC_SUPER` too, although its class file may not set it: HotSpot
/// 25.0.3's `GetClassModifiers` answers `0x621` for `Runnable` and `0x628`
/// for a nested `static interface` (measured,
/// `tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.java`; HotSpot
/// treats every class as `ACC_SUPER`); it answered `0x601` / `0x608`.
pub(crate) fn jdwp_class_modifiers(class: &crate::classloading::Class) -> u32 {
    if class.name.starts_with('[') {
        return ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_ABSTRACT;
    }
    let own = u32::from(class.access_flags.bits());
    let access = class
        .inner_classes
        .iter()
        .find(|e| e.inner_class.as_str() == &*class.name)
        .map_or(own, |e| u32::from(e.access_flags));
    let modifiers = access & !ACC_SUPER & JVM_ACC_WRITTEN_FLAGS;
    modifiers | ACC_SUPER
}

/// Replace DebugState's thread table with the thread registry's live threads.
///
/// Wave 8: it was filled once, at attach, so a thread started later had no
/// name and was missing from `AllThreads`, and one that had died stayed
/// listed. The server now refreshes it before every command that lists,
/// names or targets a thread ([`refreshes_thread_names`]). Takes the registry
/// lock, then the debug-state lock.
pub(in crate::debug) fn populate_thread_metadata(shared: &crate::vm::SharedVm) {
    let registry = &shared.threads.thread_registry;
    let names = registry.all_thread_names();
    // Wave 11: each thread's JDWP status, read before the debug-state lock
    // (the registry lock is taken per thread, never under the debug state).
    // Wave 25: with the class manager, which tells a sleep entered through
    // the registered `Thread.sleep` native (no `Thread` frame is deposited)
    // from another timed wait ([`frame_invokes_thread_sleep`]).
    let statuses: Vec<(u64, u32)> = {
        let cm = shared.classes.class_manager.read();
        let invokes_sleep =
            |entry: &cratonvm_native_api::StackTraceEntry| frame_invokes_thread_sleep(&cm, entry);
        names
            .iter()
            .map(|(tid, _)| {
                (
                    tid.0,
                    jdwp_thread_status_with(registry, *tid, &invokes_sleep),
                )
            })
            .collect()
    };
    // Wave 49 (lane L1): the virtual threads, which `AllThreads` leaves out
    // (`commands::listed_thread_ids`), also those started before the
    // debugger attached (no `ThreadStart` recorded them). Read before the
    // debug-state lock, as the manager's lock is its own.
    let manager = &shared.threads.virtual_thread_manager;
    let virtual_ids: Vec<u64> = names
        .iter()
        .map(|(tid, _)| tid.0)
        .filter(|&tid| manager.is_virtual(tid))
        .collect();
    let mut ds = shared.debug.debug_state.lock();
    ds.virtual_threads.extend(virtual_ids);
    // The heap service is the back end's own thread (wave 10), as HotSpot's
    // back end hides its own: never listed, so never suspended or named.
    let hidden = ds.heap_service_thread();
    ds.thread_names = names
        .into_iter()
        .filter(|(tid, _)| Some(tid.0) != hidden)
        .map(|(tid, name)| (tid.0, name))
        .collect();
    // Wave 45 (lane L1): a thread parked at a native's return keeps the
    // status HotSpot keeps there (`native_exit_statuses`).
    let statuses: HashMap<u64, u32> = statuses
        .into_iter()
        .filter(|(tid, _)| Some(*tid) != hidden)
        .map(|(tid, status)| match ds.native_exit_statuses.get(&tid) {
            Some(&parked) if status == commands::THREAD_STATUS_RUNNING => (tid, parked),
            _ => (tid, status),
        })
        .collect();
    ds.thread_statuses = statuses;
    // Interpreter round i5 wave 104 (lane L1): once `main` has ended, its id
    // names the ended `main` (`ids::ENDED_MAIN_VM_ID`): its name, ZOMBIE,
    // which `AllThreads` leaves out, as HotSpot answers a terminated thread.
    if let Some(name) = ds.destroy_java_vm_split.clone() {
        ds.thread_names.insert(ids::ENDED_MAIN_VM_ID, name);
        ds.thread_statuses
            .insert(ids::ENDED_MAIN_VM_ID, commands::THREAD_STATUS_ZOMBIE);
    }
}

/// [`jdwp_thread_status`], where `invokes_sleep` answers whether the
/// deposited top frame is calling `Thread.sleep*` (interpreter round i1 wave
/// 25): this VM registers `Thread.sleep(long)` as a native, so a sleeping
/// thread deposits no `Thread` frame at all — its top frame is the caller
/// (`sleepQuietly`), and the status read `WAIT` where HotSpot says
/// `SLEEPING` (`L1W24JdiThreads`, `L1W24JdiSurface`).
pub(in crate::debug) fn jdwp_thread_status_with(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    tid: crate::threading::ThreadId,
    invokes_sleep: &dyn Fn(&cratonvm_native_api::StackTraceEntry) -> bool,
) -> u32 {
    use commands::{
        THREAD_STATUS_MONITOR, THREAD_STATUS_RUNNING, THREAD_STATUS_SLEEPING, THREAD_STATUS_WAIT,
        THREAD_STATUS_ZOMBIE,
    };
    if !registry.is_alive(tid) {
        return THREAD_STATUS_ZOMBIE;
    }
    match registry.java_block_state(tid) {
        1 => THREAD_STATUS_WAIT,
        2 => THREAD_STATUS_MONITOR,
        // Interpreter round i2 wave 63, lane L1 (`CRATONVM_PUBLISHED_WAIT_KIND`):
        // the sleep's own region says whether it is a sleep, as HotSpot's
        // `JavaThreadSleepState` does, whatever frame entered it.
        3 if registry.published_wait_kind() => {
            if registry.published_sleeping(tid) == Some(true) {
                THREAD_STATUS_SLEEPING
            } else {
                THREAD_STATUS_WAIT
            }
        }
        3 => {
            // The deposited trace is outermost-first
            // (`stackwalker::capture_frames_no_lines`): the frame that called
            // the blocking native is the LAST one. Wave 12: this read the
            // first, the thread's bottom frame (`main`, `Thread.run`), so a
            // sleeping thread was never `SLEEPING`.
            let trace = registry.frame_trace_of(tid);
            if trace
                .last()
                .is_some_and(|top| is_thread_sleep_frame(top) || invokes_sleep(top))
            {
                THREAD_STATUS_SLEEPING
            } else {
                THREAD_STATUS_WAIT
            }
        }
        _ => THREAD_STATUS_RUNNING,
    }
}

/// Is `frame` one of `java.lang.Thread`'s sleep methods (`sleep`,
/// `sleepNanos`, `sleep0`, `sleepNanos0`)? The class name is accepted in
/// either spelling the frame capture may use.
pub(crate) fn is_thread_sleep_frame(frame: &cratonvm_native_api::StackTraceEntry) -> bool {
    matches!(&*frame.class_name, "java.lang.Thread" | "java/lang/Thread")
        && frame.method_name.starts_with("sleep")
}

/// Is the deposited frame `entry` stopped at an invoke of
/// `java.lang.Thread.sleep*` (interpreter round i1 wave 25)? Read from the
/// method's bytecode at the entry's `byte_code_index` (the invoke the
/// blocking native was entered from: the dispatch loop sets
/// `last_instr_pc` at every instruction) and the constant pool's method
/// reference. The deposit keeps no descriptor, so every method of that name
/// is asked; `false` when the class or the code is not there.
pub(crate) fn frame_invokes_thread_sleep(
    cm: &crate::classloading::ClassManager,
    entry: &cratonvm_native_api::StackTraceEntry,
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let (Some(class_id), Ok(bci)) = (entry.class_id, usize::try_from(entry.byte_code_index)) else {
        return false;
    };
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    class
        .methods
        .iter()
        .filter(|m| &*m.name == &*entry.method_name)
        .any(|m| {
            let Some(code) = m.code() else {
                return false;
            };
            let code: &[u8] = &code.code[..];
            // invokevirtual, invokespecial, invokestatic
            if !matches!(code.get(bci), Some(0xb6..=0xb8)) {
                return false;
            }
            let (Some(&hi), Some(&lo)) = (code.get(bci + 1), code.get(bci + 2)) else {
                return false;
            };
            let Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }) = class.constant_pool.get(u16::from_be_bytes([hi, lo]))
            else {
                return false;
            };
            class.constant_pool.get_class_name(*class_index) == Some("java/lang/Thread")
                && class
                    .constant_pool
                    .get_name_and_type(*name_and_type_index)
                    .is_some_and(|(name, _)| name.starts_with("sleep"))
        })
}

/// The commands before which the server refreshes its thread table
/// ([`populate_thread_metadata`]): `VirtualMachine.AllThreads`,
/// `ThreadGroupReference.Children`, `ThreadReference.Name` and the commands
/// whose target must be a live thread: the two invocations and (wave 9)
/// `StackFrame.SetValues`; (wave 11) `ThreadReference.Status`,
/// `ThreadGroup`, `Suspend`, `Frames` and `FrameCount`, which answer from, or
/// check the target against, the refreshed thread states; (wave 24)
/// `ClassType.NewInstance`, `InterfaceType.InvokeMethod`,
/// `ThreadReference.Interrupt` / `Stop` and `TopLevelThreadGroups`, which
/// read the thread groups of the listed threads; (wave 25) the owned- and
/// contended-monitor commands and `IsVirtual`.
pub(in crate::debug) fn refreshes_thread_names(command_set: u8, command: u8) -> bool {
    use commands::{
        CMD_CT_INVOKE_METHOD, CMD_CT_NEW_INSTANCE, CMD_IT_INVOKE_METHOD, CMD_OR_INVOKE_METHOD,
        CMD_SF_SET_VALUES, CMD_TGR_CHILDREN, CMD_TR_FRAMES, CMD_TR_FRAME_COUNT, CMD_TR_INTERRUPT,
        CMD_TR_NAME, CMD_TR_STATUS, CMD_TR_STOP, CMD_TR_SUSPEND, CMD_TR_THREAD_GROUP,
        CMD_VM_ALL_THREADS, CMD_VM_TOP_LEVEL_THREAD_GROUPS, CS_CLASS_TYPE, CS_INTERFACE_TYPE,
        CS_OBJECT_REF, CS_STACK_FRAME, CS_THREAD_GROUP_REF, CS_THREAD_REF, CS_VM,
    };
    matches!(
        (command_set, command),
        (CS_VM, CMD_VM_ALL_THREADS | CMD_VM_TOP_LEVEL_THREAD_GROUPS)
            | (CS_THREAD_GROUP_REF, CMD_TGR_CHILDREN)
            | (CS_THREAD_REF, CMD_TR_NAME)
            | (
                CS_THREAD_REF,
                CMD_TR_STATUS
                    | CMD_TR_THREAD_GROUP
                    | CMD_TR_SUSPEND
                    | CMD_TR_FRAMES
                    | CMD_TR_FRAME_COUNT
                    | CMD_TR_INTERRUPT
                    | CMD_TR_STOP
            )
            | (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD | CMD_CT_NEW_INSTANCE)
            | (CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD)
            | (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD)
            | (CS_STACK_FRAME, CMD_SF_SET_VALUES)
    ) || inspect::is_monitor_command(command_set, command)
        || (command_set, command) == (CS_THREAD_REF, commands::CMD_TR_IS_VIRTUAL)
}

/// Rewrite, in place, the thread and class ids at the head of a command's
/// payload from their wire form to the VM's (interpreter round i1 wave 25;
/// see `ids::thread_to_wire`): the thread every `ThreadReference` and
/// `StackFrame` command names first, the class every `ReferenceType`,
/// `ClassType`, `ArrayType`, `InterfaceType` and `Method` command names
/// first, and the thread (and, for `ObjectReference.InvokeMethod`, the class)
/// an invocation names after its receiver or class. Ids further in (event
/// modifiers, tagged values, object ids that name threads) are decoded where
/// they are read (`commands::handle_er_set`, `object_for_id`). A payload too
/// short for an id is left alone: its handler refuses it.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::debug) fn ids_from_wire(command_set: u8, command: u8, data: &mut [u8]) {
    ids_from_wire_split(command_set, command, data, false);
}

/// [`ids_from_wire`] with the `DestroyJavaVM` split (`split`; interpreter
/// round i5 wave 104, lane L1): a thread id decodes through
/// `ids::thread_from_wire_split`. What the server runs.
pub(in crate::debug) fn ids_from_wire_split(command_set: u8, command: u8, data: &mut [u8], split: bool) {
    use commands::{
        CMD_CT_INVOKE_METHOD, CMD_CT_NEW_INSTANCE, CMD_IT_INVOKE_METHOD, CMD_OR_INVOKE_METHOD,
        CS_ARRAY_TYPE, CS_CLASS_TYPE, CS_INTERFACE_TYPE, CS_METHOD, CS_OBJECT_REF, CS_REF_TYPE,
        CS_STACK_FRAME, CS_THREAD_REF,
    };
    fn rewrite(data: &mut [u8], at: usize, decode: &dyn Fn(u64) -> u64) {
        let Some(bytes) = data.get_mut(at..at + 8) else {
            return;
        };
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        bytes.copy_from_slice(&decode(u64::from_be_bytes(raw)).to_be_bytes());
    }
    let thread = |wire: u64| ids::thread_from_wire_split(split, wire);
    match command_set {
        CS_THREAD_REF | CS_STACK_FRAME => rewrite(data, 0, &thread),
        CS_REF_TYPE | CS_CLASS_TYPE | CS_ARRAY_TYPE | CS_INTERFACE_TYPE | CS_METHOD => {
            rewrite(data, 0, &ids::class_from_wire)
        }
        _ => {}
    }
    match (command_set, command) {
        (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD | CMD_CT_NEW_INSTANCE)
        | (CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD) => rewrite(data, 8, &thread),
        (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD) => {
            rewrite(data, 8, &thread);
            rewrite(data, 16, &ids::class_from_wire);
        }
        _ => {}
    }
}

/// `INVALID_OBJECT` for a command whose leading thread or reference-type id
/// names nothing at all (interpreter round i1 wave 43, lane L1;
/// `docs/feature-designs/jvmti-delivery-threading.md`).
///
/// HotSpot's back end reads a `threadID` / `referenceTypeID` as an object id
/// first: an id its object table does not hold is `INVALID_OBJECT` (20),
/// which JDI turns into `ObjectCollectedException`; only an id naming an
/// object of the wrong kind is `INVALID_THREAD` (10) / `INVALID_CLASS` (21)
/// (`tools/probes/interp/L1/L1W42RawJdwpErrorAnswers.java`, measured on
/// HotSpot 25.0.3). The handlers answered 10 / 21 for both. `None` (the
/// handler answers) when the id names anything the server knows under any
/// of its id spaces: a thread (the server's tables or the registry), a class
/// (the class store, read here so a class defined since the last poll
/// counts), an exported object, or the thread-group id; and for JDWP's null
/// (wire 0), whose answer is the handler's. `data` holds the VM ids
/// ([`ids_from_wire`] ran): the wire id is rebuilt so each table is asked
/// in its own decoding (a thread's wire id sent to a `ReferenceType` command
/// still names that thread, and is `INVALID_CLASS`).
///
/// Lock order: the class manager, released, then the debug state, then the
/// registry's thread map innermost (as [`thread_object_for_id`]).
pub(in crate::debug) fn unknown_id_refusal(
    shared: &crate::vm::SharedVm,
    command_set: u8,
    data: &[u8],
) -> Option<commands::CommandResult> {
    use commands::{
        CS_ARRAY_TYPE, CS_CLASS_TYPE, CS_INTERFACE_TYPE, CS_METHOD, CS_REF_TYPE, CS_STACK_FRAME,
        CS_THREAD_REF,
    };
    let vm_id = protocol::PayloadReader::new(data).read_u64_be().ok()?;
    if vm_id == ids::NO_VM_ID {
        return None;
    }
    let wire = match command_set {
        // Wave 104 (lane L1): with the `DestroyJavaVM` split.
        CS_THREAD_REF | CS_STACK_FRAME => ids::thread_to_wire_split(
            shared.debug.debugger_gates.destroy_java_vm_split(),
            vm_id,
        ),
        CS_REF_TYPE | CS_CLASS_TYPE | CS_ARRAY_TYPE | CS_INTERFACE_TYPE | CS_METHOD => {
            ids::class_to_wire(vm_id)
        }
        _ => return None,
    };
    if id_names_something(shared, wire) {
        None
    } else {
        Some(commands::CommandResult::error(commands::ERR_INVALID_OBJECT))
    }
}

/// Does wire id `wire` name a thread, a class, an exported object or the
/// thread group ([`unknown_id_refusal`])?
pub(in crate::debug) fn id_names_something(shared: &crate::vm::SharedVm, wire: u64) -> bool {
    if wire == commands::SYSTEM_THREAD_GROUP_ID {
        return true;
    }
    let class = ids::class_from_wire(wire);
    let in_class_store = u32::try_from(class).is_ok_and(|raw| {
        shared
            .classes
            .class_manager
            .read()
            .class_store
            .get(crate::classloading::ClassId::new(raw))
            .is_some()
    });
    if in_class_store {
        return true;
    }
    let ds = shared.debug.debug_state.lock();
    // Wave 104 (lane L1): with the `DestroyJavaVM` split.
    let thread = ds.vm_thread(wire);
    if ds.objects.handle_of(wire).is_some()
        || ds.class_signatures.contains_key(&class)
        || ds.class_methods.contains_key(&class)
        || commands::thread_status_of(&ds, thread).is_some()
    {
        return true;
    }
    thread < ids::HEAP_OBJECT_ID_BASE
        && shared
            .threads
            .thread_registry
            .java_thread_obj(crate::threading::ThreadId(thread))
            .is_some()
}

/// The commands that read a thread's published frames (wave 12):
/// `ThreadReference.Frames` / `FrameCount` and `StackFrame.GetValues` /
/// `ThisObject`; and (wave 25) the owned-monitor commands, whose stack
/// depths are places in that listing (`inspect::is_monitor_command`). Each
/// names the thread first.
pub(in crate::debug) fn reads_thread_frames(command_set: u8, command: u8) -> bool {
    use commands::{
        CMD_SF_GET_VALUES, CMD_SF_THIS_OBJECT, CMD_TR_FRAMES, CMD_TR_FRAME_COUNT, CS_STACK_FRAME,
        CS_THREAD_REF,
    };
    matches!(
        (command_set, command),
        (CS_THREAD_REF, CMD_TR_FRAMES | CMD_TR_FRAME_COUNT)
            | (CS_STACK_FRAME, CMD_SF_GET_VALUES | CMD_SF_THIS_OBJECT)
    ) || inspect::is_monitor_command(command_set, command)
}

/// The commands the server answers from the VM's own state, with neither the
/// heap nor a thread (interpreter round i1 wave 24): `VirtualMachine.ClassPaths`
/// and `Method.Bytecodes`; (wave 25) `VirtualMachine.Version` and
/// `ThreadReference.IsVirtual`.
/// `None` for every other command.
pub(in crate::debug) fn serve_with_the_vm(
    shared: &crate::vm::SharedVm,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> Option<commands::CommandResult> {
    match (command_set, command) {
        (commands::CS_VM, commands::CMD_VM_VERSION) => Some(vm_version(shared)),
        (commands::CS_VM, commands::CMD_VM_CLASS_PATHS) => Some(class_paths(shared)),
        (commands::CS_METHOD, commands::CMD_M_BYTECODES) => Some(method_bytecodes(shared, data)),
        (commands::CS_REF_TYPE, commands::CMD_RT_SOURCE_DEBUG_EXTENSION) => {
            Some(source_debug_extension(shared, data))
        }
        // Interpreter round i5 wave 103 (lane L1).
        (commands::CS_REF_TYPE, commands::CMD_RT_CONSTANT_POOL) => {
            Some(constant_pool(shared, data))
        }
        // Interpreter round i5 wave 104 (lane L1): `canRedefineClasses`.
        (commands::CS_VM, commands::CMD_VM_REDEFINE_CLASSES) => {
            Some(redefine_classes(shared, data))
        }
        (commands::CS_THREAD_REF, commands::CMD_TR_IS_VIRTUAL) => {
            Some(thread_is_virtual(shared, data))
        }
        (commands::CS_THREAD_REF, commands::CMD_TR_FORCE_EARLY_RETURN) => {
            Some(early_return::force_early_return(shared, data))
        }
        // Wave 44: on the frame's parked thread (`debug::pop_frames`).
        (commands::CS_STACK_FRAME, commands::CMD_SF_POP_FRAMES) => {
            Some(pop_frames::pop_frames(shared, data))
        }
        _ => None,
    }
}

/// `VirtualMachine.Version` (1/1), as the JDK the VM runs answers it
/// (interpreter round i1 wave 25; proposal
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// the JDWP version is the JDK's (`java.specification.version`: `25` is
/// JDWP 25.0, `1.8` is 1.8), `vmVersion` its `java.version` and `vmName` its
/// `java.vm.name`, as HotSpot's back end answers (`VirtualMachine.c`
/// `version`); the description names this server. The server answered JDWP
/// 1.8 and `vmVersion` "1.8.0" whatever the JDK, so JDI took a JDK 25 program
/// for a JDK 8 one: `canGetModuleInfo()` was false (`ReferenceType.module()`
/// and `allModules()` threw) and JDI never asked whether a thread is virtual.
/// The commands JDI sends only to a 9+ / 19+ target — `ReferenceType.Module`,
/// `VirtualMachine.AllModules`, `ModuleReference.Name` / `ClassLoader`,
/// `ThreadReference.IsVirtual` and the `PlatformThreadsOnly` modifier (read
/// off a HotSpot session, `tools/probes/interp/L1/L1W25JdiVersion.java`) —
/// are served since the same wave. Without the properties (a bare VM), 1.8.
pub(in crate::debug) fn vm_version(shared: &crate::vm::SharedVm) -> commands::CommandResult {
    let (spec, java_version, vm_name, vm_info) = {
        let props = shared.system_properties.read();
        let get = |key: &str| props.get(key).cloned();
        (
            get("java.specification.version"),
            get("java.version"),
            get("java.vm.name"),
            get("java.vm.info"),
        )
    };
    let (major, minor) = spec.as_deref().and_then(jdwp_version_of).unwrap_or((1, 8));
    let java_version = java_version.unwrap_or_else(|| "1.8.0".to_string());
    // Interpreter round i6 wave 118, lane L1 (i61-L1 stage 2): the identity
    // table's name without a property map, not a literal of its own.
    let vm_name = vm_name.unwrap_or_else(|| {
        cratonvm_types::vm_properties::vm_identity_value("java.vm.name")
            .unwrap_or_default()
            .to_string()
    });
    let vm_info = vm_info.unwrap_or_default();
    let mut pw = protocol::PayloadWriter::new();
    // Wave 118: the back end's own description (`VirtualMachineImpl.c`
    // `version()`, MEASURED on HotSpot 25.0.3, `L1W118VmIdentityAnswers`):
    // the JDWP version, the JVMTI version (JDK 9+ numbers it as the JDK,
    // the same pair) and `java.version (java.vm.name, java.vm.info)`. It
    // read "CratonVM JDWP Debug Server, ..." without the JVMTI line or
    // `java.vm.info`.
    pw.put_string(&format!(
        "Java Debug Wire Protocol (Reference Implementation) version {major}.{minor}\n\
         JVM Debug Interface version {major}.{minor}\n\
         JVM version {java_version} ({vm_name}, {vm_info})"
    ));
    pw.put_u32_be(major);
    pw.put_u32_be(minor);
    pw.put_string(&java_version);
    pw.put_string(&vm_name);
    commands::CommandResult::ok(pw.into_bytes())
}

/// The JDWP `(major, minor)` of a JDK whose `java.specification.version` is
/// `spec`: `(25, 0)` for `25`, `(1, 8)` for `1.8` (wave 25).
pub(in crate::debug) fn jdwp_version_of(spec: &str) -> Option<(u32, u32)> {
    let mut parts = spec.trim().split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = match parts.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    Some((major, minor))
}

/// `ThreadReference.IsVirtual` (11/15, JDWP 21; interpreter round i1 wave
/// 25): whether the thread is one of the VM's virtual threads
/// (`VirtualThreadManager::is_virtual`). JDI asks it only of a JDWP 19+
/// target, and jdb asks it of every thread event's thread.
/// `INVALID_THREAD` for an id naming no thread the session knows.
pub(in crate::debug) fn thread_is_virtual(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    let Ok(tid) = protocol::PayloadReader::new(data).read_u64_be() else {
        return commands::CommandResult::error(commands::ERR_INTERNAL);
    };
    let (known, started_virtual) = {
        let ds = shared.debug.debug_state.lock();
        (
            commands::thread_status_of(&ds, tid).is_some(),
            ds.virtual_threads.contains(&tid),
        )
    };
    if !known {
        return commands::CommandResult::error(commands::ERR_INVALID_THREAD);
    }
    let is_virtual = started_virtual || shared.threads.virtual_thread_manager.is_virtual(tid);
    commands::CommandResult::ok(vec![u8::from(is_virtual)])
}

/// `Method.Bytecodes` (6/3, interpreter round i1 wave 24): the method's
/// class-file bytecode, as JVMTI `GetBytecodes` answers it — `NATIVE_METHOD`
/// for a native method, no bytes for an abstract one; `INVALID_CLASS` /
/// `INVALID_METHODID` for ids naming none. It answered zero bytes for every
/// method (from a table nothing filled), so `canGetBytecodes` answered false,
/// which turns off what a debugger builds on the bytecode (IntelliJ's smart
/// step into, method-breakpoint emulation); HotSpot answers true.
pub(in crate::debug) fn method_bytecodes(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{CommandResult, ERR_INTERNAL, ERR_INVALID_CLASS, ERR_INVALID_METHODID};
    let mut r = protocol::PayloadReader::new(data);
    let (Ok(class_id), Ok(method_id)) = (r.read_u64_be(), r.read_u64_be()) else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(class_id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.class_store.get(crate::classloading::ClassId::new(raw)) else {
        drop(cm);
        return lambda_proxy_bytecodes(shared, crate::classloading::ClassId::new(raw), method_id);
    };
    let Some(method) = class
        .methods
        .iter()
        .find(|m| jdwp_method_id(&m.name, &m.descriptor) == method_id)
    else {
        return CommandResult::error(ERR_INVALID_METHODID);
    };
    if method.is_native() {
        return CommandResult::error(commands::ERR_NATIVE_METHOD);
    }
    let code: &[u8] = method.code().map_or(&[][..], |c| &c.code[..]);
    let mut pw = protocol::PayloadWriter::new();
    pw.put_u32_be(u32::try_from(code.len()).unwrap_or(0));
    pw.put_bytes(code);
    CommandResult::ok(pw.into_bytes())
}

/// [`method_bytecodes`] of a lambda proxy class's method (the i54-L1
/// retirement): the code `InnerClassLambdaMetafactory` spins for it, as the C
/// JVMTI table's `GetBytecodes` answers it
/// (`runtime::lambda_proxy::lambda_proxy_code`); `INVALID_CLASS` for an id
/// naming no proxy, `INVALID_METHODID` for one naming none of its methods.
pub(in crate::debug) fn lambda_proxy_bytecodes(
    shared: &crate::vm::SharedVm,
    id: crate::classloading::ClassId,
    method_id: u64,
) -> commands::CommandResult {
    use commands::{
        CommandResult, ERR_ABSENT_INFORMATION, ERR_INVALID_CLASS, ERR_INVALID_METHODID,
    };
    let Some(facts) = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let Some(index) = facts
        .methods
        .iter()
        .position(|m| jdwp_method_id(&m.name, &m.descriptor) == method_id)
    else {
        return CommandResult::error(ERR_INVALID_METHODID);
    };
    let Some(code) = crate::runtime::lambda_proxy::lambda_proxy_code(shared, id, index) else {
        return CommandResult::error(ERR_ABSENT_INFORMATION);
    };
    let mut pw = protocol::PayloadWriter::new();
    pw.put_u32_be(u32::try_from(code.len()).unwrap_or(0));
    pw.put_bytes(&code);
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.SourceDebugExtension` (2/12, interpreter round i1 wave 43,
/// lane L1; stage 1 of
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// the class's `SourceDebugExtension` attribute (JVMS 4.7.11, a JSR-45 SMAP
/// for Kotlin inline functions, JSP and the like), as JVMTI
/// `GetSourceDebugExtension` answers it; `ABSENT_INFORMATION` for a class
/// without one and for an array class; `INVALID_CLASS` for an id naming no
/// class.
///
/// `Class` does not keep the attribute (adding a field means every one of
/// its ~70 struct literals), so the class file is read again, only when a
/// debugger asks, from where a retransformation reads it
/// (`vm/src/runtime/instrument.rs`, `try_original_class_bytes`): the class
/// manager's class-bytes cache, filled by every define, else the built-in
/// delegation chain's copy when it is byte for byte the file the class was
/// defined from (`class_bytes_match_base`). A class whose file is in
/// neither (a user loader's class the FIFO evicted) answers
/// `ABSENT_INFORMATION`, which JDI reads as "no SMAP": the class keeps the
/// base stratum, as before this command was served. For a class a load-time
/// transformer changed, the cache holds the file before the transform (its
/// retransformation base); transformers do not change this attribute in
/// practice.
///
/// JDI asks it of every class whose lines it maps once the capability is
/// true (`ReferenceTypeImpl.sourceDebugExtensionInfo`, cached per class), so
/// a class without the attribute must answer `ABSENT_INFORMATION`, never an
/// error JDI would throw.
pub(in crate::debug) fn source_debug_extension(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{CommandResult, ERR_ABSENT_INFORMATION, ERR_INTERNAL, ERR_INVALID_CLASS};
    let Ok(class_id) = protocol::PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(class_id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let bytes: Option<Vec<u8>> = {
        let cm = shared.classes.class_manager.read();
        let id = crate::classloading::ClassId::new(raw);
        let Some(class) = cm.class_store.get(id) else {
            // A lambda proxy class has none (the i54-L1 retirement).
            if shared.classes.is_lambda_proxy_class(id) {
                return CommandResult::error(ERR_ABSENT_INFORMATION);
            }
            return CommandResult::error(ERR_INVALID_CLASS);
        };
        if class.name.starts_with('[') {
            return CommandResult::error(ERR_ABSENT_INFORMATION);
        }
        defined_class_file(&cm, id, &class.name)
    };
    match bytes.as_deref().and_then(source_debug_extension_of) {
        Some(smap) => {
            // A JDWP string is its length and its (modified) UTF-8 bytes,
            // which is what the attribute holds.
            let mut pw = protocol::PayloadWriter::new();
            pw.put_u32_be(u32::try_from(smap.len()).unwrap_or(0));
            pw.put_bytes(&smap);
            CommandResult::ok(pw.into_bytes())
        }
        None => CommandResult::error(ERR_ABSENT_INFORMATION),
    }
}

/// The class file class `id` (named `name`) was defined from, for the
/// debugger's reads of attributes `Class` does not keep
/// ([`source_debug_extension`]): the class-bytes cache, else the delegation
/// chain's copy when it is byte for byte the defined file. `None` when
/// neither has it.
pub(crate) fn defined_class_file(
    cm: &crate::classloading::ClassManager,
    id: crate::classloading::ClassId,
    name: &str,
) -> Option<Vec<u8>> {
    if let Some(cached) = cm.defined_class_bytes(id) {
        return Some(cached.to_vec());
    }
    match cm.find_class_bytes_for_transform(name) {
        Ok((found, _)) if cm.class_bytes_match_base(id, &found) == Some(true) => Some(found),
        _ => None,
    }
}

/// The source file names the strata of `class`'s `SourceDebugExtension`
/// list (interpreter round i1 wave 43, lane L1): what a `ClassPrepare`
/// request's `SourceNameMatch` matches besides the `SourceFile`, as HotSpot
/// matches them (`tools/probes/interp/L1/L1W43JdiSourceDebugExtension.java`:
/// a filter on `Inline.kt`, a name only the SMAP lists, reports the class).
/// Empty without the attribute. Read only while such a filter is in force
/// (`EventManager::has_source_name_filters`).
pub(in crate::debug) fn smap_source_names(
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
) -> Vec<String> {
    if class.name.starts_with('[') {
        return Vec::new();
    }
    defined_class_file(cm, class.id, &class.name)
        .as_deref()
        .and_then(source_debug_extension_of)
        .map_or_else(Vec::new, |smap| smap_file_names(&smap))
}

/// The file names of every file section (`*F`) of JSR-45 SMAP `smap`: each
/// entry is `<id> <name>`, or `+ <id> <name>` followed by a line with the
/// file's path. Each name once, in order.
pub(in crate::debug) fn smap_file_names(smap: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(smap);
    let mut names: Vec<String> = Vec::new();
    let mut in_files = false;
    let mut path_follows = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('*') {
            in_files = line.trim_end() == "*F";
            path_follows = false;
            continue;
        }
        if !in_files {
            continue;
        }
        if path_follows {
            path_follows = false;
            continue;
        }
        let (plus, entry) = match line.strip_prefix('+') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line.trim_start()),
        };
        path_follows = plus;
        if let Some((_, name)) = entry.split_once(' ') {
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// The bytes of class file `class_file`'s `SourceDebugExtension` attribute
/// ([`source_debug_extension`]); `None` without one or for a file that does
/// not parse. The reader keeps the attribute as raw bytes (it has no decoded
/// form), so it is read from its range without decoding.
pub(crate) fn source_debug_extension_of(class_file: &[u8]) -> Option<Vec<u8>> {
    use cratonvm_reader::attribute::{Attribute, LazyAttribute};
    let parsed = cratonvm_reader::class_reader::read_class(class_file).ok()?;
    let lazy = parsed
        .attributes
        .iter()
        .find(|a| a.name() == "SourceDebugExtension")?;
    match lazy {
        LazyAttribute::Raw(raw) => raw.source.get(raw.range.clone()).map(<[u8]>::to_vec),
        LazyAttribute::Decoded(Attribute::Unknown { data, .. }) => Some(data.to_vec()),
        LazyAttribute::Decoded(_) => None,
    }
}

/// The constant pool of class file `bytes`: its `constant_pool_count` and
/// its entries' bytes, in class-file order. `None` for bytes that are not a
/// well-formed pool. What JVMTI `GetConstantPool` and JDWP
/// `ReferenceType.ConstantPool` answer (HotSpot reconstitutes the pool of
/// the class file the class was defined from; MEASURED for a plain class,
/// `jvmti::native_env::agent_functions::get_constant_pool`).
pub(crate) fn class_file_constant_pool(bytes: &[u8]) -> Option<(u16, &[u8])> {
    let count = u16::from_be_bytes([*bytes.get(8)?, *bytes.get(9)?]);
    let start = 10;
    let mut pos = start;
    let mut index = 1;
    while index < count {
        let tag = *bytes.get(pos)?;
        let width = match tag {
            1 => {
                3 + usize::from(u16::from_be_bytes([
                    *bytes.get(pos + 1)?,
                    *bytes.get(pos + 2)?,
                ]))
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 5,
            5 | 6 => {
                index += 1;
                9
            }
            7 | 8 | 16 | 19 | 20 => 3,
            15 => 4,
            _ => return None,
        };
        pos += width;
        index += 1;
    }
    Some((count, bytes.get(start..pos)?))
}

/// `ReferenceType.ConstantPool` (2/18, interpreter round i5 wave 103, lane
/// L1): `count` (the class file's `constant_pool_count`) and the pool's
/// bytes, as HotSpot's back end answers it from JVMTI `GetConstantPool`;
/// `ABSENT_INFORMATION` for an array class and where this VM no longer has
/// the class file; a lambda proxy class's pool as its spun class holds it.
/// `canGetConstantPool` answers true since the same wave.
pub(in crate::debug) fn constant_pool(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{CommandResult, ERR_ABSENT_INFORMATION, ERR_INTERNAL, ERR_INVALID_CLASS};
    let Ok(class_id) = protocol::PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(class_id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let id = crate::classloading::ClassId::new(raw);
    let stored: Option<Option<(u16, Vec<u8>)>> = {
        let cm = shared.classes.class_manager.read();
        cm.class_store.get(id).map(|class| {
            if class.name.starts_with('[') {
                return None;
            }
            defined_class_file(&cm, id, &class.name).and_then(|file| {
                class_file_constant_pool(&file).map(|(count, pool)| (count, pool.to_vec()))
            })
        })
    };
    let pool = match stored {
        Some(pool) => pool,
        // A lambda proxy class (never in the store): its spun class's pool.
        None if shared.classes.is_lambda_proxy_class(id) => {
            crate::runtime::lambda_proxy::lambda_proxy_spun_class_of(shared, id)
                .map(|spun| spun.constant_pool())
        }
        None => return CommandResult::error(ERR_INVALID_CLASS),
    };
    let Some((count, bytes)) = pool else {
        return CommandResult::error(ERR_ABSENT_INFORMATION);
    };
    let mut pw = protocol::PayloadWriter::new();
    pw.put_u32_be(u32::from(count));
    pw.put_u32_be(u32::try_from(bytes.len()).unwrap_or(0));
    pw.put_bytes(&bytes);
    CommandResult::ok(pw.into_bytes())
}

/// The JDWP error HotSpot's back end answers for the JVMTI error `jvmti` of
/// a refused `RedefineClasses` (`util.c` `map2jdwpError`): the redefinition
/// errors 60-72 keep their numbers (JDWP `INVALID_CLASS_FORMAT` ..
/// `CLASS_ATTRIBUTE_CHANGE_NOT_IMPLEMENTED`), `UNSUPPORTED_OPERATION` (73)
/// and `UNMODIFIABLE_CLASS` (79) are `NOT_IMPLEMENTED`, anything else
/// `INTERNAL` (interpreter round i5 wave 104, lane L1; derived from the
/// back end's source, not measured).
pub(in crate::debug) fn jdwp_redefinition_error(jvmti: i32) -> u16 {
    match jvmti {
        // Cast: in 60..=72.
        60..=72 => jvmti as u16,
        73 | 79 => commands::ERR_NOT_IMPLEMENTED,
        _ => commands::ERR_INTERNAL,
    }
}

/// `VirtualMachine.RedefineClasses` (1/18, interpreter round i5 wave 104,
/// lane L1; `canRedefineClasses`, true since the same wave): `classes`, then
/// per class its `refType` and `classfile` (`int` length, bytes). As
/// HotSpot's back end (`VirtualMachineImpl.c` `redefineClasses`): no class is
/// success; a class id naming nothing is `INVALID_OBJECT`, naming no class
/// `INVALID_CLASS`; an empty class file `INVALID_CLASS_FORMAT`; then JVMTI
/// `RedefineClasses` of all of them at once, here this VM's (the C JVMTI
/// table's path, `instrument::redefine_classes_for_jvmti`: the class files
/// through the `ClassFileLoadHook` chain, then checked and installed
/// together), whose refusal answers its JDWP error
/// ([`jdwp_redefinition_error`]: a field added is
/// `SCHEMA_CHANGE_NOT_IMPLEMENTED`, a method added
/// `ADD_METHOD_NOT_IMPLEMENTED`; `canAddMethod` and
/// `canUnrestrictedlyRedefineClasses` stay false, as on HotSpot). Run on a
/// registered mutator (a parked thread, else the heap service,
/// [`run_on_parked_thread`]), as making and installing classes allocates;
/// HotSpot runs it on its JDWP thread with no thread suspended. A redefined
/// class's members, line and variable tables are described again
/// ([`add_class_metadata`]), as JDI re-reads them after
/// `noticeRedefineClass`.
pub(in crate::debug) fn redefine_classes(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{
        CommandResult, ERR_INTERNAL, ERR_INVALID_CLASS, ERR_INVALID_OBJECT, ERR_NOT_IMPLEMENTED,
    };
    /// JDWP `INVALID_CLASS_FORMAT`.
    const ERR_INVALID_CLASS_FORMAT: u16 = 60;
    let mut r = protocol::PayloadReader::new(data);
    let Ok(count) = r.read_u32_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    // Cast: a JDWP int; a negative count names no class.
    let count = count as i32;
    if count <= 0 {
        return CommandResult::ok(Vec::new());
    }
    let mut classes: Vec<(crate::classloading::ClassId, Vec<u8>)> = Vec::new();
    for _ in 0..count {
        let Ok(wire) = r.read_u64_be() else {
            return CommandResult::error(ERR_INTERNAL);
        };
        let class = ids::class_from_wire(wire);
        let stored = u32::try_from(class).ok().map(crate::classloading::ClassId::new);
        let in_store = stored.is_some_and(|id| {
            shared
                .classes
                .class_manager
                .read()
                .class_store
                .get(id)
                .is_some()
        });
        let lambda = stored.is_some_and(|id| shared.classes.is_lambda_proxy_class(id));
        if !in_store && !lambda {
            // An id of another kind (a thread, an object) is `INVALID_CLASS`;
            // one naming nothing `INVALID_OBJECT` (`inStream_readClassRef`).
            let names_something = id_names_something(shared, wire);
            return CommandResult::error(if names_something {
                ERR_INVALID_CLASS
            } else {
                ERR_INVALID_OBJECT
            });
        }
        let Ok(length) = r.read_u32_be() else {
            return CommandResult::error(ERR_INTERNAL);
        };
        // Cast: a JDWP int.
        let length = length as i32;
        if length <= 0 {
            return CommandResult::error(ERR_INVALID_CLASS_FORMAT);
        }
        let Ok(bytes) = r.read_bytes(length as usize) else {
            return CommandResult::error(ERR_INTERNAL);
        };
        let Some(id) = stored else {
            return CommandResult::error(ERR_INTERNAL);
        };
        classes.push((id, bytes.to_vec()));
    }
    let ids: Vec<crate::classloading::ClassId> = classes.iter().map(|(id, _)| *id).collect();
    let outcome = run_on_parked_thread(shared, None, move |shared, thread| {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        if classes
            .iter()
            .any(|&(id, _)| !crate::runtime::instrument::class_id_is_modifiable(&ctx, id))
        {
            return Err(ERR_NOT_IMPLEMENTED);
        }
        match crate::runtime::instrument::redefine_classes_for_jvmti(&mut ctx, &classes) {
            None => Ok(()),
            Some(refusal) => {
                tracing::warn!("JDWP RedefineClasses: {}", refusal.detail);
                Err(jdwp_redefinition_error(
                    crate::jvmti::native_env::redefinition_refusal_error(&refusal),
                ))
            }
        }
    });
    match outcome {
        Ok(Ok(())) => {
            let cm = shared.classes.class_manager.read();
            let mut ds = shared.debug.debug_state.lock();
            for id in ids {
                if let Some(class) = cm.class_store.get(id) {
                    let _ = add_class_metadata(&mut ds, class);
                }
                // As the back end's `eventHandler_freeClassBreakpoints`: the
                // class's breakpoints go (JDI deletes its requests too). The
                // server republishes the gates after the command.
                let _ = ds.events.clear_class_breakpoints(u64::from(id.as_u32()));
            }
            CommandResult::ok(Vec::new())
        }
        Ok(Err(code)) => CommandResult::error(code),
        Err(_) => CommandResult::error(ERR_INTERNAL),
    }
}

/// `VirtualMachine.ClassPaths` (1/13, interpreter round i1 wave 24): as
/// HotSpot's back end answers it (`classPaths`) — `baseDir` is `user.dir`,
/// the class paths are `java.class.path` split at `path.separator`, and the
/// boot class path is empty (JDK 9+ has none). JDI's
/// `PathSearchingVirtualMachine.classPath()` (jdb `classpath`) sends it
/// without a capability check; it answered `NOT_IMPLEMENTED`.
pub(in crate::debug) fn class_paths(shared: &crate::vm::SharedVm) -> commands::CommandResult {
    let (base, class_path, separator) = {
        let props = shared.system_properties.read();
        let get = |key: &str| props.get(key).cloned().unwrap_or_default();
        (
            get("user.dir"),
            get("java.class.path"),
            get("path.separator"),
        )
    };
    let separator = separator
        .chars()
        .next()
        .unwrap_or(if cfg!(windows) { ';' } else { ':' });
    let entries: Vec<&str> = if class_path.is_empty() {
        Vec::new()
    } else {
        class_path.split(separator).collect()
    };
    let mut pw = protocol::PayloadWriter::new();
    pw.put_string(&base);
    pw.put_u32_be(u32::try_from(entries.len()).unwrap_or(u32::MAX));
    for entry in entries {
        pw.put_string(entry);
    }
    pw.put_u32_be(0); // bootclasspaths
    commands::CommandResult::ok(pw.into_bytes())
}

/// Before a command that reads a thread's frames (wave 12): a thread the
/// debugger suspended while it was blocked in a native region has published
/// none (a blocked thread publishes on entry only when it is already
/// suspended, `interpreter::publish_blocked_frames`), so read them now through
/// its inspection window, on a registered mutator (a parked thread or the
/// heap service, [`run_on_parked_thread`]). Nothing happens for a thread that
/// is not suspended (the command refuses it), already has a snapshot, or is
/// not blocked in such a region (a running thread publishes at its suspend
/// point).
pub(in crate::debug) fn publish_blocked_frames_for(shared: &crate::vm::SharedVm, data: &[u8]) {
    let Some(tid) = data
        .get(..8)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_be_bytes)
    else {
        return;
    };
    let wanted = {
        let ds = shared.debug.debug_state.lock();
        // Wave 28: a listing from before a resume is read again.
        ds.is_thread_suspended(tid) && !ds.has_current_frames(tid)
    };
    if !wanted {
        return;
    }
    if !crate::runtime::interpreter::blocked_frames_readable(shared, tid) {
        // Wave 48 (lane L1): an unmounted virtual thread (a continuation that
        // yielded in a sleep, a park or a keyed wait) has no window; its
        // frames sit in the virtual-thread manager.
        if shared.threads.virtual_thread_manager.is_virtual(tid)
            && run_on_parked_thread(shared, None, move |shared, _thread| {
                crate::runtime::interpreter::publish_frames_of_unmounted_virtual_thread(shared, tid)
            })
            .unwrap_or(false)
        {
            return;
        }
        // Wave 56 (lane L6): a thread suspended while it runs a native
        // outside a blocking region is read through its native frames
        // window, now or once it opens it on its way into the native.
        if publish_native_frames_for(shared, tid) {
            return;
        }
        await_suspend_point_frames(shared, tid);
        if publish_native_frames_for(shared, tid) {
            return;
        }
        // It blocked on the way, suspended (a blocking region's entry
        // publishes then only for a suspension it sees first).
        if !await_inspection_window(shared, tid) {
            return;
        }
    }
    let _ = run_on_parked_thread(shared, None, move |shared, _thread| {
        crate::runtime::interpreter::publish_frames_of_blocked_thread(shared, tid)
    });
}

/// Read suspended thread `tid`'s frames through its native frames window
/// on a registered mutator, when that window is open (interpreter round i2
/// wave 56, lane L6, [`DebuggerGates::native_frames_window`]); answers
/// whether a listing was published.
///
/// The i53 retirement: while a stop-the-world pause is requested, which waits
/// for the thread in its native and holds every registered mutator until it
/// returns, this thread reads the window itself first
/// (`interpreter::publish_frames_of_native_thread_in_stop`).
pub(in crate::debug) fn publish_native_frames_for(shared: &crate::vm::SharedVm, tid: u64) -> bool {
    if !crate::runtime::interpreter::native_frames_readable(shared, tid) {
        return false;
    }
    if shared.mem.gc_barrier.stop_requested()
        && crate::runtime::interpreter::publish_frames_of_native_thread_in_stop(shared, tid)
    {
        return true;
    }
    run_on_parked_thread(shared, None, move |shared, _thread| {
        crate::runtime::interpreter::publish_frames_of_native_thread(shared, tid)
    })
    .unwrap_or(false)
}

/// Whether suspended thread `tid`, which has no listing of this suspension,
/// can be read through its inspection window now; waits a bounded while
/// ([`SUSPEND_POINT_WAIT`]) for a thread that is on its way INTO a blocking
/// wait to open it (interpreter round i1 wave 50, lane L1).
///
/// `Object.wait` (and a blocking region) stores the thread's Java state --
/// what `ThreadReference.Status` answers (`WAIT`, `SLEEPING`) -- and marks it
/// blocked BEFORE it opens its window (`vm_exec` `monitor_wait_with_state`:
/// state, deposit, `enter_blocked`, then `open_blocked_inspection`). A
/// debugger that waited for `WAIT`, suspended the thread and asked for its
/// frames inside that gap found the window closed and the thread not running
/// (no suspend point to wait for), and the reply listed no frames at all: JDI
/// `frame(0)` threw. Seen once in 20 host runs of `L1W43JdiForceEarlyReturn`
/// under `--compatible` (its `== refused` section suspends a thread as soon as
/// it reads `WAIT`); HotSpot's `Frames` of a suspended waiting thread always
/// lists its `Object.wait` frame. `CRATONVM_FRAME_TRACE=1` prints
/// `[AWAIT_WINDOW] tid=<n> readable=<bool> waited_ms=<n>` when it waited.
pub(in crate::debug) fn await_inspection_window(shared: &crate::vm::SharedVm, tid: u64) -> bool {
    let start = std::time::Instant::now();
    let mut waited = false;
    let readable = loop {
        {
            let ds = shared.debug.debug_state.lock();
            if !ds.is_thread_suspended(tid) || ds.has_current_frames(tid) {
                break false;
            }
        }
        if crate::runtime::interpreter::blocked_frames_readable(shared, tid) {
            break true;
        }
        // Only a thread blocked, or about to block, opens a window: one that
        // runs Java parks at its suspend point instead (and was waited for by
        // `await_suspend_point_frames`), a dead one never opens one.
        let coming = shared
            .threads
            .thread_registry
            .is_blocked(crate::threading::ThreadId(tid))
            || matches!(
                commands::thread_status_of(&shared.debug.debug_state.lock(), tid),
                Some(
                    commands::THREAD_STATUS_WAIT
                        | commands::THREAD_STATUS_SLEEPING
                        | commands::THREAD_STATUS_MONITOR
                )
            );
        if !coming || start.elapsed() >= SUSPEND_POINT_WAIT {
            break false;
        }
        waited = true;
        await_debug_state_poll(shared);
    };
    if waited && crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[AWAIT_WINDOW] tid={tid} readable={readable} waited_ms={}",
            start.elapsed().as_millis()
        );
    }
    readable
}

/// Wait, a bounded while, for suspended thread `tid` -- running Java when it
/// was suspended, so neither blocked nor parked yet -- to reach its suspend
/// point and publish its frames (interpreter round i1 wave 48, lane L1).
/// `VirtualMachine.Suspend` / `ThreadReference.Suspend` only count the
/// suspension here; the thread parks at its next suspend point. HotSpot's
/// suspension completes (a handshake) before its reply, so a `Frames` sent
/// right after it lists the thread's frames; this VM's answered no frames
/// at all for the thread still on its way (JDI: `frame(0)` threw
/// `IndexOutOfBoundsException`; seen with `L1W48JdiCompatibleBlockingBytecode`,
/// whose sleeper is suspended between two sleeps). A thread blocked in a
/// region (its window closed) or one that never reaches a suspend point (a
/// native that does not block through a region) is waited for at most
/// [`SUSPEND_POINT_WAIT`]. `CRATONVM_FRAME_TRACE=1` prints
/// `[AWAIT_SUSPEND_POINT] tid=<n> published=<bool> waited_ms=<n>`.
pub(in crate::debug) fn await_suspend_point_frames(shared: &crate::vm::SharedVm, tid: u64) {
    let registry = &shared.threads.thread_registry;
    if registry.is_blocked(crate::threading::ThreadId(tid)) {
        return;
    }
    let start = std::time::Instant::now();
    let mut nudges = 0u32;
    let published = loop {
        {
            let ds = shared.debug.debug_state.lock();
            // Only a RUNNING thread is on its way to a suspend point (a dead
            // one never gets there; a sleeping or waiting one publishes
            // through its window or its deposit).
            if !ds.is_thread_suspended(tid)
                || commands::thread_status_of(&ds, tid) != Some(commands::THREAD_STATUS_RUNNING)
            {
                return;
            }
            if ds.has_current_frames(tid) {
                break true;
            }
        }
        if start.elapsed() >= SUSPEND_POINT_WAIT
            || registry.is_blocked(crate::threading::ThreadId(tid))
            // Wave 56 (lane L6): in a native, readable through its window.
            || crate::runtime::interpreter::native_frames_readable(shared, tid)
        {
            break false;
        }
        // Wave 53 (lane L6): as the suspension's own wait does.
        if suspend_nudge_due(start.elapsed(), nudges) {
            nudges += 1;
            crate::runtime::interpreter::request_compiled_loop_exits_again(shared);
        }
        await_debug_state_poll(shared);
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[AWAIT_SUSPEND_POINT] tid={tid} published={published} waited_ms={} nudges={nudges}",
            start.elapsed().as_millis()
        );
    }
}

/// Thread `tid` published its listing (under the debug-state lock `ds`):
/// if the settle wait in progress counts it
/// ([`DebuggerGates::settle_pending_counter`], interpreter round i6 wave
/// 117, lane L1), hand it to the wait's next round. One empty-set test
/// otherwise (the set is empty outside a wait and with the switch off).
#[inline]
pub(crate) fn note_settle_arrival(ds: &mut DebugState, tid: u64) {
    if !ds.settle_wanted.is_empty() && ds.settle_wanted.remove(&tid) {
        ds.settle_arrived.push(tid);
    }
}

/// [`await_suspension_settled`]'s state with
/// [`DebuggerGates::settle_pending_counter`] (interpreter round i6 wave 117,
/// lane L1): the threads still on their way, the laggards among those its
/// one walk found, and when the pending threads were last re-walked.
pub(in crate::debug) struct CountedSettle {
    pub(in crate::debug) pending: Vec<u64>,
    pub(in crate::debug) laggards: Vec<u64>,
    pub(in crate::debug) walked_at: std::time::Instant,
}

/// Is suspended thread `tid` still on its way to a safe place, as far as the
/// debug state tells (interpreter round i1 wave 49, lane L1;
/// [`await_suspension_settled`])? Not a dead one, and not one whose listing
/// of this suspension stands (it parked at its suspend point, or an event
/// published it). Whether it is blocked is the registry's answer, read fresh
/// by the caller.
///
/// Interpreter round i2 wave 53, lane L6: no longer "only a thread whose
/// status reads `RUNNING`". That status is the thread table's, refreshed
/// before the commands that name a thread ([`refreshes_thread_names`]) --
/// not before `VirtualMachine.Suspend` nor a `SUSPEND_ALL` event set, and
/// never during the wait -- so a thread that read `SLEEPING` or `WAIT` at the
/// last refresh and has woken since was taken for stopped, and ran Java after
/// the reply. HotSpot's handshake stops it.
pub(in crate::debug) fn suspension_on_its_way(ds: &DebugState, tid: u64) -> bool {
    ds.is_thread_suspended(tid)
        && commands::thread_status_of(ds, tid)
            .is_some_and(|status| status != commands::THREAD_STATUS_ZOMBIE)
        && !ds.has_current_frames(tid)
}

/// Wait, bounded, for the threads a JDWP suspension just covered -- `only`,
/// or every thread the server knows -- to be stopped where a debugger may
/// read them, before the suspension's reply or event set goes out
/// (interpreter round i1 wave 49, lane L1; stage 1 of
/// `docs/feature-designs/jvmti-delivery-threading.md`,
/// `CRATONVM_DEBUG_SUSPEND_HANDSHAKE`). HotSpot's suspension is a handshake:
/// when `VirtualMachine.Suspend` answers, every thread is stopped. Here the
/// commands only count it and each running thread parks at its next suspend
/// point, so a `Frames`, `Status`, `OwnedMonitors` or a field read right
/// after the reply raced the thread's arrival. A thread counts as stopped
/// once it published its listing at its suspend point, is blocked in a
/// region, is an unmounted virtual thread, or is dead
/// ([`suspension_on_its_way`]). A thread still on its way after
/// [`SUSPEND_POINT_WAIT`] (all of them together) becomes a laggard
/// (`DebugState::suspend_laggards`): later waits look at it but do not wait
/// for it, until one sees it stopped. Runs on the JDWP server thread after
/// the gates are republished (so the threads' suspend points are armed),
/// holding no lock between its polls. `CRATONVM_FRAME_TRACE=1` prints
/// `[SUSPEND_SETTLED] waited=<n> laggards=<n> waited_us=<n>` whenever it
/// found a thread on its way.
#[cold]
#[inline(never)]
pub(in crate::debug) fn await_suspension_settled(shared: &crate::vm::SharedVm, only: Option<u64>) {
    if !shared.debug.debugger_gates.suspend_handshake() {
        return;
    }
    // Wave 53 (lane L6): a suspension of every thread covers the threads
    // started since the table was last refreshed too (neither
    // `VirtualMachine.Suspend` nor a `SUSPEND_ALL` event set refreshes it,
    // `refreshes_thread_names`): they were not waited for at all.
    if only.is_none() {
        populate_thread_metadata(shared);
    }
    let registry = &shared.threads.thread_registry;
    let manager = &shared.threads.virtual_thread_manager;
    let start = std::time::Instant::now();
    let mut checked: Vec<u64> = Vec::new();
    let mut first: Option<usize> = None;
    let mut polls = 0u32;
    let mut nudges = 0u32;
    let native_ack = shared.debug.debugger_gates.suspend_native_ack();
    // Interpreter round i2 wave 56, lane L6
    // (`CRATONVM_DEBUG_SUSPEND_SETTLE_LONG`): HotSpot's handshake has no time
    // bound, and on an overloaded host a running thread can be descheduled
    // for longer than 250 ms on its way to its suspend point; the reply then
    // went out with it running (`L1W49JdiSuspendCompletesBeforeReply`, 3 of
    // 80 at load ~45). The long bound only matters for such a thread: one
    // that is blocked, in an acknowledged native, deposited or dead is not
    // waited for at all.
    let settle_bound = if shared.debug.debugger_gates.suspend_settle_long() {
        SUSPEND_SETTLE_LONG_WAIT
    } else {
        SUSPEND_POINT_WAIT
    };
    let mut acknowledged: Vec<u64> = Vec::new();
    // Interpreter round i2 wave 64, lane L6
    // (`CRATONVM_DEBUG_SETTLE_ARRIVAL_COUNT`): the last walk's answer, the
    // arrival count it was stamped with, and when it ran. A round with no
    // arrival since reuses it; a walk looks only at the threads the last
    // walk found on their way (a thread seen stopped stays stopped for this
    // suspension: only a resume -- the server's own next command, or a C
    // JVMTI resume, which moves the count -- unlists it).
    let arrival_count = shared.debug.debugger_gates.settle_arrival_count();
    let mut last_walk: Option<SettleWalk> = None;
    let mut walks = 0u32;
    // Interpreter round i6 wave 117, lane L1
    // (`CRATONVM_DEBUG_SETTLE_PENDING_COUNTER`): one walk, then the arrivals.
    let pending_counter = shared.debug.debugger_gates.settle_pending_counter();
    let mut counted: Option<CountedSettle> = None;
    let pending = loop {
        acknowledged.clear();
        let arrivals_now = shared.debug.debugger_gates.arrivals();
        let reuse = arrival_count
            && last_walk
                .as_ref()
                .is_some_and(|walk| !walk.due(arrivals_now));
        let (mut pending, laggards): (Vec<u64>, Vec<u64>) = if pending_counter {
            let mut ds = shared.debug.debug_state.lock();
            match counted.as_mut() {
                None => {
                    checked = match only {
                        Some(tid) => vec![tid],
                        None => commands::known_thread_ids(&ds),
                    };
                    let pending: Vec<u64> = checked
                        .iter()
                        .copied()
                        .filter(|&tid| suspension_on_its_way(&ds, tid))
                        .collect();
                    let laggards: Vec<u64> = pending
                        .iter()
                        .copied()
                        .filter(|tid| ds.suspend_laggards.contains(tid))
                        .collect();
                    // Registered in the walk's own critical section: a
                    // listing published after it is an arrival.
                    ds.settle_wanted = pending.iter().copied().collect();
                    ds.settle_arrived.clear();
                    walks += 1;
                    counted = Some(CountedSettle {
                        pending: pending.clone(),
                        laggards: laggards.clone(),
                        walked_at: std::time::Instant::now(),
                    });
                    (pending, laggards)
                }
                Some(c) => {
                    if !ds.settle_arrived.is_empty() {
                        let arrived: HashSet<u64> = ds.settle_arrived.drain(..).collect();
                        c.pending.retain(|tid| !arrived.contains(tid));
                    }
                    // The backstop: a change no listing publication
                    // reports (a death, a C JVMTI resume).
                    if c.walked_at.elapsed() >= SETTLE_REWALK_BACKSTOP {
                        c.pending.retain(|&tid| suspension_on_its_way(&ds, tid));
                        c.walked_at = std::time::Instant::now();
                        walks += 1;
                    }
                    (c.pending.clone(), c.laggards.clone())
                }
            }
        } else if reuse {
            let walk = last_walk.as_ref().expect("reuse needs a walk");
            (walk.pending.clone(), walk.laggards.clone())
        } else {
            let ds = shared.debug.debug_state.lock();
            if checked.is_empty() {
                checked = match only {
                    Some(tid) => vec![tid],
                    None => commands::known_thread_ids(&ds),
                };
            }
            let source: &[u64] = match (&last_walk, arrival_count) {
                (Some(walk), true) => &walk.pending,
                _ => &checked,
            };
            let pending: Vec<u64> = source
                .iter()
                .copied()
                .filter(|&tid| suspension_on_its_way(&ds, tid))
                .collect();
            let laggards: Vec<u64> = pending
                .iter()
                .copied()
                .filter(|tid| ds.suspend_laggards.contains(tid))
                .collect();
            walks += 1;
            if arrival_count {
                last_walk = Some(SettleWalk {
                    pending: pending.clone(),
                    laggards: laggards.clone(),
                    arrivals: arrivals_now,
                    at: std::time::Instant::now(),
                });
            }
            (pending, laggards)
        };
        // Outside the debug-state lock: the registry's and the manager's
        // own locks.
        pending.retain(|&tid| {
            let id = crate::threading::ThreadId(tid);
            if !registry.is_alive(id) || registry.is_blocked(id) || manager.is_deposited(tid) {
                return false;
            }
            // Wave 53 (lane L6): in a native, outside a region, with no Java
            // above it: it parks at the native's return.
            if native_ack && in_native_acknowledged(registry, id) {
                acknowledged.push(tid);
                return false;
            }
            true
        });
        // Wave 117 (`CRATONVM_DEBUG_SETTLE_PENDING_COUNTER`): a thread seen
        // stopped stays stopped for this suspension, as the arrival-count
        // walk treats it.
        if let Some(c) = counted.as_mut() {
            if c.pending.len() != pending.len() {
                let still: HashSet<u64> = pending.iter().copied().collect();
                c.pending.retain(|tid| still.contains(tid));
            }
        }
        first.get_or_insert(pending.len());
        let waiting = pending.iter().any(|tid| !laggards.contains(tid));
        if !waiting || start.elapsed() >= settle_bound {
            break pending;
        }
        // Wave 53 (lane L6): a thread still running compiled code is sent to
        // its poll again ([`suspend_nudge_due`]).
        if suspend_nudge_due(start.elapsed(), nudges) {
            nudges += 1;
            crate::runtime::interpreter::request_compiled_loop_exits_again(shared);
        }
        // A thread running bytecode arrives within microseconds: yield
        // first, sleep (a coarse quantum on some hosts) only after.
        polls += 1;
        if polls < 64 {
            std::thread::yield_now();
        } else {
            await_debug_state_poll(shared);
        }
    };
    {
        let mut ds = shared.debug.debug_state.lock();
        // Wave 117: the wait is over; no listing is an arrival any more.
        if pending_counter {
            ds.settle_wanted.clear();
            ds.settle_arrived.clear();
        }
        // A laggard seen stopped is waited for again next time.
        ds.suspend_laggards
            .retain(|tid| !checked.contains(tid) || pending.contains(tid));
        ds.suspend_laggards.extend(pending.iter().copied());
    }
    // Wave 53 (lane L6): a laggard still running compiled code is asked once
    // more, so it parks soon after the reply instead of running suspended
    // until something else pauses the VM.
    if !pending.is_empty() {
        nudges += 1;
        crate::runtime::interpreter::request_compiled_loop_exits_again(shared);
    }
    if crate::runtime::env_cache::frame_trace() && first.is_some_and(|n| n > 0) {
        // Wave 58 (lane L6): each laggard's state as the wait gave up on it
        // -- parked at a suspend point, its listing's generation against
        // its current one.
        let states: Vec<String> = {
            let ds = shared.debug.debug_state.lock();
            pending
                .iter()
                .map(|&tid| {
                    let listed = ds
                        .thread_frames
                        .get(&tid)
                        .map(|f| (f.len(), f.first().map(|e| e.frame_id >> 32)));
                    format!(
                        "{tid}:parked={} listed={listed:?} gen={}",
                        ds.is_parked(tid),
                        ds.frame_generation(tid)
                    )
                })
                .collect()
        };
        eprintln!(
            "[SUSPEND_SETTLED] waited={} laggards={} waited_us={} nudges={nudges} in_native={} \
             walks={walks} polls={polls} laggard_tids={pending:?} laggard_states={states:?}",
            first.unwrap_or(0),
            pending.len(),
            start.elapsed().as_micros(),
            acknowledged.len()
        );
    }
}

/// Has thread `id` acknowledged a JDWP suspension by being in a native
/// outside a blocking region, with no Java above it
/// (`GcBlockState::debugger_native_ack`, interpreter round i2 wave 53, lane
/// L6; stage 2 of the suspension handshake, `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`)?
/// HotSpot's handshake counts a thread in native stopped: it touches no Java
/// state until its transition back, where it blocks. Here the native-call
/// funnel parks it at the native's return
/// (`interpreter::park_if_suspended_at_native_exit`, which with the switch on
/// parks a native it cannot name too), before any bytecode of its caller.
///
/// Interpreter round i5 wave 101, lane L1: also the C JVMTI table's
/// `SuspendThread` / `SuspendThreadList` / `SuspendAllVirtualThreads` stop
/// rule (`jvmti::native_env::thread_stopped`), which waited up to its 2 s
/// bound for such a thread (HotSpot's `SuspendThread` returns at once for a
/// thread in native).
pub(crate) fn in_native_acknowledged(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    id: crate::threading::ThreadId,
) -> bool {
    registry.gc_block_state_of(id).is_some_and(|state| {
        state
            .debugger_native_ack
            .load(std::sync::atomic::Ordering::Acquire)
    })
}

/// One walk of [`await_suspension_settled`] over the debug state, kept for
/// the rounds after it with [`DebuggerGates::settle_arrival_count`]
/// (interpreter round i2 wave 64, lane L6).
pub(in crate::debug) struct SettleWalk {
    /// The threads it found on their way, as far as the debug state tells.
    pub(in crate::debug) pending: Vec<u64>,
    /// Those of them that are laggards of an earlier suspension.
    pub(in crate::debug) laggards: Vec<u64>,
    /// [`DebuggerGates::arrivals`] read before it took the lock.
    pub(in crate::debug) arrivals: u64,
    /// When it ran.
    pub(in crate::debug) at: std::time::Instant,
}

impl SettleWalk {
    /// Must the next round walk again: something arrived (or changed) since
    /// this walk's count was read, or [`SETTLE_REWALK_BACKSTOP`] passed?
    pub(in crate::debug) fn due(&self, arrivals_now: u64) -> bool {
        arrivals_now != self.arrivals || self.at.elapsed() >= SETTLE_REWALK_BACKSTOP
    }
}

/// How long [`await_suspension_settled`] reuses a walk with no arrival
/// counted since ([`DebuggerGates::settle_arrival_count`], wave 64): the
/// bound of a change that notifies nothing (a listing an event published
/// without parking). A backstop, not a poll: the arrivals it covers are
/// rare, and the registry's answers are read every round regardless.
pub(in crate::debug) const SETTLE_REWALK_BACKSTOP: std::time::Duration = std::time::Duration::from_millis(10);

/// When [`await_suspension_settled`] asks the compiled loops to leave again
/// (`interpreter::request_compiled_loop_exits_again`, interpreter round i2
/// wave 53, lane L6): after [`SUSPEND_NUDGE_FIRST`] of waiting, then each
/// time the wait doubles (4, 8, 16, ... ms: at most six times within
/// [`SUSPEND_POINT_WAIT`]). A thread that runs bytecode or polls arrives
/// well within the first slice, so a settled suspension never asks; one the
/// suspension's own loop-exit pause missed is asked within a few
/// milliseconds.
pub(in crate::debug) fn suspend_nudge_due(elapsed: std::time::Duration, nudges: u32) -> bool {
    elapsed >= SUSPEND_NUDGE_FIRST.saturating_mul(1u32 << nudges.min(16))
}

/// One round of the JDWP server's waits for a thread's arrival
/// ([`await_suspension_settled`], [`await_suspend_point_frames`],
/// [`await_inspection_window`]): at most [`DEBUG_STATE_POLL`], ended early by
/// an arrival at a suspend point with [`DebuggerGates::park_condvar`] on
/// (interpreter round i2 wave 54, lane L6). A window's opening does not
/// notify, so the bound stays the old poll.
pub(in crate::debug) fn await_debug_state_poll(shared: &crate::vm::SharedVm) {
    let mut ds = shared.debug.debug_state.lock();
    shared
        .debug
        .debugger_gates
        .await_state_change(&mut ds, DEBUG_STATE_POLL);
}

/// The first slice of [`suspend_nudge_due`].
pub(in crate::debug) const SUSPEND_NUDGE_FIRST: std::time::Duration = std::time::Duration::from_millis(4);

/// How long [`await_suspend_point_frames`] waits for a suspended thread to
/// park at its suspend point.
pub(in crate::debug) const SUSPEND_POINT_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// The settle wait's bound with [`DebuggerGates::suspend_settle_long`]
/// (interpreter round i2 wave 56, lane L6): long enough for a thread an
/// overloaded host deschedules, short enough that a thread that never
/// reaches a suspend point (a compiled loop that does not poll, a native
/// with no acknowledgement) delays one reply, not the session -- it becomes
/// a laggard, which later waits do not wait for.
pub(in crate::debug) const SUSPEND_SETTLE_LONG_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A registered native thread (`NativeContext::register_native_thread`: the
/// XNIO I/O loop, which blocks through `VmNativeThreadBlocker`) returning from
/// its blocking call while the debugger holds it suspended waits here until
/// it is resumed, still GC-blocked (interpreter round i1 wave 18, lane L3).
///
/// Such a thread runs no bytecode, so it never reaches the interpreter's
/// suspend point: it was counted suspended and kept dispatching I/O. HotSpot's
/// rule for a thread in native code is that a suspension lets it run on in
/// native and stops it at its next transition back into the VM; the end of a
/// blocking call is this thread's only such transition. Its frames stay what
/// they were (none: it has no interpreter frame, which is what HotSpot answers
/// for an attached thread that has not called Java). One load while no
/// suspension is in force; `leave_blocked` runs before the thread leaves the
/// blocked region, so a collection never waits for it here.
///
/// Interpreter round i2 wave 55, lane L6
/// (`docs/feature-designs/jvmti-delivery-threading.md`):
/// a C JVMTI agent's `SuspendThread` holds it too, with no JDWP session --
/// JVMTI and JDWP suspensions are one suspension in HotSpot. Until wave 55
/// the gate also required a JDWP session, so with an agent alone a suspended
/// I/O loop kept dispatching.
///
/// Interpreter round i5 wave 106 (lane L1; the audit lane L6's wave-105
/// finding asked for): gated on [`DebuggerGates::suspension_counted`], not
/// [`DebuggerGates::suspension_in_force`], which leaves out the suspension of
/// a thread the last listing saw ended (ZOMBIE): a hold of such a thread
/// returned at once whatever its suspend count. The slow path asks the debug
/// state itself ([`DebugState::holds_off_monitor_entry`]), as before.
#[inline]
pub(crate) fn hold_native_thread_while_suspended(shared: &crate::vm::SharedVm, tid: u64) {
    if shared.debug.debugger_gates.suspension_counted() {
        hold_native_thread_while_suspended_slow(shared, tid);
    }
}

/// A thread whose `ThreadDeath` event suspended it (`SUSPEND_EVENT_THREAD`
/// or `SUSPEND_ALL`; [`send_thread_death_event`] answered true -- the
/// caller's only gate, never a bare "some suspension is counted" bit)
/// waits here until the debugger resumes it or detaches,
/// before its termination: `threadStatus` TERMINATED, `isAlive()` false and
/// the joiners' wake-up (interpreter round i5 wave 106, lane L1). HotSpot's
/// `JavaThread::exit` posts `ThreadEnd` -- the back end's handler suspends the
/// event thread inside the callback -- before `ensure_join`, so a
/// `thread.join()` elsewhere stays blocked while the debugger holds the
/// event set. Lane L6 did this for `main` in wave 105
/// (`vm_exec::end_main_thread_java_side`); every other platform thread posted
/// the event and went on, so its joiners woke while it was held. The caller
/// is in a blocked region (GC-safe) for the whole wait.
#[cold]
#[inline(never)]
pub(crate) fn hold_ending_thread_while_suspended(shared: &crate::vm::SharedVm, tid: u64) {
    let gates = &shared.debug.debugger_gates;
    let mut ds = shared.debug.debug_state.lock();
    while ds.holds_off_monitor_entry(tid) {
        // Bounded, so a resume that publishes no change is still seen.
        gates.await_state_change(&mut ds, BLOCKED_WAIT_BOUND);
    }
}

/// [`hold_native_thread_while_suspended`] while a suspension is in force: the
/// park loop of `interpreter::park_for_debugger` without its task queue (a
/// native thread runs no invocation) and without safepoint polls (it is
/// GC-blocked). Held while the debugger's suspend count or an agent's
/// `SuspendThread` stands ([`DebugState::holds_off_monitor_entry`]), not for
/// a stack read's handshake, which holds a thread only while its stack is
/// read. Ends on the resume, or on a detach (which resumes every JDWP
/// suspension: `DebugState::resume_all`).
#[cold]
#[inline(never)]
pub(in crate::debug) fn hold_native_thread_while_suspended_slow(shared: &crate::vm::SharedVm, tid: u64) {
    wait_while_suspended(shared, tid, DebugState::holds_off_monitor_entry);
}

/// Must a thread in native that is about to leave native for a JNIEnv or C
/// JVMTI function look at its suspension (gci i16/sn,
/// `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`)? A suspension of some
/// thread is in force and the switch is on: two loads, the first of which
/// is down whenever no debugger or agent suspended anything.
#[inline(always)]
pub(crate) fn native_transition_hold_armed(shared: &crate::vm::SharedVm) -> bool {
    let gates = &shared.debug.debugger_gates;
    gates.suspension_in_force() && gates.suspend_at_native_transition()
}

/// A thread whose JNI native's C code runs in native, about to leave native
/// for a JNIEnv or C JVMTI function, waits here while a debugger or a C
/// JVMTI agent holds it suspended (gci i16/sn;
/// `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017` finding 6).
/// Answers whether it waited.
///
/// HotSpot's native -> VM transition (`ThreadInVMfromNative`) processes the
/// thread's pending suspend handshake and blocks there, so a suspended
/// thread's C code runs on only until it calls into the VM. Here the thread
/// ran every such function while suspended (`jvmti::native_env`
/// `thread_stopped` counts it stopped at once: it is in a blocked region).
///
/// The caller is still IN NATIVE (`native::jni`, before its in-native leave):
/// its blocked region stands, its deposited roots are its roots, and every
/// pause excludes it, so a collection never waits for it here, and the
/// in-native leave's fairness rule (`GcBarrier::leave_in_native_if`, gci
/// i15/hx) never sees it as a leaver: it leaves only after the resume. Its
/// frames read through its window as before, so the listing is unchanged by
/// the hold. Held as [`hold_native_thread_while_suspended`] holds: by the
/// debugger's suspend count or an agent's `SuspendThread`
/// ([`DebugState::holds_off_monitor_entry`]), not by a stack read's
/// handshake, which holds a thread only while its stack is read. Never on
/// the JDWP server thread or for work the debugger runs
/// ([`debugger_hooks_suppressed`]), nor on a thread parked at a suspend
/// point (the work its park runs: an invocation's own natives).
/// `CRATONVM_FRAME_TRACE=1` prints `[NATIVE_TRANSITION_SUSPEND] tid=<n>
/// held_us=<n>` after a hold.
#[cold]
#[inline(never)]
pub(crate) fn hold_at_native_transition_while_suspended(
    shared: &crate::vm::SharedVm,
    tid: u64,
) -> bool {
    if debugger_hooks_suppressed() {
        return false;
    }
    {
        let ds = shared.debug.debug_state.lock();
        if !ds.holds_off_monitor_entry(tid) || ds.is_parked(tid) {
            return false;
        }
    }
    let began = std::time::Instant::now();
    wait_while_suspended(shared, tid, DebugState::holds_off_monitor_entry);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[NATIVE_TRANSITION_SUSPEND] tid={tid} held_us={}",
            began.elapsed().as_micros()
        );
    }
    true
}

/// Must thread `tid`, which has just won a contended monitor, give it back
/// because the debugger (or a C JVMTI agent) holds it suspended (interpreter
/// round i2 wave 54, lane L6; [`exit_monitor_while_suspended`])? One load
/// while no suspension is in force.
#[inline]
pub(crate) fn monitor_entry_held_off(
    shared: &crate::vm::SharedVm,
    tid: crate::threading::ThreadId,
) -> bool {
    let gates = &shared.debug.debugger_gates;
    gates.suspension_in_force()
        && gates.monitor_exit_on_suspend()
        && monitor_entry_held_off_slow(shared, tid.0)
}

/// [`monitor_entry_held_off`] while a suspension is in force.
#[cold]
#[inline(never)]
pub(in crate::debug) fn monitor_entry_held_off_slow(shared: &crate::vm::SharedVm, tid: u64) -> bool {
    shared.debug.debug_state.lock().holds_off_monitor_entry(tid)
}

/// A thread that won the contended `monitor` in its blocked entry
/// (`vm_exec::monitor_enter_blocking`, still GC-blocked, its inspection
/// window still open) while the debugger or a C JVMTI agent holds it
/// suspended gives the monitor back, waits for its resume, and enters again
/// (interpreter round i2 wave 54, lane L6;
/// `docs/feature-designs/jvmti-delivery-threading.md`,
/// `CRATONVM_DEBUG_MONITOR_EXIT_ON_SUSPEND`).
///
/// HotSpot's contended entry does this (`ObjectMonitor::enter` with
/// `ExitOnSuspend`): a debugger's suspension of a blocked thread never takes
/// a lock away from the rest of the program, and the thread still reads
/// `MONITOR` (`BLOCKED`) while it waits. Here it won the monitor the moment
/// its holder released it and parked holding it at its next suspend point,
/// so every other thread that needed the lock waited for the debugger's
/// resume (`L6W53JdiSuspendedThreadAndContendedMonitor`).
///
/// The acquisition `block_enter` made is fresh (entry count 1: a re-entrant
/// one never contends), so one `Monitor::exit` undoes it and wakes the next
/// entrant as a release does. The exit is by the monitor, never by the
/// object, whose address a collection may have moved meanwhile. The thread
/// stays in its blocked region throughout (Java state `BLOCKED`, roots
/// deposited, its JMX contention still published), so a collection never
/// waits for it here. `CRATONVM_FRAME_TRACE=1` prints
/// `[MONITOR_EXIT_ON_SUSPEND] tid=<n> given_back=<n>` when it gave one back.
#[inline]
pub(crate) fn exit_monitor_while_suspended(
    shared: &crate::vm::SharedVm,
    tid: crate::threading::ThreadId,
    monitor: &crate::threading::monitor::Monitor,
) {
    if shared.debug.debugger_gates.suspension_in_force() {
        exit_monitor_while_suspended_slow(shared, tid, monitor);
    }
}

/// [`exit_monitor_while_suspended`] while a suspension is in force.
#[cold]
#[inline(never)]
pub(in crate::debug) fn exit_monitor_while_suspended_slow(
    shared: &crate::vm::SharedVm,
    tid: crate::threading::ThreadId,
    monitor: &crate::threading::monitor::Monitor,
) {
    let mut given_back = 0u32;
    while monitor_entry_held_off(shared, tid) {
        if monitor.exit(tid).is_err() {
            // Not ours after all: nothing to give back (never expected).
            break;
        }
        given_back += 1;
        wait_while_suspended(shared, tid.0, DebugState::holds_off_monitor_entry);
        monitor.block_enter(tid);
    }
    if given_back > 0 && crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[MONITOR_EXIT_ON_SUSPEND] tid={} given_back={given_back}",
            tid.0
        );
    }
}

/// The VM's [`crate::threading::monitor::EntryHold`] (interpreter round i2
/// wave 56, lane L6, `CRATONVM_MONITOR_SUSPENSION_AWARE_ENTRY`): a debugger
/// or a C JVMTI agent holding the thread suspended
/// ([`monitor_entry_held_off`], one load without a suspension), waited out as
/// [`exit_monitor_while_suspended`] waits. The loop that uses it lives in the
/// monitor (`Monitor::give_back_fresh_hold_while_held_off`); this is the
/// whole of the debugger's part.
pub(crate) struct DebuggerEntryHold<'a> {
    pub(crate) shared: &'a crate::vm::SharedVm,
}

impl crate::threading::monitor::EntryHold for DebuggerEntryHold<'_> {
    #[inline]
    fn held_off(&self, thread_id: crate::threading::ThreadId) -> bool {
        monitor_entry_held_off(self.shared, thread_id)
    }

    #[cold]
    #[inline(never)]
    fn wait_until_released(&self, thread_id: crate::threading::ThreadId) {
        wait_while_suspended(
            self.shared,
            thread_id.0,
            DebugState::holds_off_monitor_entry,
        );
    }
}

/// Wait, GC-blocked, while `held(debug state, tid)` stays true (interpreter
/// round i2 wave 54, lane L6): the loop of
/// [`hold_native_thread_while_suspended_slow`], woken by the resume's
/// publication with [`DebuggerGates::park_condvar`], a 1 ms poll without.
/// The caller is in a blocked region, so it answers no safepoint.
pub(in crate::debug) fn wait_while_suspended(
    shared: &crate::vm::SharedVm,
    tid: u64,
    held: fn(&DebugState, u64) -> bool,
) {
    let gates = &shared.debug.debugger_gates;
    // Wave 63 (lane L6): the thread's own word, looked up before the
    // debug-state lock (the registry lock is never taken under it).
    let state = if gates.thread_suspend_word() {
        shared
            .threads
            .thread_registry
            .gc_block_state_of(crate::threading::ThreadId(tid))
    } else {
        None
    };
    let mut ds = shared.debug.debug_state.lock();
    let Some(state) = state else {
        while held(&ds, tid) {
            gates.await_state_change(&mut ds, BLOCKED_WAIT_BOUND);
        }
        return;
    };
    if !held(&ds, tid) {
        return;
    }
    let mut word = SuspendWordWait::enroll(gates, &mut ds, tid, &state);
    loop {
        word.note_locked_round();
        if !held(&ds, tid) {
            break;
        }
        // The lock released while the word stands: a resume rewrites it and
        // unparks this thread.
        parking_lot::MutexGuard::unlocked(&mut ds, || {
            while word.wait_unchanged(BLOCKED_WAIT_BOUND) {}
        });
    }
    word.leave(&mut ds, tid);
}

/// [`DebugState::suspend_word_flags`]: the word is live (its thread is
/// enrolled; interpreter round i2 wave 63, lane L6).
pub(crate) const SW_ENROLLED: u32 = 1;
/// [`DebugState::suspend_word_flags`]: [`DebugState::is_thread_suspended`].
pub(crate) const SW_SUSPENDED: u32 = 2;
/// [`DebugState::suspend_word_flags`]: [`DebugState::holds_off_monitor_entry`].
pub(crate) const SW_HOLDS_OFF: u32 = 4;
/// [`DebugState::suspend_word_flags`]: work is queued for the parked thread.
pub(crate) const SW_TASK: u32 = 8;
/// The flag bits of a suspend word; the bits above count its changes.
pub(in crate::debug) const SW_FLAGS: u32 = 0xF;
/// Where a suspend word's change sequence starts.
pub(in crate::debug) const SW_SEQ_SHIFT: u32 = 4;

/// How long a thread waiting on its suspend word goes without a locked round
/// of the debug state (interpreter round i2 wave 63, lane L6): the bound of a
/// change the word does not carry (a listing withdrawn by another thread, a
/// missed wake-up). Every change of a suspend count, a JVMTI suspension or a
/// parked task moves the word, so this is a backstop, not a poll.
pub(crate) const SUSPEND_WORD_BACKSTOP: std::time::Duration = std::time::Duration::from_millis(50);

/// One enrolled thread of [`DebugState::suspend_words`] (wave 63): where its
/// word lives, and the OS threads to unpark when the word moves (one per
/// nested wait; normally all the same thread).
pub(in crate::debug) struct SuspendWordEntry {
    pub(in crate::debug) state: Arc<crate::threading::jvm_thread::GcBlockState>,
    pub(in crate::debug) wakers: Vec<std::thread::Thread>,
}

impl SuspendWordEntry {
    /// Store `flags` with the next change sequence and unpark the waiters,
    /// when they differ from the word's flags (or the word is still 0: a
    /// fresh enrollment). Called under the debug-state lock, so writes never
    /// race each other.
    pub(in crate::debug) fn publish(&self, flags: u32) {
        use std::sync::atomic::Ordering;
        let word = &self.state.debugger_suspend_word;
        let old = word.load(Ordering::Relaxed);
        if old != 0 && old & SW_FLAGS == flags {
            return;
        }
        let seq = (old >> SW_SEQ_SHIFT).wrapping_add(1);
        word.store(seq.wrapping_shl(SW_SEQ_SHIFT) | flags, Ordering::Release);
        for waker in &self.wakers {
            waker.unpark();
        }
    }
}

/// A wait that asks the waiting thread's own suspend word instead of the
/// debug-state lock (interpreter round i2 wave 63, lane L6;
/// [`DebuggerGates::thread_suspend_word`]). The waiter takes a locked round
/// (reads the debug state under its lock and records the word it saw with
/// [`Self::note_locked_round`]), then waits without the lock
/// ([`Self::wait_unchanged`]) until the word moves -- every writer of what
/// the round read moves it, under that lock, and unparks the waiter -- or
/// [`SUSPEND_WORD_BACKSTOP`] passes. Inactive (switch off): every
/// [`Self::wait_unchanged`] answers `false`, so the caller's loop is the
/// lock loop it was.
pub(crate) struct SuspendWordWait {
    pub(in crate::debug) state: Option<Arc<crate::threading::jvm_thread::GcBlockState>>,
    /// The word the last locked round saw; `None` before the first.
    pub(in crate::debug) seen: Option<u32>,
    /// When the last locked round was.
    pub(in crate::debug) locked_at: std::time::Instant,
    /// Locked rounds taken (the `[DEBUG_PARK_WORD]` trace).
    pub(crate) locked_rounds: u64,
    /// Lock-free waits a moved word ended (or found moved).
    pub(crate) word_wakes: u64,
}

impl SuspendWordWait {
    /// Enroll thread `tid` (the calling thread, `GcBlockState` `state`) on
    /// its word under the debug-state lock `ds`, when `gates` say so; an
    /// inactive wait otherwise.
    pub(crate) fn enroll(
        gates: &DebuggerGates,
        ds: &mut DebugState,
        tid: u64,
        state: &Arc<crate::threading::jvm_thread::GcBlockState>,
    ) -> Self {
        let active = gates.thread_suspend_word();
        if active {
            ds.enroll_suspend_word(tid, state);
        }
        Self {
            state: active.then(|| Arc::clone(state)),
            seen: None,
            locked_at: std::time::Instant::now(),
            locked_rounds: 0,
            word_wakes: 0,
        }
    }

    /// Does this wait ask the word (the switch was on at its enrollment)?
    #[inline]
    pub(crate) fn active(&self) -> bool {
        self.state.is_some()
    }

    /// Has a locked round recorded the word yet (wave 65): may the next
    /// [`Self::wait_unchanged`] wait at all?
    #[inline]
    pub(crate) fn has_seen(&self) -> bool {
        self.seen.is_some()
    }

    /// Without the lock and without waiting: has the word moved since the
    /// last locked round (interpreter round i2 wave 66, lane L6)? `false`
    /// before the first locked round and when inactive.
    #[inline]
    pub(crate) fn moved(&self) -> bool {
        match (&self.state, self.seen) {
            (Some(state), Some(seen)) => {
                state
                    .debugger_suspend_word
                    .load(std::sync::atomic::Ordering::Acquire)
                    != seen
            }
            _ => false,
        }
    }

    /// Record, under the debug-state lock, the word a locked round saw.
    pub(crate) fn note_locked_round(&mut self) {
        if let Some(state) = &self.state {
            self.seen = Some(
                state
                    .debugger_suspend_word
                    .load(std::sync::atomic::Ordering::Acquire),
            );
            self.locked_at = std::time::Instant::now();
            self.locked_rounds += 1;
        }
    }

    /// Without the debug-state lock: may the caller skip its next locked
    /// round? `true` after waiting up to `bound` on this thread's parker
    /// with the word still what the last locked round saw; `false` at once
    /// when the word moved, when the backstop is due, before the first
    /// locked round, and always when inactive.
    pub(crate) fn wait_unchanged(&mut self, bound: std::time::Duration) -> bool {
        use std::sync::atomic::Ordering;
        let (Some(state), Some(seen)) = (&self.state, self.seen) else {
            return false;
        };
        let word = &state.debugger_suspend_word;
        if word.load(Ordering::Acquire) != seen {
            self.word_wakes += 1;
            return false;
        }
        if self.locked_at.elapsed() >= SUSPEND_WORD_BACKSTOP {
            return false;
        }
        std::thread::park_timeout(bound);
        if word.load(Ordering::Acquire) != seen {
            self.word_wakes += 1;
            return false;
        }
        true
    }

    /// End the wait under the debug-state lock `ds` (thread `tid`).
    pub(crate) fn leave(&mut self, ds: &mut DebugState, tid: u64) {
        if self.state.take().is_some() {
            ds.leave_suspend_word(tid);
        }
    }
}

/// When a park at a suspend point opens its GC-blocked region (interpreter
/// round i2 wave 68, lane L6; [`DebuggerGates::park_region_at_first_pause`],
/// `docs/feature-designs/jvmti-delivery-threading.md`).
///
/// Eager (switch off, wave 65): after the park's first locked round, and
/// again after every task or relisting that closed it. Lazy (switch on):
/// the park waits outside its region, a counted mutator enrolled with the
/// barrier, until a stop-the-world request first wakes it; it then opens
/// the region instead of arriving at the safepoint
/// ([`ParkGcRegion::enter_at_pause`], whose entry arrives for that pause),
/// and from then on behaves as an eager park for the rest of the hold. A
/// hold that sees no pause -- a step, a breakpoint looked at, a
/// suspend+resume round trip -- deposits nothing and refreshes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParkRegionOpening {
    /// The switch, as the park read it.
    pub(in crate::debug) lazy: bool,
    /// A pause has opened the region during this hold.
    pub(in crate::debug) paused: bool,
}

impl ParkRegionOpening {
    /// For one hold; `lazy` from [`DebuggerGates::park_region_at_first_pause`].
    pub(crate) const fn new(lazy: bool) -> Self {
        Self {
            lazy,
            paused: false,
        }
    }

    /// Does this park wait outside its region until a pause comes (and so
    /// need the barrier to wake it)?
    #[inline]
    pub(crate) fn lazy(&self) -> bool {
        self.lazy
    }

    /// May the park open its region on its own, at a quiet word, without a
    /// pause asking: always when eager, once a pause has opened it when lazy.
    #[inline]
    pub(crate) fn enters_eagerly(&self) -> bool {
        !self.lazy || self.paused
    }

    /// Should the park, finding a stop-the-world request with its region
    /// closed, open the region (arriving through the entry) rather than
    /// arrive at the safepoint? Only when lazy, once a locked round has seen
    /// its word (`word_seen`: its listing stands), and not while the word
    /// carries news for the locked round (`news`, wave 66's
    /// `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`): a task would close the
    /// region again at once.
    #[inline]
    pub(crate) fn opens_at_pause(&self, word_seen: bool, news: bool) -> bool {
        self.lazy && word_seen && !news
    }

    /// The region was opened at a pause.
    #[inline]
    pub(crate) fn note_opened_at_pause(&mut self) {
        self.paused = true;
    }
}

/// The GC-blocked region a park at a suspend point waits in between its
/// locked rounds (interpreter round i2 wave 65, lane L6;
/// [`DebuggerGates::park_gc_blocked`],
/// `docs/feature-designs/jvmti-delivery-threading.md`).
///
/// The `Object.wait` shape, by the same calls
/// (`NativeContextImpl::begin_blocking_region_with_state` /
/// `end_blocking_region`): the TLAB retired, the roots deposited
/// (`deposit_root_snapshot`, which raises `in_blocked_region`), the barrier's
/// blocked count entered -- arriving once for a pause already requested --
/// and, on the way out, the blocked count left (waiting out a pause in
/// progress) and the composed fixup of the collections slept through applied
/// to the frames and every off-frame reference (`check_post_block_gc`, which
/// also moves frames a redefinition made obsolete). In between a pause
/// excludes the thread from its census and remaps it through its deposit:
/// it neither wakes it nor waits for it.
///
/// While open the thread's frames hold the addresses of its last deposit, so
/// nothing may read or write them, nor the heap, from this thread: the park
/// closes the region before a task (an invocation, a heap read, a frame
/// write), before it relists its frames and before it leaves. The listing
/// it published stays readable meanwhile: its object ids are pinned handles,
/// which every collection remaps.
///
/// The thread reads RUNNABLE inside (`GcBlockState::java_state` 0, so
/// `Thread.getState()`, JDWP `ThreadReference.Status` and JVMTI
/// `GetThreadState` answer as for a thread suspended while running, as
/// HotSpot's do); the value it had is put back when the region closes.
///
/// Wave 66 (lane L6, [`DebuggerGates::park_region_reuses_deposit`]): the
/// leave ends with a full refresh of the root snapshot from the frames
/// (`check_post_block_gc`'s no-flag deposit). When neither the region nor
/// that refresh saw a collection or a redefinition, the leave records a
/// [`StandingDeposit`] key. A re-entry whose key still matches -- no pause,
/// no other root publish, the same frames at the same pc, and only work
/// that never touches this thread's frames ran meanwhile (heap work, a
/// relisting; the park calls [`Self::forget_standing`] after any other
/// task) -- raises the flag over that standing snapshot
/// ([`Self::reuse_standing`]) instead of depositing again.
pub(crate) struct ParkGcRegion {
    /// `GcBlockState::java_state` as it was before the region; `Some` while
    /// the region is open.
    pub(in crate::debug) saved_java_state: Option<u8>,
    /// Regions entered (the `[DEBUG_PARK_WORD]` trace).
    pub(crate) entries: u64,
    /// Wave 66: may a re-entry reuse the standing deposit?
    pub(in crate::debug) reuse: bool,
    /// Wave 66: `CRATONVM_DBG_BLOCKGC` -- check every reused entry's
    /// snapshot against the frames' live object slots.
    pub(in crate::debug) audit: bool,
    /// Wave 66: the key the open region's deposit was made under (read
    /// right after it), compared at the leave.
    pub(in crate::debug) entered: Option<StandingDeposit>,
    /// Wave 66: the key of the snapshot the last leave refreshed, while a
    /// re-entry may still reuse it.
    pub(in crate::debug) standing: Option<StandingDeposit>,
    /// Wave 66: entries that reused the standing deposit (the trace).
    pub(crate) reuses: u64,
    /// Wave 66: time spent opening regions (deposit and blocked-count
    /// entry, without a pause's wait) and closing them (the blocked-count
    /// leave, the fixup and the refresh), in ns (the trace).
    pub(crate) enter_ns: u64,
    pub(crate) leave_ns: u64,
    /// Wave 68: entries made at a pause that woke the park
    /// ([`Self::enter_at_pause`], [`ParkRegionOpening`]; the trace).
    pub(crate) pause_entries: u64,
    /// Wave 68: may a leave for heap work keep the entry's snapshot
    /// ([`DebuggerGates::park_leave_keeps_unfolded_snapshot`])?
    pub(in crate::debug) keep_unfolded: bool,
    /// Wave 68: leaves that kept the entry's snapshot (the trace).
    pub(crate) keeps: u64,
    /// Wave 68: the root snapshot's length right after the last full build
    /// of it on this thread (an entry's deposit, a leave's refresh). A reuse
    /// cuts the snapshot back to it before it appends the off-frame and JNI
    /// roots again, so a run of kept leaves and reused entries does not grow
    /// the snapshot by one copy of those roots per cycle.
    pub(in crate::debug) base_len: Option<usize>,
}

/// What a park region's standing root snapshot was made from (interpreter
/// round i2 wave 66, lane L6): equal keys mean no collection completed, no
/// class was redefined, no other root publish replaced the snapshot and the
/// frame stack has the same depth and top pc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StandingDeposit {
    pub(in crate::debug) generation: u64,
    pub(in crate::debug) collections: u64,
    pub(in crate::debug) publish_seq: u64,
    pub(in crate::debug) frames: usize,
    pub(in crate::debug) top_pc: usize,
    pub(in crate::debug) redefinitions: u64,
}

impl StandingDeposit {
    /// The key as it stands now for `thread`.
    pub(in crate::debug) fn read(
        shared: &crate::vm::SharedVm,
        thread: &crate::threading::jvm_thread::JvmThread,
    ) -> Self {
        use std::sync::atomic::Ordering;
        Self {
            generation: shared.mem.gc_barrier.gc_generation.load(Ordering::Acquire),
            collections: shared.mem.heap.collection_count(),
            publish_seq: thread.root_publish_seq.load(Ordering::Acquire),
            frames: thread.frames.len(),
            top_pc: thread.frames.last().map_or(0, |f| f.pc),
            redefinitions: cratonvm_classloading::class_redefinition_count(),
        }
    }

    /// The same moves-and-frames as `other`: everything but the publish
    /// sequence, which every deposit (the leave's refresh too) bumps.
    pub(in crate::debug) fn same_heap_and_frames(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.collections == other.collections
            && self.frames == other.frames
            && self.top_pc == other.top_pc
            && self.redefinitions == other.redefinitions
    }
}

impl ParkGcRegion {
    /// No region open yet.
    pub(crate) const fn closed() -> Self {
        Self {
            saved_java_state: None,
            entries: 0,
            reuse: false,
            audit: false,
            entered: None,
            standing: None,
            reuses: 0,
            enter_ns: 0,
            leave_ns: 0,
            pause_entries: 0,
            keep_unfolded: false,
            keeps: 0,
            base_len: None,
        }
    }

    /// Wave 68: let a leave for heap work keep the entry's snapshot when
    /// nothing folded into the region (`on`, from
    /// [`DebuggerGates::park_leave_keeps_unfolded_snapshot`]). Only with
    /// deposit reuse ([`Self::with_deposit_reuse`], called first): the leave
    /// needs the entry's key, which only a reusing region records.
    pub(crate) fn with_unfolded_keep(mut self, on: bool) -> Self {
        self.keep_unfolded = on && self.reuse;
        self
    }

    /// Wave 66: let a re-entry reuse the standing deposit (`on`, from
    /// [`DebuggerGates::park_region_reuses_deposit`]), auditing each reuse
    /// when `audit`.
    pub(crate) fn with_deposit_reuse(mut self, on: bool, audit: bool) -> Self {
        self.reuse = on;
        self.audit = on && audit;
        self
    }

    /// Wave 66: work that may have read or written this thread's frames or
    /// run Java on it ran since the last leave: the next entry deposits in
    /// full.
    pub(crate) fn forget_standing(&mut self) {
        self.standing = None;
    }

    /// Wave 66: raise the GC-blocked flag over the snapshot the last leave
    /// refreshed, if its key still holds; `false` (nothing done) otherwise.
    /// Called by [`Self::enter`] with `java_state` stored and the TLAB
    /// retired, where the full deposit would run.
    ///
    /// The standing snapshot holds every root the full deposit would find
    /// in the frames, the class owners, the compiled band and the shadow
    /// stack, at their current addresses: nothing moved since it was taken
    /// (no collection), the frames are the ones it scanned (no task touched
    /// them, no publish replaced it). What the work run meanwhile may have
    /// changed -- the thread's off-frame references and JNI local
    /// references -- is scanned again and appended (a duplicate only
    /// over-retains). The parts only the flag-raising deposit makes are
    /// made here as it makes them: the SATB flush, the publish sequence,
    /// the per-slot origins the wake writes back, and the frame-class
    /// census. The frame trace and its JMX attribution the region's first
    /// deposit captured still stand: the frames did not change. The thread
    /// status the full deposit would write is the RUNNABLE the leave wrote.
    pub(in crate::debug) fn reuse_standing(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let Some(standing) = self.standing.take() else {
            return false;
        };
        if standing != StandingDeposit::read(shared, thread) {
            return false;
        }
        // The refresh skipped the compiled band (a measurement lever): it
        // does not hold the band's roots.
        if crate::runtime::env_cache::noflag_deposit_skips_jit_scan() {
            return false;
        }
        // A blocked-monitor proof only a full deposit settles.
        if thread
            .gc_block_state
            .monitor_block_proven_jit_depth
            .load(Ordering::Relaxed)
            == crate::threading::jvm_thread::MONITOR_BLOCK_PROOF_ARMED
        {
            return false;
        }
        // Pure memos, cleared before every flag-raising deposit.
        thread.jit_hashmap_string_node_cache.clear();
        thread.string_case_cache.clear();
        let thread: &crate::threading::jvm_thread::JvmThread = &*thread;
        thread.root_publish_seq.fetch_add(1, Ordering::Relaxed);
        crate::memory::reclaim_guard::note_root_publish(shared, thread);
        shared.mem.heap.flush_thread_satb();
        {
            let mut snapshot = thread.root_snapshot.lock();
            // Wave 68: what earlier reuses appended goes before this one
            // appends again (a kept leave leaves it in place).
            if let Some(len) = self.base_len {
                if len <= snapshot.len() {
                    snapshot.truncate(len);
                }
            }
            crate::memory::roots::push_off_frame_thread_roots(thread, &mut snapshot);
            crate::native::jni::collect_local_ref_roots(&mut snapshot);
        }
        {
            let mut origins = thread.gc_block_state.slot_origins.lock();
            origins.clear();
            for (fi, fr) in thread.frames.iter().enumerate() {
                let live_mask = fr.live_locals_mask_here();
                for li in 0..fr.locals_len() {
                    if let crate::types::Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        origins.push(crate::threading::jvm_thread::SlotOrigin {
                            frame: fi as u32,
                            idx: li as u32,
                            is_stack: false,
                            live: live_mask.is_live(li),
                            orig: a,
                            cur: a,
                        });
                    }
                }
                for si in 0..fr.stack.len() {
                    if let crate::types::Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        origins.push(crate::threading::jvm_thread::SlotOrigin {
                            frame: fi as u32,
                            idx: si as u32,
                            is_stack: true,
                            live: true,
                            orig: a,
                            cur: a,
                        });
                    }
                }
            }
        }
        crate::runtime::interpreter::obsolete_frames::publish_blocked_frame_classes(thread);
        if self.audit {
            Self::audit_reused(shared, thread);
        }
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        self.reuses += 1;
        true
    }

    /// Wave 66, `CRATONVM_DBG_BLOCKGC`: every live object slot of the frames
    /// is in the reused snapshot (as the full deposit's own
    /// `UNPUBLISHED-LIVE-SLOT` check asks of a fresh one).
    pub(in crate::debug) fn audit_reused(
        shared: &crate::vm::SharedVm,
        thread: &crate::threading::jvm_thread::JvmThread,
    ) {
        let snapshot = thread.root_snapshot.lock();
        let published: HashSet<usize> = snapshot.iter().map(|r| r.as_ptr() as usize).collect();
        let len = snapshot.len();
        drop(snapshot);
        let heap = &shared.mem.heap;
        let mut missing = 0usize;
        let mut check = |a: usize| {
            if !published.contains(&a) && heap.is_heap_addr(a).is_some() {
                missing += 1;
            }
        };
        for fr in thread.frames.iter() {
            let live_mask = fr.live_locals_mask_here();
            for li in 0..fr.locals_len() {
                if !live_mask.is_live(li) {
                    continue;
                }
                if let crate::types::Value::Object(Some(o)) = fr.get_local(li as u16) {
                    check(o.as_ptr() as usize);
                }
            }
            for si in 0..fr.stack.len() {
                if let crate::types::Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    check(o.as_ptr() as usize);
                }
            }
        }
        eprintln!(
            "[DEBUG_PARK_REUSE] tid={} frames={} snapshot={len} missing={missing}",
            thread.thread_id.0,
            thread.frames.len()
        );
    }

    /// Is the region open: is the thread GC-blocked by it?
    #[inline]
    pub(crate) fn is_open(&self) -> bool {
        self.saved_java_state.is_some()
    }

    /// Open the region for `thread`, the calling thread's own, at its
    /// suspend point. Called with no debug-state lock held: the entry may
    /// arrive at a pause already requested and wait for its end. Answers
    /// whether it opened it -- not when it is already open, nor for a thread
    /// already GC-blocked (its own blocked region stands, and the park then
    /// answers no pause either way).
    pub(crate) fn enter(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if self.is_open()
            || thread
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire)
        {
            return false;
        }
        // RUNNABLE while blocked here; stored before the deposit raises the
        // flag, so the flag never shows over a stale kind (a `Thread.sleep`
        // the thread left long ago).
        let saved = thread.gc_block_state.java_state.swap(0, Ordering::AcqRel);
        // As `begin_blocking_region_with_state`: a pause slept through may
        // grow the young arena and free the buffer a kept TLAB points into.
        thread.tlab.retire();
        let tid = thread.thread_id;
        let started = std::time::Instant::now();
        // Wave 66: the key this region's roots are taken under, read while
        // the thread is still a counted mutator (no pause can complete
        // before the flag goes up), so a collection that folds into the
        // region shows as a moved generation at the leave.
        let entered = StandingDeposit::read(shared, thread);
        // Wave 66: the standing deposit, when its key still holds; else the
        // full deposit.
        let reused = self.reuse && self.reuse_standing(shared, thread);
        self.standing = None;
        if !reused {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            crate::jit::conservative_roots::with_frozen_band_memo(None, || {
                ctx.deposit_root_snapshot();
            });
            self.base_len = Some(thread.root_snapshot.lock().len());
        }
        self.entered = self.reuse.then_some(entered);
        self.enter_ns = self
            .enter_ns
            .saturating_add(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        if shared.mem.gc_barrier.mark_blocked_region_enter() {
            // A pause was already requested and may have counted this
            // thread: arrive for it once (the deposit raised the flag first,
            // hence `_auto`).
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
        self.saved_java_state = Some(saved);
        self.entries += 1;
        true
    }

    /// Wave 68 ([`ParkRegionOpening`]): open the region at a stop-the-world
    /// request that found the park outside it, in place of the park's
    /// safepoint arrival. [`Self::enter`] as it is: the thread is still a
    /// counted mutator the request is waiting for, so the deposit runs
    /// before the pause can start, and the entry's
    /// `mark_blocked_region_enter` reports the request and arrives for it
    /// once (`arrive_and_wait_auto`), as a native that blocks during a pause
    /// does (`begin_blocking_region_with_state`). The caller drops its
    /// barrier waker first: once inside, a later request neither wakes the
    /// thread nor waits for it. Answers whether it opened the region.
    pub(crate) fn enter_at_pause(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) -> bool {
        let opened = self.enter(shared, thread);
        if opened {
            self.pause_entries += 1;
        }
        opened
    }

    /// Close the region, if open: leave the blocked count (waiting out a
    /// pause in progress), apply the collections' fixup to `thread`'s frames
    /// and off-frame references, and put its `java_state` back. Called with
    /// no debug-state lock held. Afterwards the thread is a running mutator
    /// again, its frames current.
    pub(crate) fn leave(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) {
        self.leave_inner(shared, thread, false);
    }

    /// Wave 68 ([`DebuggerGates::park_leave_keeps_unfolded_snapshot`],
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// [`Self::leave`] before heap work queued for any thread -- work that
    /// neither reads nor writes this thread's frames. With the switch, when
    /// no collection, fold or redefinition reached the region since its
    /// entry, the leave keeps the snapshot the entry deposited (or reused)
    /// instead of refreshing it from the frames, and skips the wake audit:
    /// the frames did not move and the thread ran nothing, so the snapshot
    /// is still exact (`NativeContextImpl::check_post_block_gc_keeping`).
    ///
    /// Sound until the thread is next GC-blocked: in between it is a counted
    /// mutator running Rust heap work, so a pause cannot complete without its
    /// arrival, and every arrival (`safepoint_check`) republishes the
    /// snapshot from the frames, which also bumps the publish sequence and
    /// so refuses the next entry's reuse. Without a pause, the next entry
    /// reuses the kept snapshot ([`Self::reuse_standing`], which cuts what
    /// earlier reuses appended before it appends again).
    pub(crate) fn leave_for_heap_work(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) {
        let keep = self.keep_unfolded;
        self.leave_inner(shared, thread, keep);
    }

    /// [`Self::leave`], keeping the entry's snapshot when `keep` and nothing
    /// folded into the region (wave 68).
    pub(in crate::debug) fn leave_inner(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
        keep: bool,
    ) {
        let Some(saved) = self.saved_java_state.take() else {
            return;
        };
        let started = std::time::Instant::now();
        shared.mem.gc_barrier.mark_blocked_region_leave();
        let entered = self.entered.take();
        // Wave 68: what the region was entered under; the wake keeps the
        // snapshot only when the heap still stands as it did then.
        let keep_if = if keep {
            entered.map(|e| crate::vm::vm_exec::UnfoldedSince {
                gc_generation: e.generation,
                collections: e.collections,
                redefinitions: e.redefinitions,
            })
        } else {
            None
        };
        let kept = crate::vm::vm_exec::NativeContextImpl {
            shared,
            thread: &mut *thread,
        }
        .check_post_block_gc_keeping(keep_if);
        if kept {
            self.keeps += 1;
        } else {
            // Wave 68: the refresh is the snapshot's new full build.
            self.base_len = Some(thread.root_snapshot.lock().len());
        }
        // Wave 66: the refresh `check_post_block_gc` ended with stands for
        // the next entry when no collection and no redefinition reached the
        // region (no fixup was applied, no frame converted) and the frames
        // are the ones the region was entered with. Read with the flag
        // down: the thread is counted, so no pause completes after it.
        // Wave 68: so does a kept snapshot, under the same test.
        self.standing = entered.and_then(|entered| {
            let now = StandingDeposit::read(shared, thread);
            now.same_heap_and_frames(&entered).then_some(now)
        });
        self.leave_ns = self
            .leave_ns
            .saturating_add(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        thread
            .gc_block_state
            .java_state
            .store(saved, std::sync::atomic::Ordering::Release);
    }
}

/// The 1 ms poll of every wait on the debug state with
/// [`DebuggerGates::park_condvar`] off ([`DebuggerGates::await_state_change`]).
pub(crate) const DEBUG_STATE_POLL: std::time::Duration = std::time::Duration::from_millis(1);

/// How long a GC-blocked thread held by a suspension
/// ([`hold_native_thread_while_suspended`], [`exit_monitor_while_suspended`])
/// waits for a notification before it looks again, with
/// [`DebuggerGates::park_condvar`] on (interpreter round i2 wave 54, lane
/// L6). Every resume notifies, so this bounds only a missed one; such a
/// thread answers no safepoint, so nothing else needs it awake.
pub(in crate::debug) const BLOCKED_WAIT_BOUND: std::time::Duration = std::time::Duration::from_millis(50);

/// `VirtualMachine.Exit` (interpreter round i1 wave 23): once its reply has
/// gone out, end the process with the debugger's exit code, as HotSpot's back
/// end does (`exitWithCode` sends the reply, then `forceExit`: the transport
/// closed and a C `exit`, no Java shutdown hooks). The command only recorded
/// the code, which nothing read, so the debuggee ran on and JDI's
/// `VirtualMachine.exit` (jdb's `exit` on a launched VM) waited for a
/// disconnect that never came. The VM's buffered stdout and stderr are
/// flushed first: HotSpot's are unbuffered, so what the program printed
/// before the exit is delivered there.
pub(in crate::debug) fn exit_for_debugger(shared: &crate::vm::SharedVm, code: i32) -> ! {
    tracing::info!("JDWP: VirtualMachine.Exit({code})");
    let _ = shared.natives.fd_table.flush(1);
    let _ = shared.natives.fd_table.flush(2);
    cratonvm_native_api::process_exit::exit_process(code)
}

// ---------------------------------------------------------------------------
// T6.5 — SharedVm bridge: live VM access for debugger-initiated invocation
// ---------------------------------------------------------------------------

/// Concrete [`DebuggerVmBridge`] that forwards to a live [`SharedVm`].
///
/// Wave 8: every call runs on a thread parked at an interpreter suspend
/// point ([`run_on_parked_thread`]). An invocation runs on the very thread
/// the command names, as JDWP specifies — `Thread.currentThread()`, its
/// thread-locals and the monitors it holds are that thread's — with its
/// frames pushed above the parked one, and is refused
/// (`THREAD_NOT_SUSPENDED`) when that thread is not parked at a suspend
/// point. An allocation runs on any parked thread. Until wave 8 each call
/// ran on the JDWP server thread, under an ephemeral `JvmThread` that
/// borrowed the target's id and that no collection counted, waited for or
/// scanned (the arguments and the running frames could be moved under it).
///
/// Wave 9: the invocation options are honoured
/// ([`run_invocation_on_parked_thread`]). Without `INVOKE_SINGLE_THREADED`
/// every suspended thread runs for the call and all are suspended again
/// after it; until wave 9 only the target thread ever ran, so an invoked
/// method that waited for another suspended thread (a monitor it holds, a
/// queue it fills) hung the session. And the JDWP server serves commands
/// and events while the call runs (`run_jdwp_server`), so a stop inside the
/// invoked method is reported and can be resumed.
pub struct SharedVmBridge {
    pub shared: Arc<crate::vm::SharedVm>,
}

/// How an invocation selects its method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::debug) enum InvokeHow {
    /// `ClassType.InvokeMethod`.
    Static,
    /// `ObjectReference.InvokeMethod`: selected on the receiver's class, so
    /// `toString` named by `java.lang.Object`'s method id runs the override
    /// (until wave 8 the declaring class's method ran, whatever the receiver).
    Virtual,
    /// `ObjectReference.InvokeMethod` with `INVOKE_NONVIRTUAL`.
    NonVirtual,
    /// `ClassType.NewInstance` (wave 24): allocate, then run the constructor.
    Construct,
}

/// One invocation, as the parked thread runs it ([`invoke_on_parked_thread`]).
pub(in crate::debug) struct ParkedInvocation {
    pub(in crate::debug) class_id: crate::classloading::ClassId,
    pub(in crate::debug) method_name: String,
    pub(in crate::debug) descriptor: String,
    /// 0 for a static method.
    pub(in crate::debug) receiver_id: u64,
    pub(in crate::debug) args: Vec<DebuggerValue>,
    pub(in crate::debug) return_sig: String,
    pub(in crate::debug) how: InvokeHow,
    /// `ClassType.NewInstance` of an abstract class or an interface (wave
    /// 25): the class's Java name, which the `InstantiationException` the
    /// invocation throws instead of constructing carries.
    pub(in crate::debug) not_instantiable: Option<String>,
}

impl SharedVmBridge {
    pub fn new(shared: Arc<crate::vm::SharedVm>) -> Self {
        Self { shared }
    }

    /// The class a wire class id names, if it is loaded.
    pub(in crate::debug) fn loaded_class(&self, class_id: u64) -> Option<crate::classloading::ClassId> {
        let raw = u32::try_from(class_id).ok()?;
        let cid = crate::classloading::ClassId::new(raw);
        let cm = self.shared.classes.class_manager.read();
        cm.class_store.get(cid).map(|_| cid)
    }

    /// The refusal for an invocation on thread `tid`, which is not parked at
    /// a suspend point: `INVALID_THREAD`, as HotSpot's back end refuses an
    /// invocation on a thread that no event suspended (its invoker's
    /// `available` flag), which JDI turns into
    /// `IncompatibleThreadStateException`; `INVALID_OBJECT` for an id that
    /// names nothing at all (interpreter round i1 wave 43, lane L1:
    /// `tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`, measured
    /// on HotSpot 25.0.3). It answered `THREAD_NOT_SUSPENDED` for a live
    /// thread, which JDI turns into an `InternalException`, and
    /// `INVALID_THREAD` for an id naming nothing.
    pub(in crate::debug) fn not_parked(&self, tid: u64) -> BridgeError {
        let split = self.shared.debug.debugger_gates.destroy_java_vm_split();
        if id_names_something(&self.shared, ids::thread_to_wire_split(split, tid)) {
            BridgeError::InvalidThread
        } else {
            BridgeError::InvalidObject
        }
    }

    /// Check an invocation and run it on its thread.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::debug) fn invoke(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        how: InvokeHow,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        let class = self
            .loaded_class(class_id)
            .ok_or(BridgeError::InvalidClass)?;
        let (method_name, descriptor, is_static) = self
            .method_sig_for(class_id, method_id)
            .ok_or(BridgeError::InvalidMethod)?;
        if matches!(how, InvokeHow::Virtual | InvokeHow::NonVirtual) && receiver_id == 0 {
            return Err(BridgeError::InvalidObject);
        }
        // `ObjectReference.InvokeMethod` of a static method (interpreter
        // round i1 wave 43, lane L1): the method runs as a static one, the
        // object checked and then ignored, as HotSpot 25.0.3 answers it
        // (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`:
        // `add(4, 5)` on an instance is 9). The receiver was passed as the
        // method's first argument, which shifted every argument by one.
        let (how, receiver_id) =
            if is_static && matches!(how, InvokeHow::Virtual | InvokeHow::NonVirtual) {
                let known = {
                    let ds = self.shared.debug.debug_state.lock();
                    object_collected(&self.shared, &ds, receiver_id) == Some(false)
                };
                if !known {
                    return Err(BridgeError::InvalidObject);
                }
                (InvokeHow::Static, 0)
            } else {
                (how, receiver_id)
            };
        let mut not_instantiable = None;
        if how == InvokeHow::Construct {
            // Only a constructor constructs. An abstract class or an
            // interface has no instances: HotSpot's back end runs JNI
            // `NewObjectA`, which throws `InstantiationException` naming the
            // class (`InstanceKlass::check_valid_for_instantiation`), and the
            // debugger gets it as the invocation's exception (wave 25,
            // `tools/probes/interp/L1/L1W25JdiStopMonitors.java`). Wave 24
            // refused the command (`INVALID_CLASS`) instead.
            if method_name != "<init>" {
                return Err(BridgeError::InvalidMethod);
            }
            let cm = self.shared.classes.class_manager.read();
            match cm.class_store.get(class) {
                None => return Err(BridgeError::InvalidClass),
                Some(c) if c.name.starts_with('[') => return Err(BridgeError::InvalidClass),
                Some(c) if c.is_interface() || c.is_abstract() => {
                    not_instantiable = Some(c.name.replace('/', "."));
                }
                Some(_) => {}
            }
        }
        if descriptor_arg_count(&descriptor) != Some(args.len()) {
            return Err(BridgeError::IllegalArgument);
        }
        let call = ParkedInvocation {
            class_id: class,
            method_name,
            descriptor,
            receiver_id,
            args: args.to_vec(),
            return_sig: return_sig.to_string(),
            how,
            not_instantiable,
        };
        match run_invocation_on_parked_thread(
            &self.shared,
            thread_id,
            single_threaded,
            move |shared, thread| invoke_on_parked_thread(shared, thread, call),
        ) {
            Ok(outcome) => outcome,
            Err(ParkedError::NotParked) => Err(self.not_parked(thread_id)),
            Err(ParkedError::Busy) => Err(BridgeError::AlreadyInvoking),
            Err(ParkedError::Lost) => Err(BridgeError::Internal),
        }
    }

    /// Look up a (method_name, descriptor) by the debugger-side methodID,
    /// which is `jdwp_method_id` of the name and descriptor, so overloads
    /// have distinct ids.
    /// The name, descriptor and staticness of the method `method_id` names
    /// in class `class_id`.
    pub(in crate::debug) fn method_sig_for(&self, class_id: u64, method_id: u64) -> Option<(String, String, bool)> {
        let ds = self.shared.debug.debug_state.lock();
        let methods = ds.class_methods.get(&class_id)?;
        let hit = methods.iter().find(|m| m.method_id.0 == method_id)?;
        // ACC_STATIC.
        Some((
            hit.name.clone(),
            hit.signature.clone(),
            hit.mod_bits & 0x0008 != 0,
        ))
    }
}

/// The refusal for work that needs thread `tid` parked at a suspend point
/// when it is not: `THREAD_NOT_SUSPENDED` for a live (or suspended) thread,
/// `INVALID_THREAD` for an id that names none.
pub(crate) fn unparked_thread_refusal(ds: &DebugState, tid: u64) -> BridgeError {
    if ds.thread_names.contains_key(&tid) || ds.is_thread_suspended(tid) {
        BridgeError::ThreadNotSuspended
    } else {
        BridgeError::InvalidThread
    }
}

/// The number of arguments a method descriptor declares (one per parameter,
/// `long` and `double` included), or `None` for a malformed descriptor.
pub(in crate::debug) fn descriptor_arg_count(descriptor: &str) -> Option<usize> {
    let params = descriptor.strip_prefix('(')?.split(')').next()?;
    let mut count = 0usize;
    let mut chars = params.chars();
    while let Some(c) = chars.next() {
        let mut elem = c;
        while elem == '[' {
            elem = chars.next()?;
        }
        if elem == 'L' {
            chars.by_ref().find(|&ch| ch == ';')?;
        }
        count += 1;
    }
    Some(count)
}

/// Run one debugger invocation on the parked thread `thread` (wave 8; see
/// [`SharedVmBridge`]). The ids are resolved here, on the registered thread,
/// and the object arguments are pinned for the call (the debugger's handles
/// are weak); the result or the thrown exception is exported before this
/// thread reaches another safepoint, so it cannot move in between.
pub(in crate::debug) fn invoke_on_parked_thread(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    call: ParkedInvocation,
) -> Result<InvokeOutcome, BridgeError> {
    use crate::error::MethodCallFailed;
    use crate::types::Value;
    let mut vm_args = Vec::with_capacity(call.args.len() + 1);
    let receiver = {
        let ds = shared.debug.debug_state.lock();
        let receiver = if call.receiver_id == 0 {
            None
        } else {
            Some(object_for_id(shared, &ds, call.receiver_id).ok_or(BridgeError::InvalidObject)?)
        };
        if let Some(r) = receiver {
            vm_args.push(Value::Object(Some(r)));
        }
        for a in &call.args {
            vm_args.push(inspect::debugger_value_to_vm(shared, &ds, a)?);
        }
        receiver
    };
    let target = match (call.how, receiver) {
        (InvokeHow::Virtual, Some(r)) => shared.mem.heap.class_id_of(r),
        _ => call.class_id,
    };
    let pin_base = thread.native_pin_roots.len();
    for v in &vm_args {
        if let Value::Object(Some(o)) = v {
            thread.native_pin_roots.push(*o);
        }
    }
    let result = match call.how {
        InvokeHow::NonVirtual => crate::vm::invoke_on_class_shared_no_retarget(
            shared,
            thread,
            target,
            &call.method_name,
            &call.descriptor,
            &vm_args,
        ),
        // Wave 24: `ClassType.NewInstance` allocates and runs the
        // constructor as JNI `NewObjectA` does (class initialization, the
        // finalizer registration, the new object pinned across `<init>`),
        // on this thread. The answer is the object, wherever `<init>` left it.
        // Wave 25: an abstract class or an interface: JNI `NewObjectA`'s
        // `InstantiationException`, thrown by the invocation.
        InvokeHow::Construct if call.not_instantiable.is_some() => {
            let name = call.not_instantiable.as_deref();
            match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/InstantiationException",
                name,
            ) {
                Ok(exc) => Err(MethodCallFailed::ExceptionThrown(exc)),
                Err(failed) => Err(failed),
            }
        }
        InvokeHow::Construct => {
            use cratonvm_native_api::NativeContext as _;
            let mut ctx = crate::vm::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            ctx.new_object_initialized_with_class_id(call.class_id, &call.descriptor, &vm_args)
        }
        InvokeHow::Static | InvokeHow::Virtual => crate::vm::invoke_on_class_shared(
            shared,
            thread,
            target,
            &call.method_name,
            &call.descriptor,
            &vm_args,
        ),
    };
    thread.native_pin_roots.truncate(pin_base);
    // No safepoint from here to the export: the result stays where it is.
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    match result {
        Ok(None) => Ok(InvokeOutcome::returned(DebuggerValue::Void)),
        Ok(Some(v)) => Ok(InvokeOutcome::returned(inspect::vm_value_to_debugger(
            shared,
            &cm,
            &mut ds,
            v,
            &call.return_sig,
        ))),
        Err(MethodCallFailed::ExceptionThrown(exc)) => Ok(InvokeOutcome::threw(export_object(
            shared,
            &mut ds,
            exc,
            ObjectExport::Sent,
        ))),
        // Until wave 8 this answered "threw object 1", an id naming nothing
        // (or a thread).
        Err(MethodCallFailed::InternalError(_)) => Err(BridgeError::Internal),
    }
}

impl DebuggerVmBridge for SharedVmBridge {
    fn invoke_static(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        self.invoke(
            0,
            class_id,
            method_id,
            thread_id,
            args,
            return_sig,
            InvokeHow::Static,
            single_threaded,
        )
    }

    fn invoke_instance(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        non_virtual: bool,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        let how = if non_virtual {
            InvokeHow::NonVirtual
        } else {
            InvokeHow::Virtual
        };
        self.invoke(
            receiver_id,
            class_id,
            method_id,
            thread_id,
            args,
            return_sig,
            how,
            single_threaded,
        )
    }

    /// `ClassType.NewInstance` (wave 24): an invocation of the constructor
    /// on its thread whose answer is the new object, tagged by its class.
    fn new_instance(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        self.invoke(
            0,
            class_id,
            method_id,
            thread_id,
            args,
            "Ljava/lang/Object;",
            InvokeHow::Construct,
            single_threaded,
        )
    }

    /// `ArrayType.NewInstance`. Wave 8: the element type comes from the
    /// array class the id names (it came from a table nothing filled, so
    /// every array was an `Object[]` whatever its type said, headed by class 0
    /// when the id was unknown), and the allocation runs on a parked thread
    /// through the collecting allocator — it was a raw `alloc_array` on the
    /// server thread, which no collection waited for and which aborts the VM
    /// on an exhausted heap.
    fn new_array(&self, array_type_id: u64, length: i32) -> Result<u64, BridgeError> {
        use crate::memory::heap::ArrayElementType as E;
        let class = self
            .loaded_class(array_type_id)
            .ok_or(BridgeError::InvalidClass)?;
        let element_type = {
            let cm = self.shared.classes.class_manager.read();
            let name = cm
                .class_store
                .get(class)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            match name.as_bytes() {
                [b'[', b'Z', ..] => E::Boolean,
                [b'[', b'B', ..] => E::Byte,
                [b'[', b'C', ..] => E::Char,
                [b'[', b'S', ..] => E::Short,
                [b'[', b'I', ..] => E::Int,
                [b'[', b'J', ..] => E::Long,
                [b'[', b'F', ..] => E::Float,
                [b'[', b'D', ..] => E::Double,
                [b'[', b'L' | b'[', ..] => E::Reference,
                _ => return Err(BridgeError::InvalidClass),
            }
        };
        let length = usize::try_from(length).map_err(|_| BridgeError::IllegalArgument)?;
        let allocated = run_on_parked_thread(
            &self.shared,
            None,
            move |shared, thread| -> Result<u64, BridgeError> {
                let array = crate::runtime::interpreter::gc_alloc_array(
                    shared,
                    thread,
                    class,
                    element_type,
                    length,
                )
                .map_err(|_| BridgeError::Internal)?;
                // No safepoint between the allocation's return and the export.
                let mut ds = shared.debug.debug_state.lock();
                Ok(export_object(shared, &mut ds, array, ObjectExport::Sent))
            },
        );
        match allocated {
            Ok(id) => id,
            // `Busy` needs a thread named; any idle thread would have done.
            Err(ParkedError::NotParked | ParkedError::Busy) => Err(BridgeError::ThreadNotSuspended),
            Err(ParkedError::Lost) => Err(BridgeError::Internal),
        }
    }
}

