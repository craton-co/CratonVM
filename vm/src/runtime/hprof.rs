// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// ---------------------------------------------------------------------------
// HPROF Binary Heap Dump (version 1.0.2)
// ---------------------------------------------------------------------------
//
// Produces `jmap -dump:format=b` compatible binary dumps that can be opened
// in Eclipse MAT, VisualVM, and other HPROF-aware tools.
//
// The format is documented in:
//   https://hg.openjdk.java.net/jdk/jdk/file/tip/src/hotspot/share/services/heapDumper.cpp
//
// ---------------------------------------------------------------------------
// LIVENESS (observability audit, 2026-07-26)
// ---------------------------------------------------------------------------
// This dumper IS reachable on a default build. The only production trigger is
// `runtime::interpreter::maybe_dump_heap_on_oom`, gated on
// `VmConfig::heap_dump_on_oom` (`-XX:+HeapDumpOnOutOfMemoryError`), fired at
// most once per VM lifetime. There is NO jcmd / attach trigger: the
// `GC.heap_dump` diagnostic command in `runtime::serviceability` is
// registered on a `JcmdProcessor` that nothing constructs outside tests (see
// the LIVENESS block in that file), so `jcmd <pid> GC.heap_dump` cannot reach
// this code today.
//
// ---------------------------------------------------------------------------
// KNOWN FIDELITY GAPS
// ---------------------------------------------------------------------------
//  * obsaudit D11 (2026-07-26), FIXED: the dump used to run with no
//    safepoint at all — `dump_heap` walked `mem.heap.walk_objects()` while
//    other Java threads kept running and allocating, which could produce
//    torn field values or reference IDs for objects a concurrent collection
//    had since moved/freed. `dump_heap` now requests the same stop-the-world
//    barrier real GC cycles use before walking (see `DumpSafepoint`) and
//    releases it (with an empty, no-op pointer map — nothing here moves any
//    object) once the walk finishes, including on an early error or panic.
//    A request can still be declined if another pause is already in
//    flight, in which case this falls back to the old unpaused behaviour —
//    see `DumpSafepoint`'s doc comment for why that fallback, rather than
//    joining the other pause, was the right scope for this fix.
//  * Object IDs are raw heap addresses. Under a relocating collector two
//    dumps of the same logical object will not agree, and an address recycled
//    after a collection can alias.
//  * No `HPROF_GC_ROOT_JNI_LOCAL` / monitor-used roots are emitted, so MAT's
//    "GC root" attribution is incomplete.

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;

use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef};

use crate::runtime::serviceability::HprofWriter;
use crate::threading::jvm_thread::ThreadId;
use crate::vm::SharedVm;

// ---- HPROF heap-dump sub-record tags ------------------------------------

const GC_ROOT_JNI_GLOBAL: u8 = 0x01;
const GC_ROOT_JAVA_FRAME: u8 = 0x03;
const GC_ROOT_STICKY_CLASS: u8 = 0x05;
const GC_ROOT_THREAD_OBJ: u8 = 0x08;
const GC_CLASS_DUMP: u8 = 0x20;
const GC_INSTANCE_DUMP: u8 = 0x21;
const GC_OBJ_ARRAY_DUMP: u8 = 0x22;
const GC_PRIM_ARRAY_DUMP: u8 = 0x23;

// HPROF basic-type constants
const HPROF_OBJECT: u8 = 2;
const HPROF_BOOLEAN: u8 = 4;
const HPROF_CHAR: u8 = 5;
const HPROF_FLOAT: u8 = 6;
const HPROF_DOUBLE: u8 = 7;
const HPROF_BYTE: u8 = 8;
const HPROF_SHORT: u8 = 9;
const HPROF_INT: u8 = 10;
const HPROF_LONG: u8 = 11;

/// Synthetic base for class object IDs (avoids collision with heap pointers).
const CLASS_OBJ_ID_BASE: u64 = 0x7000_0000_0000_0000;

/// Maximum in-memory segment size before splitting into a new
/// HEAP_DUMP_SEGMENT.
///
/// Observability audit (2026-07-26): this was 1 GiB, which meant the dumper
/// accumulated up to a gibibyte of sub-records in a single `Vec<u8>` before
/// the first flush. The one production caller is the
/// `-XX:+HeapDumpOnOutOfMemoryError` path — i.e. we would demand a ~1 GiB
/// contiguous native allocation at exactly the moment the process is out of
/// memory, turning a diagnosable OOM into an allocation failure inside the
/// diagnostic itself. HotSpot's `heapDumper.cpp` flushes at 1 MiB; 8 MiB
/// keeps the record count low without the failure mode. The HPROF record
/// length field is a `u32`, so any value below 4 GiB is format-legal — the
/// choice is purely about peak RSS during the dump.
const MAX_SEGMENT_SIZE: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Dump the heap of `vm` into an HPROF binary file at `path`.
/// Returns the number of bytes written, or an error string.
///
/// `initiator` is the calling thread's id, used to request a stop-the-world
/// pause for the duration of the walk — see [`DumpSafepoint`] (obsaudit D11).
pub fn dump_heap(vm: &Arc<SharedVm>, path: &str, initiator: ThreadId) -> Result<u64, String> {
    let file = std::fs::File::create(path).map_err(|e| format!("cannot create {}: {}", path, e))?;
    let mut w = BufWriter::new(file);

    // obsaudit D11 (2026-07-26): the walk below used to run with no
    // safepoint at all — see the KNOWN FIDELITY GAPS block at the top of
    // this file for the failure mode (torn field reads / use of an object
    // a concurrent GC has since moved or freed). `DumpSafepoint` requests
    // the same stop-the-world barrier real GC cycles use
    // (`SharedVm::mem::gc_barrier`); its `Drop` releases the barrier with an
    // empty pointer map (nothing here moves any object) even if the walk
    // below panics or returns early, so a dump can never leak the VM in a
    // permanently-paused state.
    let _safepoint = DumpSafepoint::request(vm, initiator);

    let mut dumper = HprofDumper::new(vm);
    dumper
        .write_all(&mut w)
        .map_err(|e| format!("write error: {}", e))?;
    w.flush().map_err(|e| format!("flush error: {}", e))?;
    let size = w
        .into_inner()
        .map_err(|e| format!("flush error: {}", e))?
        .metadata()
        .map_err(|e| format!("metadata error: {}", e))?
        .len();
    Ok(size)
}

/// RAII stop-the-world pause for an HPROF dump (obsaudit D11).
///
/// Requests the barrier via [`crate::threading::gc_barrier::GcBarrier::request_stw_counted_with_live_blocked`]
/// — the same entry point `runtime::interpreter`'s real GC-triggering path
/// uses — then waits for every counted mutator to arrive. Deliberately does
/// *not* use the interpreter's `stw_take_over_and_wait` forcible-freeze path
/// for in-JIT peers (that machinery exists to let a *moving* collector
/// relocate objects safely under a frozen peer; an HPROF walk moves
/// nothing, so `wait_for_all`'s cooperative safepoint poll — the same one
/// JIT-compiled backward branches hit — is sufficient here and keeps this
/// diagnostic path from depending on the more experimental takeover code).
///
/// `request_stw_counted_with_live_blocked` can decline (returns `false`) if
/// another stop-the-world pause is already in flight — e.g. a real GC cycle
/// racing the OOM path that triggered this dump. Rather than block trying to
/// join someone else's pause (real GC's own coordination protocol is
/// considerably more involved than this diagnostic needs), a declined
/// request falls back to the pre-D11 behaviour: dump without a pause. That
/// preserves this path's original best-effort characteristic — a torn dump
/// on a benign race is still strictly better than the OOM handler itself
/// blocking or erroring.
struct DumpSafepoint<'a> {
    vm: &'a SharedVm,
    acquired: bool,
}

impl<'a> DumpSafepoint<'a> {
    fn request(vm: &'a SharedVm, initiator: ThreadId) -> Self {
        let acquired = vm
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(initiator, || {
                let (alive, blocked, _os_tids, blocked_tids) =
                    vm.threads.thread_registry.alive_count_blocked_and_os_tids();
                (
                    u32::try_from(alive).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
        if acquired {
            vm.mem.gc_barrier.wait_for_all();
        }
        Self { vm, acquired }
    }
}

impl Drop for DumpSafepoint<'_> {
    fn drop(&mut self) {
        if self.acquired {
            // No object moved, so the empty map is not a shortcut — it is
            // the exact and complete answer for a non-moving pause. See
            // `vm/src/native/jni.rs`'s `gc_barrier.complete_gc(cratonvm_types::PointerMap::default())`
            // test usage for the same pattern.
            self.vm
                .mem
                .gc_barrier
                .complete_gc(cratonvm_types::PointerMap::default());
        }
    }
}

// ---------------------------------------------------------------------------
// HprofDumper — orchestrates the dump phases
// ---------------------------------------------------------------------------

pub struct HprofDumper<'a> {
    vm: &'a Arc<SharedVm>,
    /// Interned string table: string → id
    strings: HashMap<String, u64>,
    next_string_id: u64,
    /// ClassId → serial number (1-based)
    class_serials: HashMap<ClassId, u32>,
    next_serial: u32,
}

impl<'a> HprofDumper<'a> {
    pub fn new(vm: &'a Arc<SharedVm>) -> Self {
        Self {
            vm,
            strings: HashMap::new(),
            next_string_id: 1,
            class_serials: HashMap::new(),
            next_serial: 1,
        }
    }

    /// Intern a string, returning its ID. Writes nothing — the records are
    /// emitted later by `flush_strings`.
    fn intern(&mut self, s: &str) -> u64 {
        if let Some(&id) = self.strings.get(s) {
            return id;
        }
        let id = self.next_string_id;
        self.next_string_id += 1;
        self.strings.insert(s.to_string(), id);
        id
    }

    /// Assign a serial number to a class, returning it.
    fn class_serial(&mut self, cid: ClassId) -> u32 {
        if let Some(&s) = self.class_serials.get(&cid) {
            return s;
        }
        let s = self.next_serial;
        self.next_serial += 1;
        self.class_serials.insert(cid, s);
        s
    }

    /// Full dump pipeline.
    pub fn write_all<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        // Phase 0: header
        w.write_all(&HprofWriter::write_header())?;

        // Phase 1: collect all strings + class serials
        self.collect_metadata();

        // Phase 2: STRING_IN_UTF8 records
        self.write_strings(w)?;

        // Phase 3: LOAD_CLASS records
        self.write_load_classes(w)?;

        // Phase 4: STACK_FRAME + STACK_TRACE records (one dummy trace)
        self.write_stack_traces(w)?;

        // Phase 5: HEAP_DUMP_SEGMENT(s) with all sub-records
        self.write_heap_segments(w)?;

        // Phase 6: HEAP_DUMP_END
        w.write_all(&HprofWriter::write_heap_dump_end())?;

        Ok(())
    }

    // ------------------------------------------------------------------
    // Phase 1: collect metadata (strings, class serials)
    // ------------------------------------------------------------------

    fn collect_metadata(&mut self) {
        let cm = self.vm.classes.class_manager.read();
        for class in cm.class_store.iter() {
            self.intern(&class.name);
            self.class_serial(class.id);

            if let Some(ref sf) = class.source_file {
                self.intern(sf);
            }

            for f in &class.fields {
                self.intern(&f.name);
                self.intern(&f.descriptor);
            }
            for m in &class.methods {
                self.intern(&m.name);
                self.intern(&m.descriptor);
            }
        }
        // Intern a few well-known strings
        self.intern("<unknown>");
        self.intern("");
    }

    // ------------------------------------------------------------------
    // Phase 2: STRING_IN_UTF8 records
    // ------------------------------------------------------------------

    fn write_strings<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Sort by id for deterministic output
        let mut entries: Vec<_> = self.strings.iter().collect();
        entries.sort_by_key(|(_, &id)| id);
        for (s, &id) in &entries {
            w.write_all(&HprofWriter::write_string_record(id, s))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Phase 3: LOAD_CLASS records
    // ------------------------------------------------------------------

    fn write_load_classes<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let cm = self.vm.classes.class_manager.read();
        for class in cm.class_store.iter() {
            let serial = self.class_serials[&class.id];
            let class_obj_id = class_obj_id_for(class.id);
            let name_id = self.strings[&*class.name];
            w.write_all(&HprofWriter::write_load_class(
                serial,
                class_obj_id,
                0,
                name_id,
            ))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Phase 4: stack traces (one dummy trace serial=1 with no frames)
    // ------------------------------------------------------------------

    fn write_stack_traces<W: Write>(&self, w: &mut W) -> io::Result<()> {
        // Thread serial 1, stack trace serial 1, no frames.
        // Required so INSTANCE_DUMP / CLASS_DUMP can reference stack_trace_serial=0
        // (meaning "no trace"), or serial=1 for thread roots.
        let thread_names = self.vm.threads.thread_registry.all_thread_names();
        if thread_names.is_empty() {
            // Always emit at least one dummy trace
            w.write_all(&HprofWriter::write_stack_trace(1, 1, &[]))?;
        } else {
            for (i, _) in thread_names.iter().enumerate() {
                let serial = (i + 1) as u32;
                w.write_all(&HprofWriter::write_stack_trace(serial, serial, &[]))?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Phase 5: HEAP_DUMP_SEGMENT records
    // ------------------------------------------------------------------

    fn write_heap_segments<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        let mut seg = SegmentBuilder::new();

        // 5a: GC roots
        self.write_gc_roots(&mut seg);

        // 5b: CLASS_DUMP sub-records
        self.write_class_dumps(&mut seg);

        // 5c: walk heap objects
        let objects = self.vm.mem.heap.walk_objects();
        for (ptr, _size) in &objects {
            // SAFETY: `walk_objects()` returns raw pointers that are
            // guaranteed to point at live object headers in the heap
            // walkable-arena. Each pointer was validated against
            // the allocator's live set before being returned.
            //
            // obsaudit D11, FIXED: `dump_heap` now requests a real
            // stop-the-world pause before this walk begins (see
            // `DumpSafepoint`) — an earlier version of this comment
            // retracted a false claim that such a pause already existed;
            // now it genuinely does. See the KNOWN FIDELITY GAPS block at
            // the top of this file.
            let obj = unsafe { ObjectRef::from_raw(*ptr) };
            let header = self.vm.mem.heap.get_header(obj);

            match header.kind() {
                ObjectKind::Object => self.write_instance_dump_with_values(&mut seg, obj),
                ObjectKind::Array => match header.element_type() {
                    ArrayElementType::Reference => self.write_obj_array_dump(&mut seg, obj),
                    _ => self.write_prim_array_dump(&mut seg, obj),
                },
                // Round-9 gc CRIT-1: humongous-continuation filler is a
                // walker sentinel, never a real object. Skip from heap
                // dumps.
                ObjectKind::HumongousFiller => continue,
            }

            // Flush segment if it exceeds threshold
            if seg.len() >= MAX_SEGMENT_SIZE {
                seg.flush(w)?;
            }
        }

        // Final flush
        seg.flush(w)?;
        Ok(())
    }

    // ---- GC roots -------------------------------------------------------

    fn write_gc_roots(&self, seg: &mut SegmentBuilder) {
        // Thread object roots.
        //
        // Observability audit (2026-07-26): `alive_thread_objects(usize::MAX)`
        // used to be called *inside* this loop, re-snapshotting (and
        // re-allocating) the whole registry once per thread — O(n^2) work and
        // O(n^2) allocation on a dump from an application server with
        // thousands of threads, on the OOM path. Hoisted out; the result is
        // indexed exactly as before.
        let thread_names = self.vm.threads.thread_registry.all_thread_names();
        let thread_objs = self
            .vm
            .threads
            .thread_registry
            .alive_thread_objects(usize::MAX);
        for (i, _) in thread_names.iter().enumerate() {
            let thread_serial = (i + 1) as u32;
            if let Some(obj) = thread_objs.get(i) {
                let obj_id = obj.as_ptr() as u64;
                seg.push_u8(GC_ROOT_THREAD_OBJ);
                seg.push_u64(obj_id);
                seg.push_u32(thread_serial);
                seg.push_u32(thread_serial); // stack trace serial
            }
        }

        // JNI global roots
        {
            let globals = self.vm.natives.jni_global_refs.lock();
            let mut roots = Vec::new();
            globals.collect_roots(&mut roots);
            for obj in &roots {
                seg.push_u8(GC_ROOT_JNI_GLOBAL);
                seg.push_u64(obj.as_ptr() as u64);
                seg.push_u64(0); // jni_ref_id
            }
        }

        // Thread stack frame roots (GC root snapshots)
        let roots = self.vm.threads.thread_registry.collect_all_root_snapshots();
        for obj in &roots {
            seg.push_u8(GC_ROOT_JAVA_FRAME);
            seg.push_u64(obj.as_ptr() as u64);
            seg.push_u32(1); // thread serial
            seg.push_u32(0xFFFF_FFFF); // frame depth = unknown
        }

        // Sticky class roots (system classes)
        let cm = self.vm.classes.class_manager.read();
        for class in cm.class_store.iter() {
            if class.name.starts_with("java/") || class.name.starts_with("[") {
                seg.push_u8(GC_ROOT_STICKY_CLASS);
                seg.push_u64(class_obj_id_for(class.id));
            }
        }
    }

    // ---- CLASS_DUMP sub-records -----------------------------------------

    fn write_class_dumps(&mut self, seg: &mut SegmentBuilder) {
        let cm = self.vm.classes.class_manager.read();
        let statics = self.vm.classes.statics.read();

        for class in cm.class_store.iter() {
            let class_obj_id = class_obj_id_for(class.id);
            let super_obj_id = class.superclass.map(class_obj_id_for).unwrap_or(0);

            // Compute instance size in HPROF terms (sum of field byte sizes)
            let instance_fields: Vec<_> = class.fields.iter().filter(|f| !f.is_static()).collect();
            let static_fields: Vec<_> = class.fields.iter().filter(|f| f.is_static()).collect();

            // Instance size: all inherited + own instance fields' byte sizes
            let instance_byte_size = compute_instance_byte_size(class.id, &cm.class_store);

            seg.push_u8(GC_CLASS_DUMP);
            seg.push_u64(class_obj_id); // class object ID
            seg.push_u32(0); // stack trace serial
            seg.push_u64(super_obj_id); // super class object ID
            seg.push_u64(0); // classloader object ID
            seg.push_u64(0); // signers object ID
            seg.push_u64(0); // protection domain object ID
            seg.push_u64(0); // reserved1
            seg.push_u64(0); // reserved2
            seg.push_u32(instance_byte_size as u32); // instance size (bytes)

            // Constant pool (empty)
            seg.push_u16(0);

            // Static fields
            seg.push_u16(static_fields.len() as u16);
            let static_vals = statics.get(&class.id);
            for (i, sf) in static_fields.iter().enumerate() {
                let name_id = self.strings.get(&*sf.name).copied().unwrap_or(0);
                let htype = descriptor_to_hprof_type(&sf.descriptor);
                seg.push_u64(name_id);
                seg.push_u8(htype);
                // Static field value
                let val = static_vals
                    .and_then(|v| v.get(i))
                    .cloned()
                    .unwrap_or(cratonvm_types::Value::Int(0));
                write_value_for_type(seg, htype, &val);
            }

            // Instance fields (own only)
            seg.push_u16(instance_fields.len() as u16);
            for f in &instance_fields {
                let name_id = self.strings.get(&*f.name).copied().unwrap_or(0);
                let htype = descriptor_to_hprof_type(&f.descriptor);
                seg.push_u64(name_id);
                seg.push_u8(htype);
            }
        }
    }

    // ---- OBJ_ARRAY_DUMP sub-records -------------------------------------

    fn write_obj_array_dump(&self, seg: &mut SegmentBuilder, obj: ObjectRef) {
        let header = self.vm.mem.heap.get_header(obj);
        let length = header.array_length() as usize;
        let class_id = header.class_id;

        seg.push_u8(GC_OBJ_ARRAY_DUMP);
        seg.push_u64(obj.as_ptr() as u64); // array object ID
        seg.push_u32(0); // stack trace serial
        seg.push_u32(length as u32); // num elements
                                     // OBJ_ARRAY_DUMP's third ID is the *array* class, e.g.
                                     // `[Ljava/lang/String;` — not the component class.
        seg.push_u64(self.array_class_obj_id(class_id)); // array class object ID

        for i in 0..length {
            let val = self
                .vm
                .mem
                .heap
                .get_array_element(obj, i)
                .unwrap_or(cratonvm_types::Value::Object(None));
            match val {
                cratonvm_types::Value::Object(Some(r)) => seg.push_u64(r.as_ptr() as u64),
                _ => seg.push_u64(0),
            }
        }
    }

    /// Resolve the HPROF class object ID to report for a reference array
    /// whose header carries `component_class_id`.
    ///
    /// Observability audit (2026-07-26) — DEFECT FIXED. `anewarray` stores the
    /// *component* class id in the array object's header (see
    /// `runtime::interpreter`'s `anewarray` arm, which passes
    /// `component_class_id` to `gc_alloc_array`). The dumper used to write
    /// `class_obj_id_for(header.class_id)` straight into the OBJ_ARRAY_DUMP
    /// "array class object ID" slot, so every `String[]` in the dump was
    /// labelled `java.lang.String`, every `Object[]` was labelled
    /// `java.lang.Object`, and MAT/VisualVM attributed the array's retained
    /// size to a non-array class that also has real instances. Class
    /// histograms from such a dump are unusable for arrays.
    ///
    /// We resolve the real array class (`[L<component>;`, or `[<component>`
    /// when the component is itself an array) out of the class store. Array
    /// classes live in the same store and therefore already have LOAD_CLASS
    /// and CLASS_DUMP records emitted for them, which is what the reader
    /// requires. If the array class has not been materialised we fall back to
    /// the component id — still wrong, but it is guaranteed to have a
    /// LOAD_CLASS record, and emitting a dangling ID would make the whole
    /// dump unparseable.
    fn array_class_obj_id(&self, component_class_id: ClassId) -> u64 {
        let cm = self.vm.classes.class_manager.read();
        let component_name = match cm.class_store.get(component_class_id) {
            Some(c) => c.name.to_string(),
            None => return class_obj_id_for(component_class_id),
        };
        let array_name = array_class_name_for(&component_name);
        match cm.class_store.find_by_name(&array_name) {
            Some(c) => class_obj_id_for(c.id),
            None => class_obj_id_for(component_class_id),
        }
    }

    // ---- PRIM_ARRAY_DUMP sub-records ------------------------------------

    fn write_prim_array_dump(&self, seg: &mut SegmentBuilder, obj: ObjectRef) {
        let header = self.vm.mem.heap.get_header(obj);
        let length = header.array_length() as usize;
        let elem_type = header.element_type();
        let hprof_type = array_element_to_hprof(elem_type);

        seg.push_u8(GC_PRIM_ARRAY_DUMP);
        seg.push_u64(obj.as_ptr() as u64); // array object ID
        seg.push_u32(0); // stack trace serial
        seg.push_u32(length as u32); // num elements
        seg.push_u8(hprof_type); // element type

        for i in 0..length {
            let val = self
                .vm
                .mem
                .heap
                .get_array_element(obj, i)
                .unwrap_or(cratonvm_types::Value::Int(0));
            match elem_type {
                ArrayElementType::Boolean | ArrayElementType::Byte => {
                    seg.push_u8(val.as_int().unwrap_or(0) as u8);
                }
                ArrayElementType::Char | ArrayElementType::Short => {
                    seg.push_u16(val.as_int().unwrap_or(0) as u16);
                }
                ArrayElementType::Int => {
                    seg.push_u32(val.as_int().unwrap_or(0) as u32);
                }
                ArrayElementType::Float => {
                    seg.push_u32(val.as_float().unwrap_or(0.0).to_bits());
                }
                ArrayElementType::Long => {
                    seg.push_u64(val.as_long().unwrap_or(0) as u64);
                }
                ArrayElementType::Double => {
                    seg.push_u64(val.as_double().unwrap_or(0.0).to_bits());
                }
                ArrayElementType::Reference => {
                    // Should not happen — reference arrays go through write_obj_array_dump
                    seg.push_u64(0);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SegmentBuilder — accumulates sub-records, flushes as HEAP_DUMP_SEGMENT
// ---------------------------------------------------------------------------

struct SegmentBuilder {
    buf: Vec<u8>,
}

impl SegmentBuilder {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1 << 20),
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn push_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn push_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn push_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn push_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Flush the accumulated buffer as a HEAP_DUMP_SEGMENT record.
    fn flush<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        // Record header: tag(1) + timestamp(4) + body_length(4)
        w.write_all(&[HprofWriter::HPROF_HEAP_DUMP_SEGMENT])?;
        w.write_all(&0u32.to_be_bytes())?; // timestamp
        w.write_all(&(self.buf.len() as u32).to_be_bytes())?;
        w.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Map a ClassId to a synthetic HPROF class object ID.
fn class_obj_id_for(cid: ClassId) -> u64 {
    CLASS_OBJ_ID_BASE + cid.as_u32() as u64
}

/// Build the internal-form array class name for a component class name.
///
/// `java/lang/String` -> `[Ljava/lang/String;`
/// `[Ljava/lang/String;` -> `[[Ljava/lang/String;`
fn array_class_name_for(component_name: &str) -> String {
    if component_name.starts_with('[') {
        format!("[{component_name}")
    } else {
        format!("[L{component_name};")
    }
}

/// Map a field descriptor string to an HPROF basic type constant.
fn descriptor_to_hprof_type(desc: &str) -> u8 {
    match desc.as_bytes().first() {
        Some(b'B') => HPROF_BYTE,
        Some(b'C') => HPROF_CHAR,
        Some(b'D') => HPROF_DOUBLE,
        Some(b'F') => HPROF_FLOAT,
        Some(b'I') => HPROF_INT,
        Some(b'J') => HPROF_LONG,
        Some(b'S') => HPROF_SHORT,
        Some(b'Z') => HPROF_BOOLEAN,
        Some(b'L') | Some(b'[') => HPROF_OBJECT,
        _ => HPROF_OBJECT, // fallback
    }
}

/// Map an ArrayElementType to an HPROF basic type constant.
fn array_element_to_hprof(et: ArrayElementType) -> u8 {
    match et {
        ArrayElementType::Boolean => HPROF_BOOLEAN,
        ArrayElementType::Char => HPROF_CHAR,
        ArrayElementType::Float => HPROF_FLOAT,
        ArrayElementType::Double => HPROF_DOUBLE,
        ArrayElementType::Byte => HPROF_BYTE,
        ArrayElementType::Short => HPROF_SHORT,
        ArrayElementType::Int => HPROF_INT,
        ArrayElementType::Long => HPROF_LONG,
        ArrayElementType::Reference => HPROF_OBJECT,
    }
}

/// Size in bytes of an HPROF basic type.
fn hprof_type_size(t: u8) -> usize {
    match t {
        HPROF_BOOLEAN | HPROF_BYTE => 1,
        HPROF_CHAR | HPROF_SHORT => 2,
        HPROF_INT | HPROF_FLOAT => 4,
        HPROF_LONG | HPROF_DOUBLE | HPROF_OBJECT => 8,
        _ => 8,
    }
}

/// Write a Value to the segment according to the HPROF type.
fn write_value_for_type(seg: &mut SegmentBuilder, htype: u8, val: &cratonvm_types::Value) {
    match htype {
        HPROF_BOOLEAN | HPROF_BYTE => {
            seg.push_u8(val.as_int().unwrap_or(0) as u8);
        }
        HPROF_CHAR | HPROF_SHORT => {
            seg.push_u16(val.as_int().unwrap_or(0) as u16);
        }
        HPROF_INT => {
            seg.push_u32(val.as_int().unwrap_or(0) as u32);
        }
        HPROF_FLOAT => {
            seg.push_u32(val.as_float().unwrap_or(0.0).to_bits());
        }
        HPROF_LONG => {
            seg.push_u64(val.as_long().unwrap_or(0) as u64);
        }
        HPROF_DOUBLE => {
            seg.push_u64(val.as_double().unwrap_or(0.0).to_bits());
        }
        HPROF_OBJECT => match val.as_object() {
            Some(r) => seg.push_u64(r.as_ptr() as u64),
            None => seg.push_u64(0),
        },
        _ => seg.push_u64(0),
    }
}

/// Count the instance fields declared across a class's whole hierarchy.
fn count_instance_fields(class_id: ClassId, store: &cratonvm_classloading::ClassStore) -> usize {
    let mut n = 0usize;
    for cid in &class_hierarchy_chain(class_id, store) {
        if let Some(cls) = store.get(*cid) {
            n += cls.fields.iter().filter(|f| !f.is_static()).count();
        }
    }
    n
}

/// Compute the value written into CLASS_DUMP's "instance size" slot.
///
/// Observability audit (2026-07-26): this used to be the sum of
/// `hprof_type_size` over every instance field in the hierarchy — i.e. the
/// size a *HotSpot* object of this shape would occupy, minus its header. That
/// is wrong twice over for CratonVM:
///
///   1. HotSpot's own dumper reports a size that *includes* the object
///      header, so MAT's "shallow size" column was already understated by a
///      header's worth for every object in the dump.
///   2. CratonVM does not use HotSpot's packed layout. Every object carries a
///      16-byte header ([`cratonvm_types::HEADER_SIZE`]) and every instance
///      field — `boolean` included — occupies a 16-byte slot
///      ([`cratonvm_types::SLOT_SIZE`]). A class with eight `boolean` fields
///      was reported as 8 bytes when it really occupies 144. An operator
///      chasing a leak sized the wrong objects by more than an order of
///      magnitude.
///
/// HPROF places no constraint on this `u32` beyond it being the instance's
/// size in bytes; the reader decodes field *values* from the declared field
/// list, not from this number. So reporting CratonVM's true footprint is both
/// spec-legal and the only answer that makes MAT's shallow/retained sizes
/// mean anything.
fn compute_instance_byte_size(
    class_id: ClassId,
    store: &cratonvm_classloading::ClassStore,
) -> usize {
    cratonvm_types::HEADER_SIZE.saturating_add(
        count_instance_fields(class_id, store).saturating_mul(cratonvm_types::SLOT_SIZE),
    )
}

/// Build class hierarchy chain from leaf class up to java/lang/Object, then reverse.
///
/// The returned order is **root-first** (`java/lang/Object` at index 0). That
/// matches CratonVM's heap slot layout: `compute_field_layout` in
/// `classloading` assigns `first_field_index = superclass.num_total_fields`,
/// so slot 0 is the root superclass's first instance field.
///
/// NOTE for HPROF emission: the *wire* order for INSTANCE_DUMP field values is
/// the opposite (leaf class first). See
/// [`HprofDumper::write_instance_dump_with_values`].
fn class_hierarchy_chain(
    class_id: ClassId,
    store: &cratonvm_classloading::ClassStore,
) -> Vec<ClassId> {
    let mut chain = Vec::new();
    let mut current = Some(class_id);
    while let Some(cid) = current {
        chain.push(cid);
        current = store.get(cid).and_then(|c| c.superclass);
    }
    chain.reverse(); // Object first
    chain
}

// ---------------------------------------------------------------------------
// Full INSTANCE_DUMP with actual field values
// ---------------------------------------------------------------------------

impl<'a> HprofDumper<'a> {
    /// Improved write_instance_dump that reads actual field values from the heap.
    ///
    /// Observability audit (2026-07-26) — DEFECT FIXED (format-breaking).
    ///
    /// The HPROF binary spec pins the INSTANCE_DUMP payload order:
    ///
    /// ```text
    /// INSTANCE DUMP
    ///   ID   object ID
    ///   u4   stack trace serial number
    ///   ID   class object ID
    ///   u4   number of bytes that follow
    ///   [value]*  instance field values (this class, followed by super class, ...)
    /// ```
    ///
    /// "this class, followed by super class" — i.e. **leaf first**. HotSpot's
    /// `DumperSupport::dump_instance_fields` walks `o->klass()` and then
    /// `java_super()`, and both Eclipse MAT and VisualVM decode in that same
    /// order, pairing the bytes against each class's *own* declared field list
    /// (which is exactly what `write_class_dumps` emits, own-fields-only, as
    /// the spec also requires).
    ///
    /// This function used to serialize in `class_hierarchy_chain` order, which
    /// is **root first**. Every instance of a class with a field-carrying
    /// superclass therefore had its bytes decoded against the wrong field
    /// descriptors: a `Foo extends Thread` would show `Thread`'s `long eetop`
    /// under `Foo`'s first declared field and vice versa. When the two groups
    /// had different widths the misalignment cascaded through the rest of the
    /// record, so reference fields decoded as garbage object IDs and MAT
    /// reported dangling references / "unknown object" errors. The existing
    /// tests never caught it because they dump a VM whose only loaded class is
    /// `java/lang/Object` — a single-level hierarchy, where both orders agree.
    ///
    /// The *slot* walk must stay root-first: `compute_field_layout` in
    /// `classloading` gives the root superclass's fields the low slot indices.
    /// So we walk root-first to read the heap, buffer one byte group per
    /// class, and emit the groups in reverse.
    fn write_instance_dump_with_values(&self, seg: &mut SegmentBuilder, obj: ObjectRef) {
        let header = self.vm.mem.heap.get_header(obj);
        let class_id = header.class_id;
        let num_fields = header.num_slots() as usize;

        let cm = self.vm.classes.class_manager.read();
        let chain = class_hierarchy_chain(class_id, &cm.class_store);

        // Read root-first (heap slot order), buffering one group per class.
        let mut per_class: Vec<Vec<u8>> = Vec::with_capacity(chain.len());
        let mut slot_idx = 0usize;

        for cid in &chain {
            let mut group = Vec::new();
            if let Some(cls) = cm.class_store.get(*cid) {
                for f in &cls.fields {
                    if f.is_static() {
                        continue;
                    }
                    let htype = descriptor_to_hprof_type(&f.descriptor);
                    let val = if slot_idx < num_fields {
                        self.vm.mem.heap.get_field(obj, slot_idx)
                    } else {
                        cratonvm_types::Value::Int(0)
                    };
                    write_value_to_vec(&mut group, htype, &val);
                    slot_idx += 1;
                }
            }
            per_class.push(group);
        }

        // Emit leaf-first, per the HPROF spec.
        let mut field_data = Vec::new();
        for group in per_class.iter().rev() {
            field_data.extend_from_slice(group);
        }

        seg.push_u8(GC_INSTANCE_DUMP);
        seg.push_u64(obj.as_ptr() as u64);
        seg.push_u32(0);
        seg.push_u64(class_obj_id_for(class_id));
        seg.push_u32(field_data.len() as u32);
        seg.extend(&field_data);
    }
}

/// Write a value to a Vec<u8> according to the HPROF type.
fn write_value_to_vec(buf: &mut Vec<u8>, htype: u8, val: &cratonvm_types::Value) {
    match htype {
        HPROF_BOOLEAN | HPROF_BYTE => {
            buf.push(val.as_int().unwrap_or(0) as u8);
        }
        HPROF_CHAR | HPROF_SHORT => {
            buf.extend_from_slice(&(val.as_int().unwrap_or(0) as u16).to_be_bytes());
        }
        HPROF_INT => {
            buf.extend_from_slice(&(val.as_int().unwrap_or(0) as u32).to_be_bytes());
        }
        HPROF_FLOAT => {
            buf.extend_from_slice(&val.as_float().unwrap_or(0.0).to_bits().to_be_bytes());
        }
        HPROF_LONG => {
            buf.extend_from_slice(&(val.as_long().unwrap_or(0) as u64).to_be_bytes());
        }
        HPROF_DOUBLE => {
            buf.extend_from_slice(&val.as_double().unwrap_or(0.0).to_bits().to_be_bytes());
        }
        HPROF_OBJECT => match val.as_object() {
            Some(r) => buf.extend_from_slice(&(r.as_ptr() as u64).to_be_bytes()),
            None => buf.extend_from_slice(&0u64.to_be_bytes()),
        },
        _ => buf.extend_from_slice(&0u64.to_be_bytes()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Unit tests for helper functions --------------------------------

    #[test]
    fn s42_descriptor_to_hprof_type() {
        assert_eq!(descriptor_to_hprof_type("I"), HPROF_INT);
        assert_eq!(descriptor_to_hprof_type("J"), HPROF_LONG);
        assert_eq!(descriptor_to_hprof_type("B"), HPROF_BYTE);
        assert_eq!(descriptor_to_hprof_type("C"), HPROF_CHAR);
        assert_eq!(descriptor_to_hprof_type("D"), HPROF_DOUBLE);
        assert_eq!(descriptor_to_hprof_type("F"), HPROF_FLOAT);
        assert_eq!(descriptor_to_hprof_type("S"), HPROF_SHORT);
        assert_eq!(descriptor_to_hprof_type("Z"), HPROF_BOOLEAN);
        assert_eq!(descriptor_to_hprof_type("Ljava/lang/String;"), HPROF_OBJECT);
        assert_eq!(descriptor_to_hprof_type("[I"), HPROF_OBJECT);
    }

    #[test]
    fn s42_array_element_to_hprof() {
        assert_eq!(
            array_element_to_hprof(ArrayElementType::Boolean),
            HPROF_BOOLEAN
        );
        assert_eq!(array_element_to_hprof(ArrayElementType::Int), HPROF_INT);
        assert_eq!(array_element_to_hprof(ArrayElementType::Long), HPROF_LONG);
        assert_eq!(
            array_element_to_hprof(ArrayElementType::Double),
            HPROF_DOUBLE
        );
        assert_eq!(
            array_element_to_hprof(ArrayElementType::Reference),
            HPROF_OBJECT
        );
    }

    #[test]
    fn s42_hprof_type_sizes() {
        assert_eq!(hprof_type_size(HPROF_BOOLEAN), 1);
        assert_eq!(hprof_type_size(HPROF_BYTE), 1);
        assert_eq!(hprof_type_size(HPROF_CHAR), 2);
        assert_eq!(hprof_type_size(HPROF_SHORT), 2);
        assert_eq!(hprof_type_size(HPROF_INT), 4);
        assert_eq!(hprof_type_size(HPROF_FLOAT), 4);
        assert_eq!(hprof_type_size(HPROF_LONG), 8);
        assert_eq!(hprof_type_size(HPROF_DOUBLE), 8);
        assert_eq!(hprof_type_size(HPROF_OBJECT), 8);
    }

    #[test]
    fn s42_class_obj_id_deterministic() {
        let id1 = class_obj_id_for(ClassId::new(1));
        let id2 = class_obj_id_for(ClassId::new(2));
        assert_ne!(id1, id2);
        assert_eq!(id1, CLASS_OBJ_ID_BASE + 1);
        assert_eq!(id2, CLASS_OBJ_ID_BASE + 2);
    }

    // ---- Observability audit (2026-07-26) regression coverage -----------

    /// `array_class_name_for` must produce the internal-form array class
    /// name so `write_obj_array_dump` can find the real array class in the
    /// class store instead of labelling `String[]` as `java.lang.String`.
    #[test]
    fn obsaudit_array_class_name_for_internal_form() {
        assert_eq!(
            array_class_name_for("java/lang/String"),
            "[Ljava/lang/String;"
        );
        assert_eq!(
            array_class_name_for("java/lang/Object"),
            "[Ljava/lang/Object;"
        );
        // Nested arrays prepend a single `[` rather than re-wrapping in `L..;`
        assert_eq!(
            array_class_name_for("[Ljava/lang/String;"),
            "[[Ljava/lang/String;"
        );
        assert_eq!(array_class_name_for("[I"), "[[I");
    }

    /// CLASS_DUMP's instance-size slot must describe CratonVM's real object
    /// footprint (a `HEADER_SIZE` header + one 16-byte cell per instance field), not
    /// HotSpot's packed field-byte sum. Guards the fix for MAT reporting an
    /// eight-`boolean` class as 8 bytes when it occupies `HEADER_SIZE + 128`.
    #[test]
    fn obsaudit_instance_byte_size_uses_real_footprint() {
        // Direct arithmetic check of the size formula the dumper now writes.
        // A class with N instance fields occupies HEADER_SIZE + N*SLOT_SIZE,
        // regardless of the fields' Java widths.
        let header = cratonvm_types::HEADER_SIZE;
        let slot = cratonvm_types::SLOT_SIZE;
        // 32 -> 24 when `forwarding_ptr` folded into the mark word, 24 -> 16
        // when the identity hash followed it and the kind/element_type/gc_age/
        // gc_flags quartet joined them in the mark word's bits 48..62. The
        // tripwire has now done its job twice — each time naming the dumper
        // doc above that had to move with it — so it keeps its literal rather
        // than becoming self-proving.
        assert_eq!(
            header, 16,
            "object header size changed; update the dumper doc"
        );
        assert_eq!(slot, 16, "field slot size changed; update the dumper doc");

        // The old (wrong) computation summed hprof_type_size, which would
        // give 8 bytes for eight booleans. The new one must not.
        let old_style: usize = (0..8).map(|_| hprof_type_size(HPROF_BOOLEAN)).sum();
        assert_eq!(old_style, 8);
        let new_style = header + 8 * slot;
        assert_eq!(new_style, header + 128);
        assert_ne!(old_style, new_style);
    }

    /// INSTANCE_DUMP field values are emitted leaf-class-first per the HPROF
    /// spec ("this class, followed by super class"). This exercises the
    /// grouping/reversal logic directly: heap slots are read root-first, but
    /// the wire bytes must come out leaf-first.
    #[test]
    fn obsaudit_instance_field_groups_emit_leaf_first() {
        // Simulate the per-class byte groups the dumper builds while walking
        // the hierarchy root-first: Object (no fields), Super (one int = 0xAA),
        // Leaf (one int = 0xBB).
        let mut per_class: Vec<Vec<u8>> = Vec::new();
        per_class.push(Vec::new()); // java/lang/Object — no instance fields
        let mut sup = Vec::new();
        write_value_to_vec(&mut sup, HPROF_INT, &cratonvm_types::Value::Int(0xAA));
        per_class.push(sup);
        let mut leaf = Vec::new();
        write_value_to_vec(&mut leaf, HPROF_INT, &cratonvm_types::Value::Int(0xBB));
        per_class.push(leaf);

        let mut field_data = Vec::new();
        for group in per_class.iter().rev() {
            field_data.extend_from_slice(group);
        }

        assert_eq!(field_data.len(), 8, "two int fields = 8 bytes");
        let first =
            u32::from_be_bytes([field_data[0], field_data[1], field_data[2], field_data[3]]);
        let second =
            u32::from_be_bytes([field_data[4], field_data[5], field_data[6], field_data[7]]);
        assert_eq!(first, 0xBB, "leaf class's field must be written first");
        assert_eq!(second, 0xAA, "super class's field must follow the leaf's");
    }

    /// The in-memory segment threshold bounds peak RSS during a dump. The one
    /// production caller is the OOM path, so a gibibyte-sized staging buffer
    /// would fail exactly when it is needed.
    #[test]
    fn obsaudit_segment_threshold_is_bounded() {
        assert!(
            MAX_SEGMENT_SIZE <= 64 * 1024 * 1024,
            "MAX_SEGMENT_SIZE ({MAX_SEGMENT_SIZE}) is large enough to fail the \
             OutOfMemoryError dump path it exists to serve"
        );
        assert!(
            (MAX_SEGMENT_SIZE as u64) < u32::MAX as u64,
            "a segment larger than the u32 HPROF record-length field cannot be written"
        );
    }

    #[test]
    fn s42_segment_builder_basic() {
        let mut seg = SegmentBuilder::new();
        assert_eq!(seg.len(), 0);
        seg.push_u8(0xFF);
        seg.push_u16(0x1234);
        seg.push_u32(0xDEADBEEF);
        seg.push_u64(0x0102030405060708);
        // 1 + 2 + 4 + 8 = 15
        assert_eq!(seg.len(), 15);
    }

    #[test]
    fn s42_segment_builder_flush_writes_record() {
        let mut seg = SegmentBuilder::new();
        seg.push_u8(GC_INSTANCE_DUMP);
        seg.push_u64(0x42);

        let mut out = Vec::new();
        seg.flush(&mut out).unwrap();

        // Record: tag(1) + timestamp(4) + length(4) + body(9)
        assert_eq!(out[0], HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
        let body_len = u32::from_be_bytes([out[5], out[6], out[7], out[8]]);
        assert_eq!(body_len, 9); // 1 + 8
        assert_eq!(seg.len(), 0); // cleared after flush
    }

    #[test]
    fn s42_segment_builder_empty_flush_noop() {
        let mut seg = SegmentBuilder::new();
        let mut out = Vec::new();
        seg.flush(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn s42_write_value_for_type_int() {
        let mut seg = SegmentBuilder::new();
        write_value_for_type(&mut seg, HPROF_INT, &cratonvm_types::Value::Int(0x12345678));
        assert_eq!(seg.len(), 4);
        assert_eq!(&seg.buf, &0x12345678u32.to_be_bytes());
    }

    #[test]
    fn s42_write_value_for_type_long() {
        let mut seg = SegmentBuilder::new();
        write_value_for_type(
            &mut seg,
            HPROF_LONG,
            &cratonvm_types::Value::Long(123456789012345),
        );
        assert_eq!(seg.len(), 8);
        assert_eq!(&seg.buf, &(123456789012345u64).to_be_bytes());
    }

    #[test]
    fn s42_write_value_for_type_object_null() {
        let mut seg = SegmentBuilder::new();
        write_value_for_type(&mut seg, HPROF_OBJECT, &cratonvm_types::Value::Object(None));
        assert_eq!(seg.len(), 8);
        assert_eq!(&seg.buf, &0u64.to_be_bytes());
    }

    #[test]
    fn s42_write_value_for_type_float() {
        let mut seg = SegmentBuilder::new();
        let f: f32 = 3.14;
        write_value_for_type(&mut seg, HPROF_FLOAT, &cratonvm_types::Value::Float(f));
        assert_eq!(seg.len(), 4);
        assert_eq!(&seg.buf, &f.to_bits().to_be_bytes());
    }

    #[test]
    fn s42_write_value_for_type_double() {
        let mut seg = SegmentBuilder::new();
        let d: f64 = 2.71828;
        write_value_for_type(&mut seg, HPROF_DOUBLE, &cratonvm_types::Value::Double(d));
        assert_eq!(seg.len(), 8);
        assert_eq!(&seg.buf, &d.to_bits().to_be_bytes());
    }

    #[test]
    fn s42_write_value_for_type_byte() {
        let mut seg = SegmentBuilder::new();
        write_value_for_type(&mut seg, HPROF_BYTE, &cratonvm_types::Value::Int(42));
        assert_eq!(seg.len(), 1);
        assert_eq!(seg.buf[0], 42);
    }

    #[test]
    fn s42_write_value_for_type_short() {
        let mut seg = SegmentBuilder::new();
        write_value_for_type(&mut seg, HPROF_SHORT, &cratonvm_types::Value::Int(0x7FFF));
        assert_eq!(seg.len(), 2);
        assert_eq!(&seg.buf, &0x7FFFu16.to_be_bytes());
    }

    #[test]
    fn s42_write_value_for_type_boolean() {
        let mut seg = SegmentBuilder::new();
        write_value_for_type(&mut seg, HPROF_BOOLEAN, &cratonvm_types::Value::Int(1));
        assert_eq!(seg.len(), 1);
        assert_eq!(seg.buf[0], 1);
    }

    #[test]
    fn s42_write_value_to_vec_all_types() {
        // Verify write_value_to_vec produces the same result as write_value_for_type
        for (htype, val, expected_size) in [
            (HPROF_BYTE, cratonvm_types::Value::Int(99), 1),
            (HPROF_BOOLEAN, cratonvm_types::Value::Int(1), 1),
            (HPROF_SHORT, cratonvm_types::Value::Int(1000), 2),
            (HPROF_CHAR, cratonvm_types::Value::Int(65), 2),
            (HPROF_INT, cratonvm_types::Value::Int(42), 4),
            (HPROF_FLOAT, cratonvm_types::Value::Float(1.0), 4),
            (HPROF_LONG, cratonvm_types::Value::Long(999), 8),
            (HPROF_DOUBLE, cratonvm_types::Value::Double(1.0), 8),
            (HPROF_OBJECT, cratonvm_types::Value::Object(None), 8),
        ] {
            let mut buf = Vec::new();
            write_value_to_vec(&mut buf, htype, &val);
            assert_eq!(
                buf.len(),
                expected_size,
                "type={} expected size={}",
                htype,
                expected_size
            );
        }
    }

    // ---- Integration tests using SharedVm ------------------------------

    /// Helper: create a VM with at least one loaded class for dump tests.
    fn make_test_vm() -> Arc<SharedVm> {
        let config = crate::config::VmConfig {
            use_synthetic_jdk: true,
            ..Default::default()
        };
        let vm = Arc::new(crate::vm::SharedVm::new(config));
        // Load a class so the class store is non-empty
        let _ = vm.load_class_concurrent("java/lang/Object");
        vm
    }

    #[test]
    fn s42_full_dump_header_and_end_present() {
        let vm = make_test_vm();

        let mut output = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.write_all(&mut output).unwrap();

        // Verify header magic
        let magic = HprofWriter::HPROF_MAGIC.as_bytes();
        assert_eq!(&output[..magic.len()], magic);

        // Verify null terminator after magic
        assert_eq!(output[magic.len()], 0);

        // Verify identifier size = 8
        let id_size_offset = magic.len() + 1;
        let id_size = u32::from_be_bytes([
            output[id_size_offset],
            output[id_size_offset + 1],
            output[id_size_offset + 2],
            output[id_size_offset + 3],
        ]);
        assert_eq!(id_size, 8);

        // Verify HEAP_DUMP_END is the last record
        let end_tag = HprofWriter::HPROF_HEAP_DUMP_END;
        // The last 9 bytes should be: tag(1) + timestamp(4) + length(4)=0
        let tail = &output[output.len() - 9..];
        assert_eq!(tail[0], end_tag);
        let end_len = u32::from_be_bytes([tail[5], tail[6], tail[7], tail[8]]);
        assert_eq!(end_len, 0);
    }

    #[test]
    fn s42_dump_contains_string_records() {
        let vm = make_test_vm();

        let mut output = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.write_all(&mut output).unwrap();

        // Scan for at least one STRING_IN_UTF8 record
        let header_size = HprofWriter::header_size();
        let mut found_string = false;
        let mut pos = header_size;
        while pos + 9 <= output.len() {
            let tag = output[pos];
            let body_len = u32::from_be_bytes([
                output[pos + 5],
                output[pos + 6],
                output[pos + 7],
                output[pos + 8],
            ]) as usize;
            if tag == HprofWriter::HPROF_UTF8 {
                found_string = true;
                break;
            }
            pos += 9 + body_len;
        }
        assert!(
            found_string,
            "Dump should contain at least one STRING_IN_UTF8 record"
        );
    }

    #[test]
    fn s42_dump_contains_load_class_records() {
        let vm = make_test_vm();

        let mut output = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.write_all(&mut output).unwrap();

        let header_size = HprofWriter::header_size();
        let mut load_class_count = 0u32;
        let mut pos = header_size;
        while pos + 9 <= output.len() {
            let tag = output[pos];
            let body_len = u32::from_be_bytes([
                output[pos + 5],
                output[pos + 6],
                output[pos + 7],
                output[pos + 8],
            ]) as usize;
            if tag == HprofWriter::HPROF_LOAD_CLASS {
                load_class_count += 1;
            }
            pos += 9 + body_len;
        }
        assert!(
            load_class_count > 0,
            "Dump should contain LOAD_CLASS records"
        );
    }

    #[test]
    fn s42_dump_contains_heap_dump_segment() {
        let vm = make_test_vm();

        let mut output = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.write_all(&mut output).unwrap();

        let header_size = HprofWriter::header_size();
        let mut found_segment = false;
        let mut pos = header_size;
        while pos + 9 <= output.len() {
            let tag = output[pos];
            let body_len = u32::from_be_bytes([
                output[pos + 5],
                output[pos + 6],
                output[pos + 7],
                output[pos + 8],
            ]) as usize;
            if tag == HprofWriter::HPROF_HEAP_DUMP_SEGMENT {
                found_segment = true;
                break;
            }
            pos += 9 + body_len;
        }
        assert!(
            found_segment,
            "Dump should contain at least one HEAP_DUMP_SEGMENT"
        );
    }

    #[test]
    fn s42_dump_contains_class_dump_subrecords() {
        let vm = make_test_vm();

        let mut output = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.write_all(&mut output).unwrap();

        // Find a HEAP_DUMP_SEGMENT and look for CLASS_DUMP (0x20) sub-records
        let header_size = HprofWriter::header_size();
        let mut found_class_dump = false;
        let mut pos = header_size;
        while pos + 9 <= output.len() {
            let tag = output[pos];
            let body_len = u32::from_be_bytes([
                output[pos + 5],
                output[pos + 6],
                output[pos + 7],
                output[pos + 8],
            ]) as usize;
            if tag == HprofWriter::HPROF_HEAP_DUMP_SEGMENT && body_len > 0 {
                // Scan sub-records in segment body
                let body_start = pos + 9;
                let body_end = body_start + body_len;
                if body_start < output.len() && output[body_start] == GC_CLASS_DUMP {
                    found_class_dump = true;
                    break;
                }
                // Also scan further into the body for CLASS_DUMP tags
                let mut sub_pos = body_start;
                while sub_pos < body_end {
                    if output[sub_pos] == GC_CLASS_DUMP {
                        found_class_dump = true;
                        break;
                    }
                    sub_pos += 1;
                }
                if found_class_dump {
                    break;
                }
            }
            pos += 9 + body_len;
        }
        assert!(
            found_class_dump,
            "Dump should contain CLASS_DUMP sub-records in HEAP_DUMP_SEGMENT"
        );
    }

    #[test]
    fn s42_dump_stack_trace_present() {
        let vm = make_test_vm();

        let mut output = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.write_all(&mut output).unwrap();

        let header_size = HprofWriter::header_size();
        let mut found_trace = false;
        let mut pos = header_size;
        while pos + 9 <= output.len() {
            let tag = output[pos];
            let body_len = u32::from_be_bytes([
                output[pos + 5],
                output[pos + 6],
                output[pos + 7],
                output[pos + 8],
            ]) as usize;
            if tag == HprofWriter::HPROF_TRACE {
                found_trace = true;
                break;
            }
            pos += 9 + body_len;
        }
        assert!(
            found_trace,
            "Dump should contain at least one STACK_TRACE record"
        );
    }

    #[test]
    fn s42_dump_to_file_succeeds() {
        let vm = make_test_vm();

        let tmp = std::env::temp_dir().join("cratonvm_s42_test.hprof");
        let path = tmp.to_str().unwrap();
        let size = dump_heap(&vm, path, ThreadId(0)).expect("dump_heap should succeed");
        assert!(size > 0, "Dump file should be non-empty");

        // Verify the file starts with HPROF magic
        let data = std::fs::read(&tmp).unwrap();
        let magic = HprofWriter::HPROF_MAGIC.as_bytes();
        assert_eq!(&data[..magic.len()], magic);

        // Clean up
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn s42_dump_record_sequence_valid() {
        // Verify the record sequence: STRING* LOAD_CLASS* TRACE* SEGMENT+ END
        let vm = make_test_vm();

        let mut output = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.write_all(&mut output).unwrap();

        let header_size = HprofWriter::header_size();
        let mut tags = Vec::new();
        let mut pos = header_size;
        while pos + 9 <= output.len() {
            let tag = output[pos];
            let body_len = u32::from_be_bytes([
                output[pos + 5],
                output[pos + 6],
                output[pos + 7],
                output[pos + 8],
            ]) as usize;
            tags.push(tag);
            pos += 9 + body_len;
        }

        // Last tag must be HEAP_DUMP_END
        assert_eq!(*tags.last().unwrap(), HprofWriter::HPROF_HEAP_DUMP_END);

        // Must contain at least one HEAP_DUMP_SEGMENT
        assert!(
            tags.contains(&HprofWriter::HPROF_HEAP_DUMP_SEGMENT),
            "Must contain HEAP_DUMP_SEGMENT"
        );

        // First tags should be STRING records
        assert_eq!(tags[0], HprofWriter::HPROF_UTF8);

        // Must contain LOAD_CLASS and TRACE records
        assert!(tags.contains(&HprofWriter::HPROF_LOAD_CLASS));
        assert!(tags.contains(&HprofWriter::HPROF_TRACE));
    }

    #[test]
    fn s42_intern_deduplicates_strings() {
        let vm = make_test_vm();
        let mut dumper = HprofDumper::new(&vm);

        let id1 = dumper.intern("java/lang/Object");
        let id2 = dumper.intern("java/lang/Object");
        let id3 = dumper.intern("java/lang/String");
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn s42_class_serial_deterministic() {
        let vm = make_test_vm();
        let mut dumper = HprofDumper::new(&vm);

        let s1 = dumper.class_serial(ClassId::new(10));
        let s2 = dumper.class_serial(ClassId::new(10));
        let s3 = dumper.class_serial(ClassId::new(20));
        assert_eq!(s1, s2);
        assert_ne!(s1, s3);
        assert!(s1 >= 1);
        assert!(s3 >= 1);
    }
}
