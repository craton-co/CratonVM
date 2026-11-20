// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP ID management.
//!
//! Maps between JVM-internal references and the 64-bit wire IDs that JDWP
//! uses on the protocol.  Every ID type is a simple newtype around `u64`;
//! the [`IdManager`] hands out monotonically-increasing values and keeps a
//! bidirectional mapping so we can go from wire ID back to the internal
//! object.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Wire-level ID newtypes
// ---------------------------------------------------------------------------

/// Opaque object reference sent over JDWP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub u64);

/// Reference-type (class / interface / array) ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReferenceTypeId(pub u64);

/// Thread ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

/// Method ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodId(pub u64);

/// Field ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldId(pub u64);

/// Where heap-object ids start ([`IdManager::fresh_object_id`]). This server
/// sends thread ids, the thread-group id and class ids as small numbers, and
/// JDI keeps one mirror cache for every object id, threads included: an
/// object id that equalled a thread id would come back as that thread's
/// mirror. It is also above every user-space address, so no id minted here
/// equals one of the address-shaped ids sent before wave 7.
pub const HEAP_OBJECT_ID_BASE: u64 = 1 << 48;

// ---------------------------------------------------------------------------
// Id 0 on the wire (interpreter round i1 wave 25, lane L1b)
// ---------------------------------------------------------------------------
//
// JDWP reserves the id 0 for `null` in every id space: JDI maps an object,
// thread or reference-type id of 0 to a null mirror
// (`VirtualMachineImpl.objectMirror` / `referenceType` return null for it).
// This server puts the VM's own ids on the wire, and both of its small id
// spaces start at 0: the main thread is `ThreadId(0)` (`vm_init`), and the
// first class the store defines, `java.lang.Object`, is `ClassId(0)`. So JDI
// saw the main thread as `null` — in `VirtualMachine.allThreads()` and as
// the thread of every event it raised — and `java.lang.Object` as `null`, the
// superclass of every class (the conformance runner's three scenarios failed
// on exactly these three nulls). Inside the server every table keeps the
// VM's ids; the 0 of each space is sent as an id no VM id can take, and read
// back as 0, at the wire only ([`thread_to_wire`] / [`thread_from_wire`],
// [`class_to_wire`] / [`class_from_wire`]); every other id is sent as it is.

/// The wire id of VM thread 0 (the main thread): below the thread-group id
/// (`commands::SYSTEM_THREAD_GROUP_ID`, `HEAP_OBJECT_ID_BASE - 1`) and the
/// heap-object ids, far above any VM thread id.
pub const MAIN_THREAD_WIRE_ID: u64 = HEAP_OBJECT_ID_BASE - 2;

/// The wire id of `ClassId(0)`: outside the `u32` class-id space, so no other
/// class's id can equal it.
pub const CLASS_ZERO_WIRE_ID: u64 = 1 << 32;

/// A VM id no thread or class has: what a wire id of 0 (JDWP's `null`) reads
/// as, so it names nothing (`INVALID_THREAD` / `INVALID_CLASS`) instead of
/// the main thread or `java.lang.Object`.
pub const NO_VM_ID: u64 = u64::MAX;

/// The wire id of VM thread `vm_tid`.
#[inline]
pub fn thread_to_wire(vm_tid: u64) -> u64 {
    if vm_tid == 0 {
        MAIN_THREAD_WIRE_ID
    } else {
        vm_tid
    }
}

/// The VM thread id a wire thread (or object) id names: [`NO_VM_ID`] for
/// `null`.
#[inline]
pub fn thread_from_wire(wire: u64) -> u64 {
    match wire {
        0 => NO_VM_ID,
        MAIN_THREAD_WIRE_ID => 0,
        other => other,
    }
}

/// The wire reference-type id of VM class `raw` (`ClassId::as_u32`).
#[inline]
pub fn class_to_wire(raw: u64) -> u64 {
    if raw == 0 {
        CLASS_ZERO_WIRE_ID
    } else {
        raw
    }
}

/// The VM class id a wire reference-type id names: [`NO_VM_ID`] (which no
/// `u32` class id converts from) for `null`.
#[inline]
pub fn class_from_wire(wire: u64) -> u64 {
    match wire {
        0 => NO_VM_ID,
        CLASS_ZERO_WIRE_ID => 0,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// The DestroyJavaVM thread (interpreter round i5 wave 104, lane L1)
// ---------------------------------------------------------------------------
//
// HotSpot's launcher detaches `main` (`JavaThread::exit`: JVMTI `ThreadEnd`,
// JDWP `ThreadDeath`) and then attaches the same OS thread anew as
// `DestroyJavaVM` (JVMTI `ThreadStart`, JDWP `ThreadStart`): two `Thread`
// objects, two JDWP thread ids. Here both mirrors live on the launcher's one
// registry entry, VM thread 0 (`vm_exec::list_destroy_java_vm_thread`). Once
// `main` has ended (`DebugState::destroy_java_vm_split`), the server gives
// the two their own wire ids: the entry (now `DestroyJavaVM`) goes out as
// [`DESTROY_JAVA_VM_WIRE_ID`], and the debugger's old `main` id,
// [`MAIN_THREAD_WIRE_ID`], names [`ENDED_MAIN_VM_ID`], a VM id no registry
// entry has, which the server answers as the dead `main` (its name, ZOMBIE,
// the null group). Before the split nothing changes.

/// The wire id of VM thread 0 once `main` has ended: the `DestroyJavaVM`
/// thread. Below [`MAIN_THREAD_WIRE_ID`], far above any VM thread id.
pub const DESTROY_JAVA_VM_WIRE_ID: u64 = HEAP_OBJECT_ID_BASE - 3;

/// The VM id the server keeps the ended `main` under once the split is on:
/// [`MAIN_THREAD_WIRE_ID`] itself, which [`thread_to_wire`] sends unchanged
/// (it is not 0), so a table keyed by it and an event carrying it need no
/// rewriting.
pub const ENDED_MAIN_VM_ID: u64 = MAIN_THREAD_WIRE_ID;

/// [`thread_to_wire`] with the `DestroyJavaVM` split (`split`): VM thread 0
/// goes out as [`DESTROY_JAVA_VM_WIRE_ID`].
#[inline]
pub fn thread_to_wire_split(split: bool, vm_tid: u64) -> u64 {
    if split && vm_tid == 0 {
        DESTROY_JAVA_VM_WIRE_ID
    } else {
        thread_to_wire(vm_tid)
    }
}

/// [`thread_from_wire`] with the `DestroyJavaVM` split (`split`):
/// [`DESTROY_JAVA_VM_WIRE_ID`] names VM thread 0 and
/// [`MAIN_THREAD_WIRE_ID`] the ended `main` ([`ENDED_MAIN_VM_ID`]). Without
/// the split [`DESTROY_JAVA_VM_WIRE_ID`] names nothing, as before.
#[inline]
pub fn thread_from_wire_split(split: bool, wire: u64) -> u64 {
    match wire {
        DESTROY_JAVA_VM_WIRE_ID if split => 0,
        MAIN_THREAD_WIRE_ID if split => ENDED_MAIN_VM_ID,
        other => thread_from_wire(other),
    }
}

// ---------------------------------------------------------------------------
// IdManager
// ---------------------------------------------------------------------------

/// Manages the mapping between internal JVM handles and JDWP wire IDs.
///
/// All counters start at 1 (0 is the JDWP "null" sentinel).
///
/// Frame ids are not minted here: a JDWP frame id is
/// `DebugState::frame_id` (the thread's frame generation `<< 32 |` the
/// frame's position in the published listing), validated by membership in
/// that listing (`commands::frame_refusal`), so an id kept across a resume is
/// refused. A second, counter-based frame-id scheme lived here with no
/// production caller until interpreter round i1 wave 29 deleted it
/// (`docs/feature-designs/jvmti-delivery-threading.md`).
pub struct IdManager {
    next_object_id: u64,
    next_ref_type_id: u64,
    next_thread_id: u64,
    next_method_id: u64,
    next_field_id: u64,

    // Forward map: wire-id → internal handle (stored as u64).
    objects: HashMap<u64, u64>,
    ref_types: HashMap<u64, u64>,
    threads: HashMap<u64, u64>,
    methods: HashMap<u64, u64>,
    fields: HashMap<u64, u64>,

    // Reverse map: internal handle → wire-id.
    objects_rev: HashMap<u64, u64>,
    ref_types_rev: HashMap<u64, u64>,
    threads_rev: HashMap<u64, u64>,
    methods_rev: HashMap<u64, u64>,
    fields_rev: HashMap<u64, u64>,
}

impl IdManager {
    pub fn new() -> Self {
        Self {
            next_object_id: 1,
            next_ref_type_id: 1,
            next_thread_id: 1,
            next_method_id: 1,
            next_field_id: 1,
            objects: HashMap::new(),
            ref_types: HashMap::new(),
            threads: HashMap::new(),
            methods: HashMap::new(),
            fields: HashMap::new(),
            objects_rev: HashMap::new(),
            ref_types_rev: HashMap::new(),
            threads_rev: HashMap::new(),
            methods_rev: HashMap::new(),
            fields_rev: HashMap::new(),
        }
    }

    // -- objects -----------------------------------------------------------

    /// Register an internal object handle and return a fresh [`ObjectId`].
    /// If the same handle was already registered the existing ID is returned.
    #[cfg(test)]
    pub fn register_object(&mut self, internal: u64) -> ObjectId {
        if let Some(&id) = self.objects_rev.get(&internal) {
            return ObjectId(id);
        }
        let id = self.next_object_id;
        self.next_object_id += 1;
        self.objects.insert(id, internal);
        self.objects_rev.insert(internal, id);
        ObjectId(id)
    }

    pub fn lookup_object(&self, id: ObjectId) -> Option<u64> {
        self.objects.get(&id.0).copied()
    }

    /// A fresh heap-object id with no mapping here ([`ObjectTable`] keeps
    /// its own): [`HEAP_OBJECT_ID_BASE`] plus the counter
    /// [`Self::register_object`] draws from, so the two never hand out one id
    /// twice.
    pub fn fresh_object_id(&mut self) -> ObjectId {
        let n = self.next_object_id;
        self.next_object_id += 1;
        ObjectId(HEAP_OBJECT_ID_BASE + n)
    }

    // -- reference types ---------------------------------------------------

    pub fn register_ref_type(&mut self, internal: u64) -> ReferenceTypeId {
        if let Some(&id) = self.ref_types_rev.get(&internal) {
            return ReferenceTypeId(id);
        }
        let id = self.next_ref_type_id;
        self.next_ref_type_id += 1;
        self.ref_types.insert(id, internal);
        self.ref_types_rev.insert(internal, id);
        ReferenceTypeId(id)
    }

    pub fn lookup_ref_type(&self, id: ReferenceTypeId) -> Option<u64> {
        self.ref_types.get(&id.0).copied()
    }

    // -- threads -----------------------------------------------------------

    pub fn register_thread(&mut self, internal: u64) -> ThreadId {
        if let Some(&id) = self.threads_rev.get(&internal) {
            return ThreadId(id);
        }
        let id = self.next_thread_id;
        self.next_thread_id += 1;
        self.threads.insert(id, internal);
        self.threads_rev.insert(internal, id);
        ThreadId(id)
    }

    pub fn lookup_thread(&self, id: ThreadId) -> Option<u64> {
        self.threads.get(&id.0).copied()
    }

    /// Return all currently-registered thread wire IDs.
    pub fn all_thread_ids(&self) -> Vec<ThreadId> {
        self.threads.keys().copied().map(ThreadId).collect()
    }

    // -- methods -----------------------------------------------------------

    pub fn register_method(&mut self, internal: u64) -> MethodId {
        if let Some(&id) = self.methods_rev.get(&internal) {
            return MethodId(id);
        }
        let id = self.next_method_id;
        self.next_method_id += 1;
        self.methods.insert(id, internal);
        self.methods_rev.insert(internal, id);
        MethodId(id)
    }

    pub fn lookup_method(&self, id: MethodId) -> Option<u64> {
        self.methods.get(&id.0).copied()
    }

    // -- fields ------------------------------------------------------------

    pub fn register_field(&mut self, internal: u64) -> FieldId {
        if let Some(&id) = self.fields_rev.get(&internal) {
            return FieldId(id);
        }
        let id = self.next_field_id;
        self.next_field_id += 1;
        self.fields.insert(id, internal);
        self.fields_rev.insert(internal, id);
        FieldId(id)
    }

    pub fn lookup_field(&self, id: FieldId) -> Option<u64> {
        self.fields.get(&id.0).copied()
    }
}

// ---------------------------------------------------------------------------
// ObjectTable — JDWP object ids for heap objects (interpreter round i1 wave 7)
// ---------------------------------------------------------------------------

/// How an object id reaches the debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectExport {
    /// Sent in a reply or an event now: counted, and released only by
    /// `VirtualMachine.DisposeObjects` (JDI disposes the times it received
    /// an id) or the end of the session.
    Sent,
    /// Written into a suspended thread's published frame snapshot: held while
    /// the snapshot stands ([`ObjectTable::unpin`] on resume), and counted as
    /// sent only when a `StackFrame` command actually hands it out.
    Pinned,
}

/// One exported object.
#[derive(Debug, Clone, Copy)]
struct ObjectEntry {
    /// The VM-side handle that tracks the referent's moves: a JNI WEAK global
    /// reference (wave 8), or a strong one while `disabled > 0`.
    handle: u64,
    /// The referent's address as of [`ObjectTable::addr_epoch`];
    /// [`COLLECTED`] once the referent is known to be gone.
    addr: usize,
    /// Times the id was sent, net of `DisposeObjects`.
    sent: u64,
    /// Published frame snapshots naming it.
    pins: u32,
    /// `ObjectReference.DisableCollection` count net of `EnableCollection`
    /// (JDWP nests them). While it is non-zero the handle is strong.
    disabled: u32,
}

/// [`ObjectEntry::addr`] of an entry whose referent was collected: no object
/// starts at address 0, so the address index never maps it.
const COLLECTED: usize = 0;

/// The JDWP `objectID` table: an id names one heap object for as long as the
/// debugger may use it, however often a collection moves the object. What
/// HotSpot's back end keeps in `commonRef.c`.
///
/// Until wave 7 an id was the object's current address, so a moving
/// collection turned every id a debugger held into a name for nothing, or
/// for whichever object later started at that address. Now an id maps to a
/// VM handle that the collector updates; this table is pure bookkeeping (the
/// VM side, `debug::export_object` / `debug::object_for_id`, owns the handles
/// and hands the ones to free back through [`Self::take_released`]).
///
/// The handles are WEAK (wave 8), as HotSpot's back end keeps them: an id
/// does not keep its object alive, so a collection may take it
/// (`ObjectReference.IsCollected` then answers `true` and every other command
/// `INVALID_OBJECT`); the entry itself stays until the debugger disposes of the
/// id or detaches. `ObjectReference.DisableCollection` makes the handle strong
/// until the matching `EnableCollection` ([`Self::disable_collection`]). Until
/// wave 8 every handle was strong, so an object the debugger had printed
/// could never be collected while the session lasted; see
/// docs/feature-designs/jvmti-delivery-threading.md.
///
/// The same object always gets the same id (JDI compares mirrors by id):
/// [`Self::lookup_addr`] answers by current address, and the address index is
/// rebuilt ([`Self::rekey`]) after any collection, which may have moved every
/// referent.
pub struct ObjectTable {
    entries: HashMap<u64, ObjectEntry>,
    /// Current address → id, valid while the VM's collection count equals
    /// `addr_epoch`.
    by_addr: HashMap<usize, u64>,
    addr_epoch: u64,
    /// Handles of entries that are gone, for the VM side to free.
    released: Vec<u64>,
}

impl ObjectTable {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            by_addr: HashMap::new(),
            addr_epoch: 0,
            released: Vec::new(),
        }
    }

    /// Number of live ids.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no id is live.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `id` names a live entry.
    pub fn contains(&self, id: u64) -> bool {
        self.entries.contains_key(&id)
    }

    /// The VM handle behind `id`, if it is live.
    pub fn handle_of(&self, id: u64) -> Option<u64> {
        self.entries.get(&id).map(|e| e.handle)
    }

    /// Does the address index predate collection `epoch`?
    pub fn needs_rekey(&self, epoch: u64) -> bool {
        epoch != self.addr_epoch
    }

    /// Rebuild the address index for collection `epoch`: `resolve` answers a
    /// handle's current referent address (`None`: the referent was collected;
    /// the entry stays, marked collected, until the debugger disposes of the
    /// id — JDWP `IsCollected` must be able to say so).
    pub fn rekey(&mut self, epoch: u64, resolve: impl Fn(u64) -> Option<usize>) {
        self.by_addr.clear();
        for (&id, entry) in self.entries.iter_mut() {
            match resolve(entry.handle) {
                Some(addr) if addr != COLLECTED => {
                    entry.addr = addr;
                    self.by_addr.insert(addr, id);
                }
                _ => entry.addr = COLLECTED,
            }
        }
        self.addr_epoch = epoch;
    }

    /// The id already naming the object at `addr`, and its handle, WITHOUT
    /// counting an export: the caller confirms the handle still names that
    /// address (a weak referent can die, and its address be reused, between
    /// two rekeys) and then counts the export with [`Self::note_export`], or
    /// drops the stale index entry with [`Self::forget_addr`]. Same validity
    /// rule as [`Self::lookup_addr`].
    pub fn candidate_for_addr(&self, addr: usize) -> Option<(u64, u64)> {
        let id = *self.by_addr.get(&addr)?;
        let entry = self.entries.get(&id)?;
        Some((id, entry.handle))
    }

    /// Count one export of `id` (see [`Self::candidate_for_addr`]).
    pub fn note_export(&mut self, id: u64, how: ObjectExport) {
        if let Some(entry) = self.entries.get_mut(&id) {
            match how {
                ObjectExport::Sent => entry.sent += 1,
                ObjectExport::Pinned => entry.pins += 1,
            }
        }
    }

    /// `id`'s referent is gone (its handle no longer names the address the
    /// index has for it): unindex it and mark it collected.
    pub fn forget_addr(&mut self, id: u64) {
        if let Some(entry) = self.entries.get_mut(&id) {
            if self.by_addr.get(&entry.addr) == Some(&id) {
                self.by_addr.remove(&entry.addr);
            }
            entry.addr = COLLECTED;
        }
    }

    /// `ObjectReference.DisableCollection`: the new count (`None`: unknown
    /// id). The caller makes the handle strong when this answers 1.
    pub fn disable_collection(&mut self, id: u64) -> Option<u32> {
        let entry = self.entries.get_mut(&id)?;
        entry.disabled = entry.disabled.saturating_add(1);
        Some(entry.disabled)
    }

    /// `ObjectReference.EnableCollection`: the new count (`None`: unknown id;
    /// a count already at zero stays there). The caller makes the handle weak
    /// again when this answers 0 after a non-zero count.
    pub fn enable_collection(&mut self, id: u64) -> Option<u32> {
        let entry = self.entries.get_mut(&id)?;
        entry.disabled = entry.disabled.saturating_sub(1);
        Some(entry.disabled)
    }

    /// Point `id` at a new VM handle (weak ↔ strong); the old one goes to
    /// [`Self::take_released`]. No-op for an unknown id.
    pub fn replace_handle(&mut self, id: u64, handle: u64) {
        if let Some(entry) = self.entries.get_mut(&id) {
            let old = std::mem::replace(&mut entry.handle, handle);
            self.released.push(old);
        }
    }

    /// The id of the object at `addr`, counting the export. Only valid right
    /// after [`Self::needs_rekey`] answered `false` (or [`Self::rekey`] ran)
    /// for the current collection count.
    #[cfg(test)]
    pub fn lookup_addr(&mut self, addr: usize, how: ObjectExport) -> Option<u64> {
        let id = *self.by_addr.get(&addr)?;
        let entry = self.entries.get_mut(&id)?;
        match how {
            ObjectExport::Sent => entry.sent += 1,
            ObjectExport::Pinned => entry.pins += 1,
        }
        Some(id)
    }

    /// Record a new id for the object at `addr` held by `handle` (as of
    /// collection `epoch`, which [`Self::rekey`] established).
    pub fn insert(&mut self, id: u64, handle: u64, addr: usize, how: ObjectExport) {
        let (sent, pins) = match how {
            ObjectExport::Sent => (1, 0),
            ObjectExport::Pinned => (0, 1),
        };
        self.entries.insert(
            id,
            ObjectEntry {
                handle,
                addr,
                sent,
                pins,
                disabled: 0,
            },
        );
        self.by_addr.insert(addr, id);
    }

    /// Count one more send of an id a snapshot pinned (a `StackFrame` command
    /// handed it out). No-op for an id this table does not know.
    pub fn note_sent(&mut self, id: u64) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.sent += 1;
        }
    }

    /// Drop one snapshot pin of `id`; the entry goes once it was never sent.
    pub fn unpin(&mut self, id: u64) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.pins = entry.pins.saturating_sub(1);
        }
        self.release_if_unused(id);
    }

    /// `VirtualMachine.DisposeObjects`: the debugger dropped `ref_count`
    /// references to `id`. The entry goes when none is left and no snapshot
    /// pins it; a later export of the object mints a new id, as in HotSpot.
    pub fn dispose(&mut self, id: u64, ref_count: u32) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.sent = entry.sent.saturating_sub(u64::from(ref_count));
        }
        self.release_if_unused(id);
    }

    fn release_if_unused(&mut self, id: u64) {
        let unused = self
            .entries
            .get(&id)
            .is_some_and(|e| e.sent == 0 && e.pins == 0);
        if unused {
            if let Some(entry) = self.entries.remove(&id) {
                if self.by_addr.get(&entry.addr) == Some(&id) {
                    self.by_addr.remove(&entry.addr);
                }
                self.released.push(entry.handle);
            }
        }
    }

    /// Forget every id (the debugger detached); their handles go to
    /// [`Self::take_released`].
    pub fn release_all(&mut self) {
        self.by_addr.clear();
        self.released
            .extend(self.entries.drain().map(|(_, entry)| entry.handle));
    }

    /// Handles whose entries are gone, for the VM side to free.
    pub fn take_released(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.released)
    }
}

impl Default for ObjectTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
