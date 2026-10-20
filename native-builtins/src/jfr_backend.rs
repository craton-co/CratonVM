// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The VM half of JFR for a real JDK image (SBR5 lane `jfr`): the
//! `jdk.jfr.internal.JVM` natives that let the JDK's own `jdk.jfr` bytecode
//! run unmodified, the way HotSpot's `jfr/` tree serves it.
//!
//! What runs in Java (and is no longer shadowed): `Event` instrumentation
//! (`JVMUpcalls`/`EventInstrumentation`), `Recording`/`PlatformRecorder`,
//! `RecordingStream`/`EventDirectoryStream`, `JDKEvents`, the settings and
//! periodic machinery, the metadata descriptor, `EventWriter`. What this file
//! provides is the boundary:
//!
//! * the class schema hook's counterpart (`getConfiguration`/`setConfiguration`
//!   on the static `eventConfiguration` field, `retransformClasses` through the
//!   VM's redefinition, trace ids);
//! * the `EventWriter` protocol: per-thread native buffers in the `Unsafe`
//!   arena, `newEventWriter`/`getEventWriter`/`commit`/`flush`;
//! * the chunk: `setOutput` rotation, `flush`, `storeMetadataDescriptor`, the
//!   constant-pool tables behind `getThreadId`/`getClassId`/`getStackTraceId`/
//!   `addStringConstant`, written by [`crate::jfr_chunk`].
//!
//! No lock here is held across a call back into the VM (allocation, Java): a
//! thread parked on one while another waits for a collection would be a
//! deadlock.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

use crate::jfr_chunk::{
    encode_event, ChunkWriter, ClassRec, FrameRec, LoaderRec, MethodRec, ModuleRec, Needs,
    PackageRec, PoolTables, SegmentInput, StackRec, ThreadRec, Val,
};
use crate::jfr_meta::Metadata;

const JVM: &str = "jdk/jfr/internal/JVM";
/// One thread-local event buffer; grown on demand for a larger event.
const BUF_SIZE: usize = 32 * 1024;
/// The largest event buffer the writer will grow to (`EventWriter.MAX_EVENT_SIZE`
/// is 2^28; a buffer is never worth more than this).
const BUF_MAX: usize = 64 * 1024 * 1024;
const TICKS_PER_SECOND: i64 = 1_000_000_000;
const DEFAULT_MEMORY_CAP: usize = 64 * 1024 * 1024;

pub(crate) struct ThreadBuf {
    addr: i64,
    cap: usize,
    /// first byte not yet moved into the chunk
    top: i64,
    /// end of committed data (`JVM.commit`)
    pos: i64,
    writer_root: usize,
    jfr_tid: i64,
}

#[derive(Default)]
pub(crate) struct Backend {
    pub(crate) created: bool,
    pub(crate) recording: bool,
    pub(crate) blob: Vec<u8>,
    pub(crate) blob_id: i64,
    pub(crate) meta: Option<Arc<Metadata>>,
    pub(crate) tables: PoolTables,
    pub(crate) needs: Needs,
    pub(crate) chunk: Option<ChunkWriter>,
    pub(crate) bufs: HashMap<u64, ThreadBuf>,
    pub(crate) class_ids: HashMap<ClassId, i64>,
    pub(crate) method_ids: HashMap<(String, String, String), i64>,
    pub(crate) next_method_id: i64,
    pub(crate) stack_ids: HashMap<Vec<(i64, i32, i32, i64)>, i64>,
    pub(crate) next_stack_id: i64,
    pub(crate) instrumented: HashSet<ClassId>,
    pub(crate) file_notification: i64,
    pub(crate) memory_cap: usize,
    pub(crate) mark_final: bool,
    pub(crate) string_generation: i32,
    pub(crate) stack_depth: i32,
    /// stack filters registered with `registerStackFilter`: id -> (class, method) prefixes
    pub(crate) stack_filters: HashMap<i64, Vec<(String, String)>>,
    pub(crate) next_filter_id: i64,
    pub(crate) enabled: HashMap<i64, bool>,
    pub(crate) thresholds: HashMap<i64, i64>,
    pub(crate) stack_enabled: HashMap<i64, bool>,
    pub(crate) last_cpu: Option<(u64, u64, u64, u64)>,
    pub(crate) threads_started: i64,
    pub(crate) threads_peak: i64,
    pub(crate) class_name_ids: HashMap<String, i64>,
    pub(crate) next_gc_id_seen: i32,
    pub(crate) thread_cpu: HashMap<i64, (u64, u64, i64)>,
    pub(crate) sampler_running: bool,
    pub(crate) sampler_period_ms: i64,
    pub(crate) sampler_stop: bool,
    pub(crate) safepoint_id: i64,
    /// gci i31/hp: HotSpot's `MetaspaceGC::capacity_until_GC()` as this VM's
    /// collections have moved it (`jdk.MetaspaceSummary.gcThreshold`); 0 =
    /// not yet set (`MetaspaceSize`). See `jfr_vm_more::metaspace_threshold_step`.
    pub(crate) metaspace_capacity_until_gc: i64,
    /// zt t5/jf: the end (backend ticks) of the last ZGC collection
    /// `jfr_vm_more::gc_cycle` recorded, taken by the next `jdk.SystemGC`
    /// writer so the request's row ends with its collection, as HotSpot's
    /// `JVM_GC` (`jfr_vm_more::zgc_system_gc_duration`). Never set off ZGC.
    pub(crate) last_zgc_collection_end: Option<i64>,
    /// pool name -> text -> key, for enum-like VM pools: a key keeps its text for the chunk's whole life
    /// (`needs.extra` is emptied after each checkpoint, so it cannot hold the numbering).
    pub(crate) enum_keys: std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    /// type name -> id of the image's own `metadata.bin` (what HotSpot's `getTypeId(String)` answers); `None` until read.
    pub(crate) native_type_ids: Option<std::collections::HashMap<String, i64>>,
    pub(crate) sampler_periods: std::collections::BTreeMap<i64, i64>,
    pub(crate) sample_cursor: usize,
}

pub(crate) static BACKEND: VmScoped<Backend> = VmScoped::new();

pub(crate) fn now_ticks() -> i64 {
    crate::jfr::counter_time()
}

pub(crate) fn now_nanos() -> i64 {
    crate::jfr::epoch_nanos()
}

/// The last `n` of `args`: the parameters without a receiver (instance natives
/// on JDK 17/21 pass the `JVM` singleton first).
fn params(args: &[Value], n: usize) -> &[Value] {
    if args.len() > n {
        &args[args.len() - n..]
    } else {
        args
    }
}

fn arg_long(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    }
}

fn arg_int(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

fn arg_obj(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

pub(crate) fn ensure_chunk(b: &mut Backend) {
    if b.chunk.is_none() {
        b.chunk = Some(ChunkWriter::new(now_nanos(), now_ticks()));
    }
    if b.memory_cap == 0 {
        b.memory_cap = DEFAULT_MEMORY_CAP;
    }
}

/// Move every thread's committed events into the chunk.
fn drain_buffers(b: &mut Backend) {
    ensure_chunk(b);
    let cap = b.memory_cap.max(1 << 20);
    let mut collected: Vec<u8> = Vec::new();
    for buf in b.bufs.values_mut() {
        if buf.pos > buf.top {
            let len = (buf.pos - buf.top) as usize;
            let start = collected.len();
            collected.resize(start + len, 0);
            if crate::unsafe_arena_copy_out(buf.top, &mut collected[start..]) {
                buf.top = buf.pos;
            } else {
                collected.truncate(start);
                buf.top = buf.pos;
            }
        }
    }
    if let Some(chunk) = b.chunk.as_mut() {
        if !collected.is_empty() {
            chunk.append_events(&collected);
        }
        chunk.trim_pending(cap);
    }
}

/// Write a flush segment of the current chunk (events, checkpoint, metadata,
/// header). `finish` closes the file chunk.
fn flush_segment(b: &mut Backend, finish: bool) {
    drain_buffers(b);
    let live: Vec<i64> = b.bufs.values().map(|t| t.jfr_tid).collect();
    b.needs.threads.extend(live);
    let Some(chunk) = b.chunk.as_mut() else {
        return;
    };
    if !chunk.is_file() {
        return;
    }
    if finish && b.mark_final {
        chunk.final_chunk = true;
        b.mark_final = false;
    }
    let input = SegmentInput {
        meta: b.meta.as_deref(),
        metadata_blob: &b.blob,
        metadata_id: b.blob_id,
        tables: &b.tables,
        needs: &b.needs,
        now_nanos: now_nanos(),
        now_ticks: now_ticks(),
        ticks_per_second: TICKS_PER_SECOND,
    };
    if let Err(e) = chunk.flush_segment(&input, finish) {
        eprintln!("[cratonvm] JFR chunk write failed: {e}");
    }
    // What the checkpoint carried is no longer needed by the next segment.
    b.needs = Needs::default();
}

/// A chunk ends (or the memory chunk is handed to a file): a fresh epoch.
fn bump_generation(b: &mut Backend) -> i32 {
    b.string_generation = if b.string_generation >= 32767 {
        1
    } else {
        b.string_generation + 1
    };
    b.tables.strings.clear();
    b.string_generation
}

fn set_string_generation(ctx: &mut dyn NativeContext, generation: i32) {
    let Some(class_id) = ctx.class_id_by_name("jdk/jfr/internal/StringPool") else {
        return;
    };
    if !ctx.is_class_initialized(class_id) {
        return;
    }
    if let Some(index) = ctx.static_field_index_by_name(class_id, "generation") {
        ctx.set_static_field(class_id, index, Value::Int(generation));
    }
}

// ---------------------------------------------------------------------------
// ids: threads, classes, stack traces
// ---------------------------------------------------------------------------

pub(crate) fn jfr_thread_id(ctx: &mut dyn NativeContext, thread: ObjectRef) -> i64 {
    match crate::lang_misc::thread_field(&*ctx, thread, "tid") {
        Value::Long(t) if t > 0 => t,
        Value::Int(t) if t > 0 => i64::from(t),
        _ => ctx.thread_id().max(1) as i64,
    }
}

/// Record `thread` in the thread pool (first sight only) and mark it used.
pub(crate) fn register_thread(ctx: &mut dyn NativeContext, thread: ObjectRef) -> i64 {
    let tid = jfr_thread_id(ctx, thread);
    let known = BACKEND.with(ctx.vm_identity(), |b| {
        let known = b.tables.threads.contains_key(&tid);
        b.needs.threads.insert(tid);
        known
    });
    if known {
        return tid;
    }
    let name = match crate::lang_misc::thread_field(&*ctx, thread, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let class_id = ctx.class_id_of_object(thread);
    let is_virtual = ctx
        .class_name_of_id(class_id)
        .is_some_and(|n| n.contains("VirtualThread"));
    let os_tid = ctx.thread_os_tid(thread).map(i64::from).unwrap_or(0);
    BACKEND.with(ctx.vm_identity(), |b| {
        b.tables.threads.entry(tid).or_insert(ThreadRec {
            os_name: name.clone(),
            os_tid,
            java_name: name,
            java_tid: tid,
            group: 0,
            is_virtual,
        });
    });
    tid
}

pub(crate) fn loader_ref(b: &mut Backend, loader: i32) -> i64 {
    let id = i64::from(loader.max(0)) + 1;
    if !b.tables.loaders.contains_key(&id) {
        let name = match loader {
            0 => "bootstrap",
            1 => "platform",
            2 => "app",
            _ => "",
        };
        b.tables.symbol(name);
        b.tables.loaders.insert(
            id,
            LoaderRec {
                type_class: 0,
                name: name.to_owned(),
            },
        );
    }
    id
}

/// The JFR id of `class_id`, registering its class-pool record on first sight.
pub(crate) fn register_class(ctx: &mut dyn NativeContext, class_id: ClassId) -> i64 {
    let vm = ctx.vm_identity();
    if let Some(id) = BACKEND.with(vm, |b| {
        let id = b.class_ids.get(&class_id).copied();
        if let Some(id) = id {
            b.needs.classes.insert(id);
        }
        id
    }) {
        return id;
    }
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let modifiers = i32::from(ctx.class_access_flags(class_id));
    let hidden = ctx.is_class_hidden(class_id);
    let loader = ctx.loader_id_of_class(class_id);
    let module = ctx.module_name_of_class(class_id);
    BACKEND.with(vm, |b| {
        if let Some(&id) = b.class_ids.get(&class_id) {
            b.needs.classes.insert(id);
            return id;
        }
        let id = crate::jfr::fresh_type_id();
        b.class_ids.insert(class_id, id);
        b.class_name_ids.insert(name.clone(), id);
        let loader_ref = loader_ref(b, loader);
        b.tables.symbol(&name);
        let package_name = name
            .rsplit_once('/')
            .map(|(p, _)| p.to_owned())
            .unwrap_or_default();
        let mut package_id = 0;
        if !name.starts_with('[') {
            package_id = {
                // one package record per package name
                let key = b.tables.symbol(&package_name);
                if !b.tables.packages.contains_key(&key) {
                    let mut module_id = 0;
                    if let Some(m) = &module {
                        let mkey = b.tables.symbol(m);
                        module_id = mkey;
                        b.tables.modules.entry(mkey).or_insert_with(|| ModuleRec {
                            name: m.clone(),
                            version: String::new(),
                            location: String::new(),
                            loader: 0,
                        });
                    }
                    b.tables.symbol("");
                    b.tables.packages.insert(
                        key,
                        PackageRec {
                            name: package_name.clone(),
                            module: module_id,
                            exported: true,
                        },
                    );
                }
                key
            };
        }
        b.tables.classes.insert(
            id,
            ClassRec {
                loader: loader_ref,
                name: name.clone(),
                package: package_id,
                modifiers,
                hidden,
            },
        );
        b.needs.classes.insert(id);
        id
    })
}

fn register_method(
    ctx: &mut dyn NativeContext,
    class_jfr: i64,
    class_id: Option<ClassId>,
    class_name: &str,
    method: &str,
    descriptor: &str,
) -> i64 {
    let vm = ctx.vm_identity();
    let key = (
        class_name.to_owned(),
        method.to_owned(),
        descriptor.to_owned(),
    );
    if let Some(id) = BACKEND.with(vm, |b| b.method_ids.get(&key).copied()) {
        return id;
    }
    let modifiers = class_id
        .and_then(|c| ctx.declared_method_access_flags(c, method, descriptor))
        .map(i32::from)
        .unwrap_or(1);
    BACKEND.with(vm, |b| {
        if let Some(&id) = b.method_ids.get(&key) {
            return id;
        }
        b.next_method_id += 1;
        let id = b.next_method_id;
        b.method_ids.insert(key, id);
        b.tables.symbol(method);
        b.tables.symbol(descriptor);
        b.tables.methods.insert(
            id,
            MethodRec {
                class: class_jfr,
                name: method.to_owned(),
                descriptor: descriptor.to_owned(),
                modifiers,
                hidden: false,
            },
        );
        id
    })
}

pub(crate) fn jfr_stack_trace_id(ctx: &mut dyn NativeContext, skip: i32, filter_id: i64) -> i64 {
    let mut frames = ctx.capture_stack_trace();
    // `capture_stack_trace` lists the outermost frame first; JFR (and the skip
    // count) go innermost first, and HotSpot counts the native `getStackTraceId`
    // frame itself, which this capture does not include.
    frames.reverse();
    if std::env::var_os("CRATONVM_DBG_JFR").is_some() {
        eprintln!("[jfr] stack skip={skip} frames={}", frames.len());
        for f in frames.iter().take(8) {
            eprintln!(
                "[jfr]   {}.{}{:?} line={}",
                f.class_name, f.method_name, f.method_descriptor, f.line_number
            );
        }
    }
    let skip = (skip.max(1) - 1) as usize;
    intern_stack(ctx, &frames, skip, filter_id)
}

/// Intern `frames` (innermost first), after dropping `skip`, as a stack-trace
/// pool entry; `0` when nothing is left. A frame is `JIT compiled` when the JIT
/// cache holds a body of its method, `Interpreted` otherwise (the VM keeps no
/// per-frame inlining record, so `Inlined` is never reported).
pub(crate) fn intern_stack(
    ctx: &mut dyn NativeContext,
    frames: &[cratonvm_native_api::StackTraceEntry],
    skip: usize,
    filter_id: i64,
) -> i64 {
    if frames.len() <= skip {
        return 0;
    }
    let vm = ctx.vm_identity();
    let (depth, filters) = BACKEND.with(vm, |b| {
        (
            if b.stack_depth > 0 {
                b.stack_depth as usize
            } else {
                64
            },
            if filter_id != 0 {
                b.stack_filters.get(&filter_id).cloned()
            } else {
                None
            },
        )
    });
    let mut recs: Vec<(i64, i32, i32, i64)> = Vec::new();
    let mut truncated = false;
    for entry in frames.iter().skip(skip) {
        if let Some(filters) = &filters {
            let class_dotted = entry.class_name.replace('/', ".");
            if filters.iter().any(|(c, m)| {
                (c.is_empty() || class_dotted == *c || entry.class_name.as_ref() == c.as_str())
                    && (m.is_empty() || entry.method_name.as_ref() == m.as_str())
            }) {
                continue;
            }
        }
        if recs.len() >= depth {
            truncated = true;
            break;
        }
        let internal = entry.class_name.replace('.', "/");
        let class_id = entry.class_id.or_else(|| ctx.class_id_by_name(&internal));
        let class_jfr = match class_id {
            Some(c) => register_class(ctx, c),
            None => 0,
        };
        let descriptor = entry.method_descriptor.as_deref().unwrap_or("()V");
        let method = register_method(
            ctx,
            class_jfr,
            class_id,
            &internal,
            entry.method_name.as_ref(),
            descriptor,
        );
        let compiled = class_id.is_some_and(|c| {
            ctx.jfr_method_compiled(c, &internal, entry.method_name.as_ref(), descriptor)
        });
        let frame_type = if entry.line_number == -2 {
            3 // Native
        } else if compiled {
            1
        } else {
            0
        };
        recs.push((
            method,
            entry.line_number.max(0),
            entry.byte_code_index.max(0),
            frame_type,
        ));
    }
    if recs.is_empty() {
        return 0;
    }
    BACKEND.with(vm, |b| {
        let key = recs.clone();
        let id = if let Some(&id) = b.stack_ids.get(&key) {
            id
        } else {
            b.next_stack_id += 1;
            let id = b.next_stack_id;
            b.stack_ids.insert(key, id);
            b.tables.stacks.insert(
                id,
                StackRec {
                    truncated,
                    frames: recs
                        .iter()
                        .map(|&(method, line, bci, frame_type)| FrameRec {
                            method,
                            line,
                            bci,
                            frame_type,
                        })
                        .collect(),
                },
            );
            id
        };
        b.needs.stacks.insert(id);
        id
    })
}

// ---------------------------------------------------------------------------
// natives: lifecycle
// ---------------------------------------------------------------------------

fn ok_void() -> MethodCallResult {
    Ok(None)
}

fn ok_bool(v: bool) -> MethodCallResult {
    Ok(Some(Value::Int(i32::from(v))))
}

fn nat_create_jfr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let vm = ctx.vm_identity();
    if BACKEND.with(vm, |b| b.created) {
        return ok_bool(true);
    }
    if arg_int(args, 0) != 0 {
        // JVMSupport.createFailedNativeJFR(): fail on purpose.
        return ok_bool(false);
    }
    BACKEND.with(vm, |b| {
        b.created = true;
        ensure_chunk(b);
    });
    crate::jfr::mark_created(ctx);
    // HotSpot hands the Java half the internal types once the recorder exists.
    // Best effort: a failure here only hides internal types.
    let _ = ctx.invoke_static(
        "jdk/jfr/internal/JVMUpcalls",
        "unhideInternalTypes",
        "()V",
        &[],
    );
    ok_bool(true)
}

fn nat_destroy_jfr(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let was = BACKEND.with(vm, |b| {
        let was = b.created;
        if b.chunk.as_ref().is_some_and(|c| c.is_file()) {
            flush_segment(b, true);
        }
        b.created = false;
        set_recording(b, false);
        was
    });
    crate::jfr::clear_created(ctx);
    ok_bool(was)
}

fn nat_begin_recording(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    BACKEND.with(vm, |b| {
        set_recording(b, true);
        ensure_chunk(b);
        if let Some(chunk) = b.chunk.as_mut() {
            if !chunk.is_file() && chunk.pending_len() == 0 {
                chunk.start_nanos = now_nanos();
                chunk.start_ticks = now_ticks();
            }
        }
    });
    ctx.jfr_begin_java_recording();
    ok_void()
}

fn nat_end_recording(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let generation = BACKEND.with(vm, |b| {
        set_recording(b, false);
        if b.chunk.as_ref().is_some_and(|c| c.is_file()) {
            flush_segment(b, true);
            let g = bump_generation(b);
            b.chunk = Some(ChunkWriter::new(now_nanos(), now_ticks()));
            Some(g)
        } else {
            // An in-memory recording that nobody dumped: its events go with it.
            drain_buffers(b);
            let g = bump_generation(b);
            b.needs = Needs::default();
            b.chunk = Some(ChunkWriter::new(now_nanos(), now_ticks()));
            Some(g)
        }
    });
    if let Some(g) = generation {
        set_string_generation(ctx, g);
    }
    ctx.jfr_end_java_recording();
    ok_void()
}

fn nat_is_recording(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ok_bool(BACKEND.with(ctx.vm_identity(), |b| b.recording))
}

fn nat_mark_chunk_final(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    BACKEND.with(ctx.vm_identity(), |b| b.mark_final = true);
    ok_void()
}

fn nat_store_metadata(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let Some(array) = arg_obj(args, 0) else {
        return ok_void();
    };
    let len = ctx.array_length(array);
    let mut bytes = vec![0u8; len];
    ctx.read_byte_array_into(array, 0, &mut bytes);
    let meta = Metadata::parse(&bytes).map(Arc::new);
    BACKEND.with(ctx.vm_identity(), |b| {
        b.blob = bytes;
        b.blob_id += 1;
        b.meta = meta;
    });
    ok_void()
}

fn nat_set_output(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let path = arg_obj(args, 0).and_then(|s| ctx.read_string(s));
    let vm = ctx.vm_identity();
    let generation = BACKEND.with(vm, |b| {
        ensure_chunk(b);
        let mut generation = None;
        if b.chunk.as_ref().is_some_and(|c| c.is_file()) {
            flush_segment(b, true);
            generation = Some(bump_generation(b));
            b.chunk = Some(ChunkWriter::new(now_nanos(), now_ticks()));
        }
        if let Some(path) = &path {
            if let Some(chunk) = b.chunk.as_mut() {
                match chunk.open_file(path, TICKS_PER_SECOND) {
                    Ok(()) => {}
                    Err(e) => eprintln!("[cratonvm] JFR could not open chunk {path}: {e}"),
                }
            }
            // The first segment: pending events, a checkpoint, the metadata.
            // A streaming reader waits for it before it can parse the chunk.
            flush_segment(b, false);
        }
        generation
    });
    if let Some(g) = generation {
        set_string_generation(ctx, g);
    }
    ok_void()
}

fn nat_flush_all(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    BACKEND.with(ctx.vm_identity(), |b| {
        if b.chunk.as_ref().is_some_and(|c| c.is_file()) {
            flush_segment(b, false);
        } else {
            drain_buffers(b);
        }
    });
    ok_void()
}

fn nat_get_chunk_start_nanos(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let v = BACKEND.with(ctx.vm_identity(), |b| {
        ensure_chunk(b);
        b.chunk
            .as_ref()
            .map(|c| c.start_nanos)
            .unwrap_or_else(now_nanos)
    });
    Ok(Some(Value::Long(v)))
}

fn nat_should_rotate_disk(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let rotate = BACKEND.with(ctx.vm_identity(), |b| {
        b.file_notification > 0
            && b.chunk
                .as_ref()
                .is_some_and(|c| c.is_file() && c.size_hint() as i64 >= b.file_notification)
    });
    ok_bool(rotate)
}

fn nat_set_file_notification(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = arg_long(params(args, 1), 0);
    BACKEND.with(ctx.vm_identity(), |b| b.file_notification = v);
    ok_void()
}

fn nat_set_memory_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = arg_long(params(args, 1), 0);
    BACKEND.with(ctx.vm_identity(), |b| {
        if v > 0 {
            b.memory_cap = (v as usize).max(1 << 20);
        }
    });
    ok_void()
}

fn nat_set_stack_depth(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = arg_int(params(args, 1), 0);
    BACKEND.with(ctx.vm_identity(), |b| b.stack_depth = v);
    ok_void()
}

fn nat_set_enabled(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 2);
    let (id, on) = (arg_long(args, 0), arg_int(args, 1) != 0);
    BACKEND.with(ctx.vm_identity(), |b| {
        b.enabled.insert(id, on);
    });
    ok_void()
}

// ---------------------------------------------------------------------------
// natives: ids
// ---------------------------------------------------------------------------

fn nat_get_thread_id(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let tid = match arg_obj(args, 0) {
        Some(thread) => register_thread(ctx, thread),
        None => 0,
    };
    Ok(Some(Value::Long(tid)))
}

fn class_id_of_mirror_arg(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<ClassId> {
    let mirror = arg_obj(args, 0)?;
    ctx.class_id_from_mirror(mirror)
}

fn nat_get_class_id(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    match class_id_of_mirror_arg(ctx, args) {
        Some(class_id) => Ok(Some(Value::Long(register_class(ctx, class_id)))),
        // a primitive or array mirror: the legacy name-keyed id
        None => crate::jfr::legacy_type_id_from_mirror(ctx, args),
    }
}

/// `jdk.jfr.internal.JVM.getTypeId(String)` for the types the VM defines, with
/// the ids HotSpot 25's `metadata.bin` bakes in (`Sbr5JfrTypes`).
fn known_type_id(name: &str) -> Option<i64> {
    Some(match name {
        "long" => 231,
        "int" => 232,
        "short" => 233,
        "byte" => 234,
        "double" => 235,
        "float" => 236,
        "boolean" => 237,
        "char" => 238,
        "java.lang.String" => 239,
        "java.lang.Thread" => 188,
        "java.lang.Class" => 190,
        "jdk.types.StackTrace" => 212,
        _ => return None,
    })
}

/// `type name -> id` out of an image's `jdk/jfr/internal/types/metadata.bin`
/// (the `DataInputStream` file `MetadataLoader` reads). JDK 17/21 and JDK 25 lay a
/// type element out alike except that JDK 25 adds a `level` string before the
/// `experimental` flag, so both shapes are tried and the one that consumes the
/// whole file wins.
fn parse_image_type_ids(bytes: &[u8]) -> Option<std::collections::HashMap<String, i64>> {
    for with_level in [false, true] {
        if let Some(map) = parse_image_type_ids_shape(bytes, with_level) {
            return Some(map);
        }
    }
    None
}

fn parse_image_type_ids_shape(
    bytes: &[u8],
    with_level: bool,
) -> Option<std::collections::HashMap<String, i64>> {
    struct R<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl R<'_> {
        fn take(&mut self, n: usize) -> Option<&[u8]> {
            let s = self.b.get(self.p..self.p.checked_add(n)?)?;
            self.p += n;
            Some(s)
        }
        fn int(&mut self) -> Option<i32> {
            Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
        }
        fn long(&mut self) -> Option<i64> {
            Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
        }
        fn utf(&mut self) -> Option<String> {
            let n = u16::from_be_bytes(self.take(2)?.try_into().ok()?) as usize;
            Some(String::from_utf8_lossy(self.take(n)?).into_owned())
        }
        fn bool(&mut self) -> Option<bool> {
            Some(self.take(1)?[0] != 0)
        }
    }
    let mut r = R { b: bytes, p: 0 };
    let count = r.int()?;
    if !(0..100_000).contains(&count) {
        return None;
    }
    let mut map = std::collections::HashMap::new();
    for _ in 0..count {
        let fields = r.int()?;
        if !(0..10_000).contains(&fields) {
            return None;
        }
        for _ in 0..fields {
            for _ in 0..4 {
                r.utf()?;
            }
            for _ in 0..3 {
                r.bool()?;
            }
            for _ in 0..3 {
                r.utf()?;
            }
            r.bool()?;
        }
        let name = r.utf()?;
        for _ in 0..3 {
            r.utf()?; // label, description, category
        }
        for _ in 0..3 {
            r.bool()?; // thread, stackTrace, startTime
        }
        r.utf()?; // period
        r.bool()?; // cutoff
        r.bool()?; // throttle
        if with_level {
            r.utf()?;
        }
        r.bool()?; // experimental
        r.bool()?; // internal
        let id = r.long()?;
        r.bool()?;
        r.bool()?;
        map.insert(name, id);
    }
    (r.p == bytes.len()).then_some(map)
}

/// The id the image's own `metadata.bin` bakes in for `name` (HotSpot answers
/// `JVM.getTypeId(String)` from the same table). The ids differ between JDK
/// feature releases, so they are read from the running image, not fixed.
fn image_type_id(ctx: &mut dyn NativeContext, name: &str) -> Option<i64> {
    let vm = ctx.vm_identity();
    if BACKEND.with(vm, |b| b.native_type_ids.is_none()) {
        let map = ctx
            .find_resource("jdk/jfr/internal/types/metadata.bin")
            .and_then(|bytes| parse_image_type_ids(&bytes))
            .unwrap_or_default();
        BACKEND.with(vm, |b| b.native_type_ids = Some(map));
    }
    BACKEND.with(vm, |b| {
        b.native_type_ids
            .as_ref()
            .and_then(|m| m.get(name).copied())
    })
}

fn nat_get_type_id_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let name = arg_obj(args, 0)
        .and_then(|s| ctx.read_string(s))
        .unwrap_or_default();
    Ok(Some(Value::Long(
        image_type_id(ctx, &name)
            .or_else(|| known_type_id(&name))
            .unwrap_or_else(|| crate::jfr::type_id(&name)),
    )))
}

fn nat_get_stack_trace_id(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 2);
    let skip = arg_int(args, 0);
    let filter = arg_long(args, 1);
    Ok(Some(Value::Long(jfr_stack_trace_id(ctx, skip, filter))))
}

fn nat_add_string_constant(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 2);
    let id = arg_long(args, 0);
    let text = arg_obj(args, 1).and_then(|s| ctx.read_string(s));
    if let Some(text) = text {
        BACKEND.with(ctx.vm_identity(), |b| {
            b.tables.strings.insert(id, text);
            b.needs.strings.insert(id);
        });
    }
    ok_bool(true)
}

fn nat_register_stack_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 2);
    let read_strings = |ctx: &mut dyn NativeContext, arr: Option<ObjectRef>| -> Vec<String> {
        let Some(arr) = arr else { return Vec::new() };
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            })
            .collect()
    };
    let classes = read_strings(ctx, arg_obj(args, 0));
    let methods = read_strings(ctx, arg_obj(args, 1));
    let filters: Vec<(String, String)> = classes.into_iter().zip(methods).collect();
    let id = BACKEND.with(ctx.vm_identity(), |b| {
        b.next_filter_id += 1;
        let id = b.next_filter_id;
        b.stack_filters.insert(id, filters);
        id
    });
    Ok(Some(Value::Long(id)))
}

fn nat_unregister_stack_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(params(args, 1), 0);
    BACKEND.with(ctx.vm_identity(), |b| {
        b.stack_filters.remove(&id);
    });
    ok_void()
}

// ---------------------------------------------------------------------------
// natives: configuration, instrumentation
// ---------------------------------------------------------------------------

fn nat_set_configuration(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 2);
    let Some(class_id) = class_id_of_mirror_arg(ctx, args) else {
        return ok_bool(false);
    };
    let config = arg_obj(args, 1);
    match ctx.static_field_index_by_name(class_id, "eventConfiguration") {
        Some(index) => {
            ctx.set_static_field(class_id, index, Value::Object(config));
            ok_bool(true)
        }
        // A class the schema hook did not extend (it had no event superclass
        // when defined): keep the legacy side table so the round trip holds.
        None => crate::jfr::legacy_set_configuration(ctx, args),
    }
}

fn nat_get_configuration(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let Some(class_id) = class_id_of_mirror_arg(ctx, args) else {
        return Ok(Some(Value::Object(None)));
    };
    // Reading the static field through JNI initializes the class on HotSpot,
    // which is what registers a user event class the first time anything asks
    // for its configuration (`EventType.getEventType(Ev.class)`).
    ctx.ensure_class_initialized_with_class_id(class_id)?;
    match ctx.static_field_index_by_name(class_id, "eventConfiguration") {
        Some(index) => Ok(Some(ctx.get_static_field(class_id, index))),
        None => crate::jfr::legacy_get_configuration(ctx, args),
    }
}

fn nat_is_instrumented(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let Some(class_id) = class_id_of_mirror_arg(ctx, args) else {
        return ok_bool(false);
    };
    ok_bool(BACKEND.with(ctx.vm_identity(), |b| b.instrumented.contains(&class_id)))
}

fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = vec![0u8; len];
    ctx.read_byte_array_into(arr, 0, &mut out);
    out
}

/// `JVMUpcalls.onRetransform(traceId, false, false, clazz, oldBytes)`.
fn upcall_on_retransform(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    trace_id: i64,
    old: &[u8],
) -> Option<Vec<u8>> {
    let mut scope = NativeHandleScope::new(ctx);
    let mirror = scope.get_class_mirror(class_id);
    let mirror = scope.root(mirror);
    let array = scope.new_array(ArrayElementType::Byte, old.len());
    let array = scope.root(array);
    let target = scope.get(&array);
    if !scope.write_byte_array_from(target, 0, old) {
        return None;
    }
    let mirror_value = scope.get(&mirror);
    let array_value = scope.get(&array);
    let result = scope
        .invoke_static(
            "jdk/jfr/internal/JVMUpcalls",
            "onRetransform",
            "(JZZLjava/lang/Class;[B)[B",
            &[
                Value::Long(trace_id),
                Value::Int(0),
                Value::Int(0),
                Value::Object(Some(mirror_value)),
                Value::Object(Some(array_value)),
            ],
        )
        .ok()??;
    let Value::Object(Some(out)) = result else {
        return None;
    };
    Some(read_byte_array(&*scope, out))
}

fn nat_retransform_classes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let Some(array) = arg_obj(args, 0) else {
        return ok_void();
    };
    let mut class_ids: Vec<ClassId> = Vec::new();
    for i in 0..ctx.array_length(array) {
        if let Value::Object(Some(mirror)) = ctx.get_array_element(array, i) {
            if let Some(id) = ctx.class_id_from_mirror(mirror) {
                class_ids.push(id);
            }
        }
    }
    let retransform_start = now_ticks();
    let batch = ctx.begin_redefinition_batch();
    let mut definitions: Vec<(ClassId, Vec<u8>, Vec<u8>)> = Vec::new();
    for class_id in class_ids {
        let Some(original) = ctx
            .class_bytes(class_id)
            .or_else(|| ctx.reconstituted_class_bytes(class_id))
        else {
            continue;
        };
        let trace_id = register_class_type_id(ctx, class_id);
        let Some(instrumented) = upcall_on_retransform(ctx, class_id, trace_id, &original) else {
            continue;
        };
        if instrumented == original || instrumented.is_empty() {
            continue;
        }
        definitions.push((class_id, instrumented, original));
    }
    let mut ok_ids: Vec<ClassId> = Vec::new();
    if !definitions.is_empty() {
        let mut refused = false;
        for (class_id, bytes, _) in &definitions {
            if let Err(refusal) = ctx.check_class_redefinition(*class_id, bytes) {
                if std::env::var_os("CRATONVM_DBG_JFR").is_some() {
                    eprintln!("[jfr] retransform refused: {:?}", refusal.detail);
                }
                refused = true;
                break;
            }
        }
        if !refused {
            let classes: Vec<(ClassId, &[u8])> = definitions
                .iter()
                .map(|(c, b, _)| (*c, b.as_slice()))
                .collect();
            let held = ctx.begin_redefinition_install(&classes);
            // gci i19/rv finding 2 (orchestrator-applied): lowered on an OOM
            // unwind too (see `instrument::install_redefinitions`).
            let installed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for (class_id, bytes, original) in &definitions {
                    match ctx.redefine_class_as_instrument(
                        *class_id,
                        bytes,
                        true,
                        Some(original.as_slice()),
                    ) {
                        Ok(()) => ok_ids.push(*class_id),
                        Err(refusal) => {
                            if std::env::var_os("CRATONVM_DBG_JFR").is_some() {
                                eprintln!("[jfr] retransform failed: {:?}", refusal.detail);
                            }
                            break;
                        }
                    }
                }
            }));
            ctx.end_redefinition_install(held);
            if let Err(payload) = installed {
                ctx.end_redefinition_batch(batch);
                std::panic::resume_unwind(payload);
            }
        }
    }
    ctx.end_redefinition_batch(batch);
    if !ok_ids.is_empty() && crate::jfr_vm_events::any_recording() {
        let refs: Vec<i64> = ok_ids.iter().map(|c| register_class(ctx, *c)).collect();
        crate::jfr_vm_more::retransformed(ctx.vm_identity(), retransform_start, &refs);
    }
    if !ok_ids.is_empty() {
        BACKEND.with(ctx.vm_identity(), |b| {
            b.instrumented.extend(ok_ids);
        });
    }
    ok_void()
}

/// The id `JVM.getTypeId(Class)` answers for this class, without needing a
/// mirror argument.
fn register_class_type_id(ctx: &mut dyn NativeContext, class_id: ClassId) -> i64 {
    register_class(ctx, class_id)
}

// ---------------------------------------------------------------------------
// natives: the EventWriter protocol
// ---------------------------------------------------------------------------

fn nat_get_event_writer(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let tid = ctx.thread_id();
    let root = BACKEND
        .peek(ctx.vm_identity(), |b| {
            b.bufs.get(&tid).map(|t| t.writer_root)
        })
        .flatten()
        .unwrap_or(0);
    if root == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let writer = ctx.resolve_global_root(root);
    if let Some(writer) = writer {
        if ctx.is_current_virtual() {
            // the carrier's writer belongs to whichever virtual thread runs now
            let thread = ctx.current_thread_object();
            let jfr_tid = jfr_thread_id(ctx, thread);
            let writer = ctx.resolve_global_root(root).unwrap_or(writer);
            ctx.set_field_by_name(writer, "threadID", Value::Long(jfr_tid));
            register_thread(ctx, thread);
        }
        let writer = ctx.resolve_global_root(root).unwrap_or(writer);
        return Ok(Some(Value::Object(Some(writer))));
    }
    Ok(Some(Value::Object(None)))
}

fn set_writer_excluded(ctx: &mut dyn NativeContext, thread: ObjectRef, excluded: bool) {
    let Some(key) = ctx.thread_registry_id(thread) else {
        return;
    };
    let root = BACKEND
        .peek(ctx.vm_identity(), |b| {
            b.bufs.get(&key).map(|t| t.writer_root)
        })
        .flatten()
        .unwrap_or(0);
    if root == 0 {
        return;
    }
    if let Some(writer) = ctx.resolve_global_root(root) {
        ctx.set_field_by_name(writer, "excluded", Value::Int(i32::from(excluded)));
    }
}

fn nat_exclude(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(thread) = arg_obj(params(args, 1), 0) {
        crate::jfr::exclude_thread(&*ctx, thread)?;
        set_writer_excluded(ctx, thread, true);
    }
    ok_void()
}

fn nat_include(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(thread) = arg_obj(params(args, 1), 0) {
        crate::jfr::include_thread(&*ctx, thread);
        set_writer_excluded(ctx, thread, false);
    }
    ok_void()
}

fn nat_new_event_writer(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let tid = ctx.thread_id();
    let addr = crate::unsafe_arena_allocate(BUF_SIZE);
    if addr == 0 {
        return Err(MethodCallFailed::from(RuntimeError::OutOfMemoryError {
            message: "JFR thread buffer".to_owned(),
        }));
    }
    let thread = ctx.current_thread_object();
    let jfr_tid = register_thread(ctx, thread);
    let excluded = i32::from(crate::jfr::thread_is_excluded(&*ctx, thread));
    let is_virtual = i32::from(ctx.is_current_virtual());
    // JDK 21's EventWriter has no `pinVirtualThread` flag: five parameters.
    let has_pin = ctx.method_exists("jdk/jfr/internal/event/EventWriter", "<init>", "(JJJZZZ)V");
    let created = {
        let mut scope = NativeHandleScope::new(&mut *ctx);
        let mut ctor_args = vec![
            Value::Long(addr),
            Value::Long(addr + BUF_SIZE as i64),
            Value::Long(jfr_tid),
            Value::Int(1),
        ];
        if has_pin {
            ctor_args.push(Value::Int(is_virtual));
        }
        ctor_args.push(Value::Int(excluded));
        scope.new_object_initialized(
            "jdk/jfr/internal/event/EventWriter",
            if has_pin { "(JJJZZZ)V" } else { "(JJJZZ)V" },
            &ctor_args,
        )?
    };
    let Some(Value::Object(Some(writer))) = created else {
        crate::unsafe_arena_free(addr);
        return Ok(Some(Value::Object(None)));
    };
    let root = ctx.add_global_root(writer);
    let old = BACKEND.with(vm, |b| {
        b.bufs.insert(
            tid,
            ThreadBuf {
                addr,
                cap: BUF_SIZE,
                top: addr,
                pos: addr,
                writer_root: root,
                jfr_tid,
            },
        )
    });
    if let Some(old) = old {
        // a stale entry for a reused thread id
        crate::unsafe_arena_free(old.addr);
        if old.writer_root != 0 {
            ctx.remove_global_root(old.writer_root);
        }
    }
    Ok(Some(Value::Object(ctx.resolve_global_root(root))))
}

/// `JVM.commit(long nextPosition)`: the event written up to `nextPosition` is
/// complete. Answers `nextPosition`, which the writer reads as "committed, keep
/// writing from here".
fn nat_commit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let next = arg_long(params(args, 1), 0);
    let tid = ctx.thread_id();
    BACKEND.with(ctx.vm_identity(), |b| {
        if let Some(buf) = b.bufs.get_mut(&tid) {
            if next >= buf.pos && next <= buf.addr + buf.cap as i64 {
                buf.pos = next;
            }
        }
    });
    Ok(Some(Value::Long(next)))
}

/// `JVM.flush(EventWriter, uncommittedSize, requestedSize)`: the thread's
/// buffer is full. Move what is committed into the chunk, carry the unfinished
/// event to the front of the buffer (growing it if `requestedSize` needs more
/// room) and tell the writer where it is now.
fn nat_flush_writer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 3);
    let Some(writer) = arg_obj(args, 0) else {
        return ok_void();
    };
    let uncommitted = arg_int(args, 1).max(0) as usize;
    let requested = arg_int(args, 2).max(0) as usize;
    let tid = ctx.thread_id();
    let vm = ctx.vm_identity();
    let positions = BACKEND.with(vm, |b| {
        drain_buffers_for(b, tid);
        let buf = b.bufs.get_mut(&tid)?;
        // buf.top == buf.pos here: everything committed is in the chunk
        let need = uncommitted + requested;
        let mut carried = vec![0u8; uncommitted];
        if uncommitted > 0 && !crate::unsafe_arena_copy_out(buf.pos, &mut carried) {
            return None;
        }
        let mut addr = buf.addr;
        let mut cap = buf.cap;
        if need > cap {
            let mut new_cap = cap;
            while new_cap < need && new_cap < BUF_MAX {
                new_cap *= 2;
            }
            if new_cap < need {
                // beyond the largest buffer: the event is dropped (valid = false)
                buf.top = buf.addr;
                buf.pos = buf.addr;
                return Some((buf.addr, buf.addr, buf.addr + cap as i64, false));
            }
            let new_addr = crate::unsafe_arena_allocate(new_cap);
            if new_addr == 0 {
                buf.top = buf.addr;
                buf.pos = buf.addr;
                return Some((buf.addr, buf.addr, buf.addr + cap as i64, false));
            }
            crate::unsafe_arena_free(buf.addr);
            addr = new_addr;
            cap = new_cap;
            buf.addr = new_addr;
            buf.cap = new_cap;
        }
        if uncommitted > 0 {
            crate::unsafe_arena_copy_in(addr, &carried);
        }
        buf.top = addr;
        buf.pos = addr;
        Some((addr, addr + uncommitted as i64, addr + cap as i64, true))
    });
    if let Some((start, current, max, valid)) = positions {
        ctx.set_fields_by_name(
            writer,
            &[
                ("startPosition", Value::Long(start)),
                ("currentPosition", Value::Long(current)),
                ("maxPosition", Value::Long(max)),
                ("valid", Value::Int(i32::from(valid))),
            ],
        );
    }
    ok_void()
}

fn drain_buffers_for(b: &mut Backend, tid: u64) {
    ensure_chunk(b);
    let cap = b.memory_cap.max(1 << 20);
    let mut bytes: Vec<u8> = Vec::new();
    if let Some(buf) = b.bufs.get_mut(&tid) {
        if buf.pos > buf.top {
            bytes.resize((buf.pos - buf.top) as usize, 0);
            crate::unsafe_arena_copy_out(buf.top, &mut bytes);
            buf.top = buf.pos;
        }
    }
    if let Some(chunk) = b.chunk.as_mut() {
        if !bytes.is_empty() {
            chunk.append_events(&bytes);
        }
        chunk.trim_pending(cap);
    }
}

// ---------------------------------------------------------------------------
// Producing VM-side events
// ---------------------------------------------------------------------------

/// Append a finished VM event of type `name` to the chunk. Returns whether the
/// metadata declares the type and the event was written.
pub fn emit_vm_event(vm: usize, name: &str, values: &[(String, Val)]) -> bool {
    BACKEND.with(vm, |b| {
        let Some(meta) = b.meta.clone() else {
            return false;
        };
        let Some(bytes) = encode_event(&meta, name, values) else {
            return false;
        };
        ensure_chunk(b);
        if let Some(chunk) = b.chunk.as_mut() {
            chunk.append_events(&bytes);
        }
        true
    })
}

/// A concrete `jdk.jfr.Event` subclass finished initialization (called by the
/// VM's class-initialization path for such classes only). HotSpot registers it
/// with `FlightRecorder` at that point unless it (or a superclass up to
/// `jdk.jfr.Event`) is `@Registered(false)`; the Java half then creates the
/// `EventConfiguration` and stores it with `JVM.setConfiguration`.
pub fn event_class_initialized(ctx: &mut dyn NativeContext, class_id: ClassId) {
    let Some(event_root) = ctx.class_id_by_name("jdk/jfr/Event") else {
        return;
    };
    let mut registered = true;
    let mut current = Some(class_id);
    let mut depth = 0;
    'walk: while let Some(c) = current {
        if c == event_root || depth > 64 {
            break;
        }
        for annotation in ctx.class_annotations(c) {
            if annotation.type_descriptor == "Ljdk/jfr/Registered;" {
                for (name, value) in &annotation.elements {
                    if name == "value" {
                        if let cratonvm_native_api::AnnotationElementValue::Int(v) = value {
                            registered = *v != 0;
                        }
                    }
                }
                break 'walk;
            }
        }
        current = ctx.superclass_of(c);
        depth += 1;
    }
    if !registered {
        return;
    }
    let mut scope = NativeHandleScope::new(ctx);
    let mirror = scope.get_class_mirror(class_id);
    let _ = scope.invoke_static(
        "jdk/jfr/FlightRecorder",
        "register",
        "(Ljava/lang/Class;)V",
        &[Value::Object(Some(mirror))],
    );
}
/// Forget everything of `vm` (VM teardown).
pub(crate) fn forget_vm(vm: usize) {
    BACKEND.with(vm, |b| set_recording(b, false));
    BACKEND.forget(vm);
}

/// The VM's mark in a thread table row: this thread's native buffer is gone
/// (the thread exited): flush what it committed. Returns the writer's global
/// root handle for the caller to release.
pub(crate) fn thread_exited(vm: usize, tid: u64) -> Option<usize> {
    if !BACKEND.has_row(vm) {
        return None;
    }
    BACKEND.with(vm, |b| {
        drain_buffers_for(b, tid);
        b.bufs.remove(&tid).map(|buf| {
            crate::unsafe_arena_free(buf.addr);
            buf.writer_root
        })
    })
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------

pub fn register_real_jfr_backend(registry: &mut NativeMethodRegistry) {
    let prev = registry.current_category();
    registry.set_category(NativeKind::Bridge);
    let table: &[(&str, &str, cratonvm_native_api::NativeCallback)] = &[
        ("createJFR", "(Z)Z", nat_create_jfr),
        ("destroyJFR", "()Z", nat_destroy_jfr),
        ("beginRecording", "()V", nat_begin_recording),
        ("endRecording", "()V", nat_end_recording),
        ("isRecording", "()Z", nat_is_recording),
        ("markChunkFinal", "()V", nat_mark_chunk_final),
        ("storeMetadataDescriptor", "([B)V", nat_store_metadata),
        ("setOutput", "(Ljava/lang/String;)V", nat_set_output),
        ("flush", "()V", nat_flush_all),
        ("getChunkStartNanos", "()J", nat_get_chunk_start_nanos),
        ("shouldRotateDisk", "()Z", nat_should_rotate_disk),
        ("setFileNotification", "(J)V", nat_set_file_notification),
        ("setMemorySize", "(J)V", nat_set_memory_size),
        ("setStackDepth", "(I)V", nat_set_stack_depth),
        ("setEnabled", "(JZ)V", nat_set_enabled),
        ("emitEvent", "(JJJ)Z", nat_emit_event),
        ("setThreshold", "(JJ)Z", nat_set_threshold),
        ("setStackTraceEnabled", "(JZ)V", nat_set_stack_trace_enabled),
        ("getThreadId", "(Ljava/lang/Thread;)J", nat_get_thread_id),
        ("getClassId", "(Ljava/lang/Class;)J", nat_get_class_id),
        ("getTypeId", "(Ljava/lang/Class;)J", nat_get_class_id),
        ("getTypeId", "(Ljava/lang/String;)J", nat_get_type_id_string),
        ("getStackTraceId", "(IJ)J", nat_get_stack_trace_id),
        ("getStackTraceId", "(I)J", nat_get_stack_trace_id_old),
        (
            "addStringConstant",
            "(JLjava/lang/String;)Z",
            nat_add_string_constant,
        ),
        (
            "registerStackFilter",
            "([Ljava/lang/String;[Ljava/lang/String;)J",
            nat_register_stack_filter,
        ),
        ("unregisterStackFilter", "(J)V", nat_unregister_stack_filter),
        (
            "setConfiguration",
            "(Ljava/lang/Class;Ljdk/jfr/internal/event/EventConfiguration;)Z",
            nat_set_configuration,
        ),
        (
            "getConfiguration",
            "(Ljava/lang/Class;)Ljava/lang/Object;",
            nat_get_configuration,
        ),
        (
            "isInstrumented",
            "(Ljava/lang/Class;)Z",
            nat_is_instrumented,
        ),
        (
            "retransformClasses",
            "([Ljava/lang/Class;)V",
            nat_retransform_classes,
        ),
        (
            "getEventWriter",
            "()Ljdk/jfr/internal/event/EventWriter;",
            nat_get_event_writer,
        ),
        (
            "newEventWriter",
            "()Ljdk/jfr/internal/event/EventWriter;",
            nat_new_event_writer,
        ),
        ("commit", "(J)J", nat_commit),
        ("exclude", "(Ljava/lang/Thread;)V", nat_exclude),
        ("include", "(Ljava/lang/Thread;)V", nat_include),
        (
            "flush",
            "(Ljdk/jfr/internal/event/EventWriter;II)V",
            nat_flush_writer,
        ),
    ];
    for (name, descriptor, callback) in table {
        registry.register_with_kind(JVM, name, descriptor, *callback, NativeKind::Bridge);
    }
    // The answers that changed from "no" to "yes" once instrumentation works.
    registry.register_with_kind(
        JVM,
        "getAllowedToDoEventRetransforms",
        "()Z",
        |_ctx, _args| ok_bool(true),
        NativeKind::Bridge,
    );
    crate::jfr_vm_more::register_sampler_native(registry);
    crate::jfr_vm_more::register_sampler_natives(registry);
    registry.set_category(prev);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_type_ids_are_the_hotspot_25_ones() {
        assert_eq!(known_type_id("long"), Some(231));
        assert_eq!(known_type_id("java.lang.String"), Some(239));
        assert_eq!(known_type_id("jdk.types.StackTrace"), Some(212));
        assert_eq!(known_type_id("com.example.Foo"), None);
    }

    fn metadata_bin(with_level: bool) -> Vec<u8> {
        fn utf(v: &mut Vec<u8>, s: &str) {
            v.extend_from_slice(&(s.len() as u16).to_be_bytes());
            v.extend_from_slice(s.as_bytes());
        }
        let mut v = Vec::new();
        v.extend_from_slice(&1i32.to_be_bytes()); // one type
        v.extend_from_slice(&0i32.to_be_bytes()); // no fields
        utf(&mut v, "jdk.types.X");
        for _ in 0..3 {
            utf(&mut v, ""); // label, description, category
        }
        v.extend_from_slice(&[0, 0, 0]); // thread, stackTrace, startTime
        utf(&mut v, ""); // period
        v.extend_from_slice(&[0, 0]); // cutoff, throttle
        if with_level {
            utf(&mut v, "");
        }
        v.extend_from_slice(&[0, 0]); // experimental, internal
        v.extend_from_slice(&200i64.to_be_bytes());
        v.extend_from_slice(&[0, 0]);
        v
    }

    #[test]
    fn image_type_ids_parse_both_layouts() {
        for with_level in [false, true] {
            let ids = parse_image_type_ids(&metadata_bin(with_level)).expect("parses");
            assert_eq!(ids.get("jdk.types.X"), Some(&200));
        }
        assert!(parse_image_type_ids(&[0, 0, 0]).is_none());
    }

    #[test]
    fn params_drops_a_receiver() {
        let args = [Value::Int(7), Value::Long(1)];
        assert_eq!(params(&args, 1).len(), 1);
        assert_eq!(params(&args, 2).len(), 2);
    }
}

static VM_START_MILLIS: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

/// Wall-clock milliseconds when this process's VM was set up (`jvmStartTime`).
pub(crate) fn vm_start_millis() -> i64 {
    *VM_START_MILLIS.get_or_init(|| now_nanos() / 1_000_000)
}

/// `epoch nanos - counter ticks`: add to a tick to get epoch nanos, subtract an
/// epoch timestamp to get ticks.
pub(crate) fn epoch_to_ticks_offset() -> i64 {
    now_nanos() - now_ticks()
}

fn nat_emit_event(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 3);
    let (type_id, timestamp) = (arg_long(args, 0), arg_long(args, 1));
    ok_bool(crate::jfr_vm_events::emit_periodic(ctx, type_id, timestamp))
}

fn nat_set_threshold(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 2);
    let (id, ticks) = (arg_long(args, 0), arg_long(args, 1));
    BACKEND.with(ctx.vm_identity(), |b| {
        b.thresholds.insert(id, ticks);
    });
    ok_bool(true)
}

fn nat_set_stack_trace_enabled(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 2);
    let (id, on) = (arg_long(args, 0), arg_int(args, 1) != 0);
    BACKEND.with(ctx.vm_identity(), |b| {
        b.stack_enabled.insert(id, on);
    });
    ok_void()
}

fn set_recording(b: &mut Backend, on: bool) {
    use std::sync::atomic::Ordering;
    if b.recording != on {
        b.recording = on;
        if on {
            crate::jfr_vm_events::RECORDING_VMS.fetch_add(1, Ordering::Relaxed);
        } else {
            crate::jfr_vm_events::RECORDING_VMS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// The class-pool id of the class named `internal` (slash form), registering a
/// record from the name alone when no `ClassId` was ever registered for it.
/// For producers with no `NativeContext` (the JIT worker, the GC).
pub(crate) fn class_by_name_locked(b: &mut Backend, internal: &str) -> i64 {
    if let Some(&id) = b.class_name_ids.get(internal) {
        b.needs.classes.insert(id);
        return id;
    }
    let id = crate::jfr::fresh_type_id();
    b.class_name_ids.insert(internal.to_owned(), id);
    let loader = loader_ref(b, 0);
    b.tables.symbol(internal);
    let package_name = internal
        .rsplit_once('/')
        .map(|(p, _)| p.to_owned())
        .unwrap_or_default();
    let key = b.tables.symbol(&package_name);
    b.tables.packages.entry(key).or_insert(PackageRec {
        name: package_name,
        module: 0,
        exported: true,
    });
    b.tables.classes.insert(
        id,
        ClassRec {
            loader,
            name: internal.to_owned(),
            package: key,
            modifiers: 1,
            hidden: false,
        },
    );
    b.needs.classes.insert(id);
    id
}

/// The method-pool id of `class.name descriptor`, from names alone.
pub(crate) fn method_by_name_locked(
    b: &mut Backend,
    class: &str,
    name: &str,
    descriptor: &str,
) -> i64 {
    let class_id = class_by_name_locked(b, class);
    let key = (class.to_owned(), name.to_owned(), descriptor.to_owned());
    if let Some(&id) = b.method_ids.get(&key) {
        b.needs.methods.insert(id);
        return id;
    }
    b.next_method_id += 1;
    let id = b.next_method_id;
    b.method_ids.insert(key, id);
    b.tables.symbol(name);
    b.tables.symbol(descriptor);
    b.tables.methods.insert(
        id,
        MethodRec {
            class: class_id,
            name: name.to_owned(),
            descriptor: descriptor.to_owned(),
            modifiers: 1,
            hidden: false,
        },
    );
    b.needs.methods.insert(id);
    id
}

/// JDK 17/21's `getStackTraceId(int)`: no filter argument.
fn nat_get_stack_trace_id_old(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let args = params(args, 1);
    let skip = arg_int(args, 0);
    let id = jfr_stack_trace_id(ctx, skip, 0);
    if std::env::var_os("CRATONVM_DBG_JFR").is_some() {
        eprintln!("[jfr] getStackTraceId(I) args={args:?} -> {id}");
    }
    Ok(Some(Value::Long(id)))
}
