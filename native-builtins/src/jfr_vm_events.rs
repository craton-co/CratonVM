// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The events HotSpot's own JFR writes for the VM itself (SBR5 lane `jfr`),
//! produced into the chunk [`crate::jfr_backend`] writes: the native periodic
//! events the Java side asks for through `JVM.emitEvent` (`jdk.JVMInformation`,
//! `jdk.OSInformation`, `jdk.InitialSystemProperty`, `jdk.CPUInformation`,
//! `jdk.CPULoad`, `jdk.PhysicalMemory`, ...), and the instant/duration events
//! the VM raises itself (`jdk.ThreadStart`, `jdk.ThreadEnd`, `jdk.ClassLoad`,
//! `jdk.GarbageCollection`, `jdk.GCPhasePause`).
//!
//! Field layouts are not hard-coded here beyond field *names*: the metadata the
//! Java half stored with `storeMetadataDescriptor` decides order and encoding
//! ([`crate::jfr_chunk::encode_event`]), so a field this file does not set is
//! written as that type's zero. An event type the metadata does not declare, a
//! type that is not enabled and one whose duration is under its threshold are
//! all skipped before any work is done.

use std::sync::atomic::Ordering;

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::jfr_backend::{
    ensure_chunk, jfr_stack_trace_id, now_ticks, register_class, register_thread, Backend, BACKEND,
};
use crate::jfr_chunk::{encode_event, s, ThreadRec, Val};

/// How many VMs have a recording running: the one load every hook starts with.
pub(crate) use cratonvm_native_api::JFR_BACKEND_RECORDING as RECORDING_VMS;

/// A hook's cheap first test.
#[inline]
pub fn any_recording() -> bool {
    RECORDING_VMS.load(Ordering::Relaxed) != 0
}

pub(crate) fn fields(pairs: Vec<(&str, Val)>) -> Vec<(String, Val)> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// `Some(want_stack_trace)` when `name` should be written now: recording,
/// declared, enabled, and `duration` (ticks) at least its threshold.
pub(crate) fn admit(b: &Backend, name: &str, duration: i64) -> Option<bool> {
    if !b.recording {
        return None;
    }
    let meta = b.meta.as_ref()?;
    let ty = meta.by_name(name)?;
    if !b.enabled.get(&ty.id).copied().unwrap_or(false) {
        return None;
    }
    if duration < b.thresholds.get(&ty.id).copied().unwrap_or(0) {
        return None;
    }
    Some(b.stack_enabled.get(&ty.id).copied().unwrap_or(false))
}

pub(crate) fn append(b: &mut Backend, name: &str, values: &[(String, Val)]) {
    let Some(meta) = b.meta.clone() else {
        return;
    };
    let Some(bytes) = encode_event(&meta, name, values) else {
        return;
    };
    ensure_chunk(b);
    if let Some(chunk) = b.chunk.as_mut() {
        chunk.append_events(&bytes);
    }
}

/// The thread the VM itself runs GC and safepoints on (`VM Thread`).
const VM_THREAD_ID: i64 = 1;

pub(crate) fn ensure_vm_thread(b: &mut Backend) -> i64 {
    b.tables.threads.entry(VM_THREAD_ID).or_insert(ThreadRec {
        os_name: "VM Thread".to_owned(),
        os_tid: std::process::id() as i64,
        java_name: String::new(),
        java_tid: 0,
        group: 0,
        is_virtual: false,
    });
    b.needs.threads.insert(VM_THREAD_ID);
    VM_THREAD_ID
}

// ---------------------------------------------------------------------------
// instant / duration events raised by the VM
// ---------------------------------------------------------------------------

/// A thread was started: `jdk.ThreadStart`, emitted by the starting thread.
pub fn thread_started(ctx: &mut dyn NativeContext, thread: ObjectRef, parent: Option<ObjectRef>) {
    let vm = ctx.vm_identity();
    let want = BACKEND.with(vm, |b| {
        b.threads_started += 1;
        admit(b, "jdk.ThreadStart", 0)
    });
    let Some(want_stack) = want else {
        return;
    };
    let start = now_ticks();
    let child = register_thread(ctx, thread);
    let parent = parent.map(|p| register_thread(ctx, p)).unwrap_or(0);
    let event_thread = child;
    let stack = if want_stack {
        jfr_stack_trace_id(ctx, 1, 0)
    } else {
        0
    };
    BACKEND.with(vm, |b| {
        append(
            b,
            "jdk.ThreadStart",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("eventThread", Val::Ref(event_thread)),
                ("stackTrace", Val::Ref(stack)),
                ("thread", Val::Ref(child)),
                ("parentThread", Val::Ref(parent)),
            ]),
        );
    });
}

/// A thread is ending: `jdk.ThreadEnd`.
pub fn thread_ended(ctx: &mut dyn NativeContext, thread: ObjectRef) {
    let vm = ctx.vm_identity();
    if BACKEND.with(vm, |b| admit(b, "jdk.ThreadEnd", 0)).is_none() {
        return;
    }
    let start = now_ticks();
    let t = register_thread(ctx, thread);
    BACKEND.with(vm, |b| {
        append(
            b,
            "jdk.ThreadEnd",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("eventThread", Val::Ref(t)),
                ("thread", Val::Ref(t)),
            ]),
        );
    });
}

/// A class finished loading and linking: `jdk.ClassLoad`.
pub fn class_loaded(ctx: &mut dyn NativeContext, class_id: ClassId, duration_ticks: i64) {
    let vm = ctx.vm_identity();
    let Some(want_stack) = BACKEND.with(vm, |b| admit(b, "jdk.ClassLoad", duration_ticks)) else {
        return;
    };
    let start = now_ticks() - duration_ticks;
    let class_ref = register_class(ctx, class_id);
    let thread = ctx.current_thread_object();
    let t = register_thread(ctx, thread);
    let stack = if want_stack {
        jfr_stack_trace_id(ctx, 1, 0)
    } else {
        0
    };
    BACKEND.with(vm, |b| {
        append(
            b,
            "jdk.ClassLoad",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration_ticks)),
                ("eventThread", Val::Ref(t)),
                ("stackTrace", Val::Ref(stack)),
                ("loadedClass", Val::Ref(class_ref)),
                ("definingClassLoader", Val::Ref(0)),
                ("initiatingClassLoader", Val::Ref(0)),
            ]),
        );
    });
}

/// A collection ran: `jdk.GarbageCollection` and its `jdk.GCPhasePause`.
/// `name` and `cause` index the VM's own GC name / GC cause pools, which are
/// written with the checkpoint (see [`register_static_pools`]).
pub fn gc_collected(
    vm: usize,
    gc_id: i32,
    name: &str,
    cause: &str,
    start_nanos: u64,
    duration_nanos: u64,
) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = duration_nanos as i64;
    BACKEND.with(vm, |b| {
        let gc = admit(b, "jdk.GarbageCollection", duration);
        let phase = admit(b, "jdk.GCPhasePause", duration);
        if gc.is_none() && phase.is_none() {
            return;
        }
        let t = ensure_vm_thread(b);
        if gc.is_some() {
            let name_key = gc_name_key(b, name);
            let cause_key = gc_cause_key(b, cause);
            append(
                b,
                "jdk.GarbageCollection",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(duration)),
                    ("eventThread", Val::Ref(t)),
                    ("gcId", Val::Int(gc_id)),
                    ("name", Val::Ref(name_key)),
                    ("cause", Val::Ref(cause_key)),
                    ("sumOfPauses", Val::Long(duration)),
                    ("longestPause", Val::Long(duration)),
                ]),
            );
        }
        if phase.is_some() {
            append(
                b,
                "jdk.GCPhasePause",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(duration)),
                    ("eventThread", Val::Ref(t)),
                    ("gcId", Val::Int(gc_id)),
                    ("name", s("GC Pause")),
                ]),
            );
        }
    });
}

/// Register `text` in the `extra` pool `pool` (an enum-like VM pool whose
/// entries are one string field) and return its key.
///
/// zs s8/jf2: keys start at 1, so `Val::Ref(0)` in an enum-typed field is
/// null as JFR's parser reads it (id 0 resolves to null only when the pool
/// has no entry 0). Numbered from 0, a "no value" reference read back as the
/// pool's first entry -- a `ZAllocationStall` with no refusal on record read
/// `type = Small` once a relocation-set group had interned it. The keys are
/// this backend's own numbering (each entry is written with its text), so no
/// reader sees the shift. `CRATONVM_JFR_ENUM_KEYS_FROM_ONE=0` numbers from 0
/// again.
pub(crate) fn enum_key(b: &mut Backend, pool: &str, field: &str, text: &str) -> i64 {
    let first = enum_first_key();
    let table = b.enum_keys.entry(pool.to_owned()).or_default();
    let key = match table.get(text) {
        Some(k) => *k,
        None => {
            let k = table.len() as i64 + first;
            table.insert(text.to_owned(), k);
            k
        }
    };
    // (Re-)register the entry for the next checkpoint: the numbering is the
    // backend's, the checkpoint's `needs` only says what to write.
    b.needs
        .extra
        .entry(pool.to_owned())
        .or_default()
        .entry(key)
        .or_insert_with(|| vec![(field.to_owned(), s(text))]);
    key
}

/// zs s8/jf2: the first key [`enum_key`] hands out -- 1, or 0 under
/// `CRATONVM_JFR_ENUM_KEYS_FROM_ONE=0`. Read once per process (a writer
/// spelling, not compatibility state).
fn enum_first_key() -> i64 {
    static FIRST: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *FIRST.get_or_init(|| {
        i64::from(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JFR_ENUM_KEYS_FROM_ONE",
        ))
    })
}

fn gc_name_key(b: &mut Backend, name: &str) -> i64 {
    enum_key(b, "jdk.types.GCName", "name", name)
}

fn gc_cause_key(b: &mut Backend, cause: &str) -> i64 {
    enum_key(b, "jdk.types.GCCause", "cause", cause)
}

// ---------------------------------------------------------------------------
// periodic events behind JVM.emitEvent
// ---------------------------------------------------------------------------

fn prop(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let k = scope.create_string(key);
    let k = scope.root(k);
    let kv = scope.get(&k);
    match scope
        .invoke_static(
            "java/lang/System",
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(kv))],
        )
        .ok()?
    {
        Some(Value::Object(Some(text))) => scope.read_string(text),
        _ => None,
    }
}

pub(crate) fn proc_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn cpu_times() -> Option<(u64, u64, u64, u64)> {
    // (machine busy, machine total, jvm user, jvm system) in clock ticks
    let stat = proc_file("/proc/stat")?;
    let line = stat.lines().next()?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    let total: u64 = nums.iter().take(8).sum();
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let me = proc_file("/proc/self/stat")?;
    let after = me.rsplit_once(')')?.1;
    let parts: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = parts.get(11)?.parse().ok()?;
    let stime: u64 = parts.get(12)?.parse().ok()?;
    Some((total - idle, total, utime, stime))
}

fn cpu_model() -> (String, i32, i32) {
    let text = proc_file("/proc/cpuinfo").unwrap_or_default();
    let mut model = String::new();
    let mut processors = 0;
    let mut sockets = std::collections::BTreeSet::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "model name" if model.is_empty() => model = v.trim().to_owned(),
                "processor" => processors += 1,
                "physical id" => {
                    sockets.insert(v.trim().to_owned());
                }
                _ => {}
            }
        }
    }
    let hw = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(processors.max(1));
    (model, sockets.len().max(1) as i32, hw)
}

/// `JVM.emitEvent(typeId, timestamp, periodicType)`: write the periodic event
/// of that type. `false` when the VM has no producer for it.
pub fn emit_periodic(ctx: &mut dyn NativeContext, type_id: i64, timestamp: i64) -> bool {
    let vm = ctx.vm_identity();
    let Some(name) = BACKEND.with(vm, |b| {
        b.meta
            .as_ref()
            .and_then(|m| m.by_id(type_id))
            .map(|t| t.name.clone())
    }) else {
        return false;
    };
    let one = |pairs: Vec<(&str, Val)>| -> Vec<Vec<(String, Val)>> {
        let mut all = vec![("startTime", Val::Long(timestamp))];
        all.extend(pairs);
        vec![fields(all)]
    };
    let events: Vec<Vec<(String, Val)>> = match name.as_str() {
        "jdk.JVMInformation" => {
            // Wave 118 (i61-L1): the identity table, not a literal of its own.
            let vm_name = prop(ctx, "java.vm.name").unwrap_or_else(|| {
                cratonvm_types::vm_properties::vm_identity_value("java.vm.name")
                    .unwrap_or_default()
                    .to_owned()
            });
            let vm_ver = prop(ctx, "java.vm.version").unwrap_or_default();
            let os = prop(ctx, "os.name").unwrap_or_default().to_lowercase();
            let arch = prop(ctx, "os.arch").unwrap_or_default();
            let rt = prop(ctx, "java.runtime.version").unwrap_or_default();
            let cmd = prop(ctx, "sun.java.command").unwrap_or_default();
            one(vec![
                ("jvmName", s(&vm_name)),
                (
                    "jvmVersion",
                    s(&format!("{vm_name} ({vm_ver}) for {os}-{arch} JRE ({rt})")),
                ),
                ("jvmArguments", Val::Str(None)),
                ("jvmFlags", Val::Str(None)),
                ("javaArguments", s(&cmd)),
                (
                    "jvmStartTime",
                    Val::Long(crate::jfr_backend::vm_start_millis()),
                ),
                ("pid", Val::Long(i64::from(std::process::id()))),
            ])
        }
        "jdk.OSInformation" => {
            let os = prop(ctx, "os.name").unwrap_or_default();
            let ver = prop(ctx, "os.version").unwrap_or_default();
            one(vec![("osVersion", s(&format!("{os} {ver}")))])
        }
        "jdk.CPUInformation" => {
            let (model, sockets, hw) = cpu_model();
            one(vec![
                ("cpu", s(&model)),
                ("description", s(&model)),
                ("sockets", Val::Int(sockets)),
                ("cores", Val::Int(hw)),
                ("hwThreads", Val::Int(hw)),
            ])
        }
        "jdk.PhysicalMemory" => {
            let total = proc_file("/proc/meminfo")
                .and_then(|t| {
                    let mut total = 0i64;
                    let mut avail = 0i64;
                    for l in t.lines() {
                        if let Some(v) = l.strip_prefix("MemTotal:") {
                            total =
                                v.trim().trim_end_matches("kB").trim().parse::<i64>().ok()? * 1024;
                        } else if let Some(v) = l.strip_prefix("MemAvailable:") {
                            avail =
                                v.trim().trim_end_matches("kB").trim().parse::<i64>().ok()? * 1024;
                        }
                    }
                    Some((total, total - avail))
                })
                .unwrap_or((0, 0));
            one(vec![
                ("totalSize", Val::Long(total.0)),
                ("usedSize", Val::Long(total.1)),
            ])
        }
        "jdk.CPULoad" => {
            let now = cpu_times();
            let last = BACKEND.with(vm, |b| {
                let last = b.last_cpu;
                if now.is_some() {
                    b.last_cpu = now;
                }
                last
            });
            let (user, system, machine) = match (last, now) {
                (Some((b0, t0, u0, s0)), Some((b1, t1, u1, s1))) if t1 > t0 => {
                    let dt = (t1 - t0) as f32;
                    // jvm times are per process, machine totals sum all cpus
                    (
                        ((u1 - u0) as f32 / dt).min(1.0),
                        ((s1 - s0) as f32 / dt).min(1.0),
                        ((b1 - b0) as f32 / dt).min(1.0),
                    )
                }
                _ => (0.0, 0.0, 0.0),
            };
            one(vec![
                ("jvmUser", Val::Float(user)),
                ("jvmSystem", Val::Float(system)),
                ("machineTotal", Val::Float(machine)),
            ])
        }
        "jdk.InitialSystemProperty" => {
            // HotSpot reports the properties the VM itself owns (and `-D`s), not
            // the ones the JDK's Java code derives later (`java.version`, ...).
            const VM_OWNED: [&str; 15] = [
                "java.class.path",
                "java.home",
                "java.library.path",
                "java.vm.compressedOopsMode",
                "java.vm.info",
                "java.vm.name",
                "java.vm.specification.name",
                "java.vm.specification.vendor",
                "java.vm.specification.version",
                "java.vm.vendor",
                "java.vm.version",
                "jdk.debug",
                "sun.boot.library.path",
                "sun.java.command",
                "sun.java.launcher",
            ];
            let pairs = system_properties(ctx);
            pairs
                .into_iter()
                .filter(|(k, _)| VM_OWNED.contains(&k.as_str()))
                .map(|(k, v)| {
                    fields(vec![
                        ("startTime", Val::Long(timestamp)),
                        ("key", s(&k)),
                        ("value", s(&v)),
                    ])
                })
                .collect()
        }
        "jdk.InitialEnvironmentVariable" => std::env::vars()
            .map(|(k, v)| {
                fields(vec![
                    ("startTime", Val::Long(timestamp)),
                    ("key", s(&k)),
                    ("value", s(&v)),
                ])
            })
            .collect(),
        "jdk.ResidentSetSize" => {
            let status = proc_file("/proc/self/status").unwrap_or_default();
            let kb = |key: &str| -> i64 {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(key))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<i64>().ok())
                    .unwrap_or(0)
                    * 1024
            };
            one(vec![
                ("size", Val::Long(kb("VmRSS:"))),
                ("peak", Val::Long(kb("VmHWM:"))),
            ])
        }
        "jdk.SwapSpace" => {
            let info = proc_file("/proc/meminfo").unwrap_or_default();
            let kb = |key: &str| -> i64 {
                info.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<i64>().ok())
                    .unwrap_or(0)
                    * 1024
            };
            one(vec![
                ("totalSize", Val::Long(kb("SwapTotal:"))),
                ("freeSize", Val::Long(kb("SwapFree:"))),
            ])
        }
        "jdk.VirtualizationInformation" => one(vec![("name", s("No virtualization detected"))]),
        "jdk.CPUTimeStampCounter" => one(vec![
            ("fastTimeEnabled", Val::Bool(false)),
            ("fastTimeAutoEnabled", Val::Bool(false)),
            ("osFrequency", Val::Long(1_000_000_000)),
            ("fastTimeFrequency", Val::Long(0)),
        ]),
        "jdk.JavaMonitorStatistics" => one(vec![("count", Val::Long(0))]),
        "jdk.CompilerConfiguration" => one(vec![
            ("threadCount", Val::Int(1)),
            ("tieredCompilation", Val::Bool(true)),
            ("dynamicCompilerThreadCount", Val::Bool(false)),
        ]),
        "jdk.NativeLibrary" => {
            let maps = proc_file("/proc/self/maps").unwrap_or_default();
            let mut libs: Vec<(String, i64, i64)> = Vec::new();
            for l in maps.lines() {
                let mut it = l.split_whitespace();
                let (Some(range), Some(_), Some(_), Some(_), Some(_), Some(path)) = (
                    it.next(),
                    it.next(),
                    it.next(),
                    it.next(),
                    it.next(),
                    it.next(),
                ) else {
                    continue;
                };
                if !path.starts_with('/') || !(path.contains(".so") || path.ends_with("cratonvm")) {
                    continue;
                }
                let Some((lo, hi)) = range.split_once('-') else {
                    continue;
                };
                let (Ok(lo), Ok(hi)) = (i64::from_str_radix(lo, 16), i64::from_str_radix(hi, 16))
                else {
                    continue;
                };
                match libs.iter_mut().find(|(n, _, _)| n == path) {
                    Some(e) => {
                        e.1 = e.1.min(lo);
                        e.2 = e.2.max(hi);
                    }
                    None => libs.push((path.to_owned(), lo, hi)),
                }
            }
            libs.into_iter()
                .map(|(n, lo, hi)| {
                    fields(vec![
                        ("startTime", Val::Long(timestamp)),
                        ("name", s(&n)),
                        ("baseAddress", Val::Long(lo)),
                        ("topAddress", Val::Long(hi)),
                    ])
                })
                .collect()
        }
        "jdk.SystemProcess" => {
            let mut out = Vec::new();
            if let Ok(dir) = std::fs::read_dir("/proc") {
                for entry in dir.flatten() {
                    let pid = entry.file_name().to_string_lossy().to_string();
                    if !pid.bytes().all(|b| b.is_ascii_digit()) {
                        continue;
                    }
                    let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
                        .map(|b| {
                            String::from_utf8_lossy(&b)
                                .split('\0')
                                .collect::<Vec<_>>()
                                .join(" ")
                                .trim()
                                .to_owned()
                        })
                        .unwrap_or_default();
                    out.push(fields(vec![
                        ("startTime", Val::Long(timestamp)),
                        ("pid", s(&pid)),
                        (
                            "commandLine",
                            if cmd.is_empty() {
                                Val::Str(None)
                            } else {
                                s(&cmd)
                            },
                        ),
                    ]));
                }
            }
            out
        }
        "jdk.JavaThreadStatistics" => {
            // Interpreter round i5 wave 100, lane L6: HotSpot reads these off
            // `ThreadService` -- the JMM's platform-thread counters, which
            // `ThreadMXBean` answers too (`phases_late::management`): live
            // platform threads (no virtual thread), the live daemons (was a
            // constant 0), the cumulative starts and the peak.
            use crate::phases_late::management as jmm;
            let active = i64::from(jmm::live_platform_thread_count(&*ctx));
            let daemons = i64::from(jmm::daemon_thread_count(&*ctx));
            let started = jmm::total_started_thread_count(&*ctx);
            let jmm_peak = i64::from(jmm::peak_thread_count(&*ctx));
            let (accumulated, peak) = BACKEND.with(vm, |b| {
                if active > b.threads_peak {
                    b.threads_peak = active;
                }
                (b.threads_started.max(started), b.threads_peak.max(jmm_peak))
            });
            one(vec![
                ("activeCount", Val::Long(active)),
                ("daemonCount", Val::Long(daemons)),
                ("accumulatedCount", Val::Long(accumulated)),
                ("peakCount", Val::Long(peak.max(active))),
            ])
        }
        "jdk.ClassLoadingStatistics" => {
            let loaded = ctx.list_loaded_class_ids().len() as i64;
            one(vec![
                ("loadedClassCount", Val::Long(loaded)),
                ("unloadedClassCount", Val::Long(0)),
            ])
        }
        "jdk.ThreadAllocationStatistics" => {
            let thread = ctx.current_thread_object();
            let t = register_thread(ctx, thread);
            let allocated = ctx.total_allocated_bytes().unwrap_or(0) as i64;
            one(vec![
                ("allocated", Val::Long(allocated)),
                ("thread", Val::Ref(t)),
            ])
        }
        other => match crate::jfr_vm_more::periodic(ctx, other, timestamp) {
            Some(v) => v,
            None => return false,
        },
    };
    BACKEND.with(vm, |b| {
        for values in &events {
            append(b, &name, values);
        }
    });
    true
}

/// Every property `System.getProperties()` holds, as `(key, value)`.
fn system_properties(ctx: &mut dyn NativeContext) -> Vec<(String, String)> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let props = match scope.invoke_static(
        "java/lang/System",
        "getProperties",
        "()Ljava/util/Properties;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(p)))) => p,
        _ => return Vec::new(),
    };
    let props = scope.root(props);
    let names = {
        let p = scope.get(&props);
        match scope.invoke_virtual(p, "stringPropertyNames", "()Ljava/util/Set;", &[]) {
            Ok(Some(Value::Object(Some(n)))) => n,
            _ => return Vec::new(),
        }
    };
    let names = scope.root(names);
    let array = {
        let n = scope.get(&names);
        match scope.invoke_virtual(n, "toArray", "()[Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(a)))) => a,
            _ => return Vec::new(),
        }
    };
    let array = scope.root(array);
    let len = {
        let a = scope.get(&array);
        scope.array_length(a)
    };
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let key = {
            let a = scope.get(&array);
            match scope.get_array_element(a, i) {
                Value::Object(Some(k)) => scope.read_string(k),
                _ => None,
            }
        };
        let Some(key) = key else { continue };
        let value = {
            let p = scope.get(&props);
            let k = scope.create_string(&key);
            let k = scope.root(k);
            let kv = scope.get(&k);
            match scope.invoke_virtual(
                p,
                "getProperty",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[Value::Object(Some(kv))],
            ) {
                Ok(Some(Value::Object(Some(v)))) => scope.read_string(v),
                _ => None,
            }
        };
        out.push((key, value.unwrap_or_default()));
    }
    out
}

/// A platform thread is about to disappear: write `jdk.ThreadEnd` when it is
/// wanted, and release the thread's event buffer (flushing what it committed).
pub fn thread_exiting(ctx: &mut dyn NativeContext, thread: ObjectRef) {
    if any_recording() {
        crate::jfr_vm_more::thread_cpu_load_at_exit(ctx, thread);
        thread_ended(ctx, thread);
    }
    let vm = ctx.vm_identity();
    let tid = ctx.thread_id();
    if let Some(root) = crate::jfr_backend::thread_exited(vm, tid) {
        if root != 0 {
            ctx.remove_global_root(root);
        }
    }
}

/// Common tail of the blocking events: thread, optional stack, then `values`.
fn emit_blocking(
    ctx: &mut dyn NativeContext,
    name: &str,
    duration_ticks: i64,
    class: Option<ClassId>,
    extra: Vec<(&str, Val)>,
) {
    let vm = ctx.vm_identity();
    let Some(want_stack) = BACKEND.with(vm, |b| admit(b, name, duration_ticks)) else {
        return;
    };
    let start = now_ticks() - duration_ticks;
    let class_ref = class.map(|c| register_class(ctx, c)).unwrap_or(0);
    let thread = ctx.current_thread_object();
    let t = register_thread(ctx, thread);
    let stack = if want_stack {
        jfr_stack_trace_id(ctx, 1, 0)
    } else {
        0
    };
    let mut all = vec![
        ("startTime", Val::Long(start)),
        ("duration", Val::Long(duration_ticks)),
        ("eventThread", Val::Ref(t)),
        ("stackTrace", Val::Ref(stack)),
    ];
    all.push((
        match name {
            "jdk.ThreadPark" => "parkedClass",
            _ => "monitorClass",
        },
        Val::Ref(class_ref),
    ));
    all.extend(extra);
    BACKEND.with(vm, |b| append(b, name, &fields(all)));
}

/// `jdk.ThreadPark`: a `LockSupport.park*` returned. `timeout_ns` is the
/// caller's (`Long.MIN_VALUE` for an untimed or an absolute park), `until_ms`
/// an absolute park's deadline (`Long.MIN_VALUE` otherwise) and
/// `duration_ticks` the whole park, as HotSpot's `Unsafe_Park` posts them
/// (interpreter round i3 wave 73, lane L6; `until` from wave 79). Wave 79,
/// lane L6 (page `i73-L6-jfr-thread-park-fields-differ-from-hotspot`):
/// `parked_class` / `address` are the thread's `parkBlocker`'s class and
/// address (null and 0 without a blocker), where they were always null / 0.
pub fn thread_parked(
    ctx: &mut dyn NativeContext,
    timeout_ns: i64,
    until_ms: i64,
    parked_class: Option<ClassId>,
    address: i64,
    duration_ticks: i64,
) {
    emit_blocking(
        ctx,
        "jdk.ThreadPark",
        duration_ticks,
        parked_class,
        vec![
            ("timeout", Val::Long(timeout_ns)),
            ("until", Val::Long(until_ms)),
            ("address", Val::Long(address)),
        ],
    );
}

/// `jdk.JavaMonitorWait`: an `Object.wait` returned.
pub fn monitor_waited(
    ctx: &mut dyn NativeContext,
    monitor: ObjectRef,
    timeout_ms: i64,
    timed_out: bool,
    address: i64,
    duration_ticks: i64,
) {
    if !any_recording() {
        return;
    }
    let class = ctx.class_id_of_object(monitor);
    emit_blocking(
        ctx,
        "jdk.JavaMonitorWait",
        duration_ticks,
        Some(class),
        vec![
            ("notifier", Val::Ref(0)),
            ("timeout", Val::Long(timeout_ms)),
            ("timedOut", Val::Bool(timed_out)),
            ("address", Val::Long(address)),
        ],
    );
}

/// `jdk.JavaMonitorEnter`: a contended `monitorenter` acquired.
pub fn monitor_entered(
    ctx: &mut dyn NativeContext,
    monitor: ObjectRef,
    address: i64,
    duration_ticks: i64,
) {
    if !any_recording() {
        return;
    }
    let class = ctx.class_id_of_object(monitor);
    emit_blocking(
        ctx,
        "jdk.JavaMonitorEnter",
        duration_ticks,
        Some(class),
        vec![
            ("previousOwner", Val::Ref(0)),
            ("address", Val::Long(address)),
        ],
    );
}

/// The calling VM thread is ending: free its JFR event buffer (after flushing
/// what it committed) and release its `EventWriter`.
pub fn thread_buffer_released(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    let tid = ctx.thread_id();
    if let Some(root) = crate::jfr_backend::thread_exited(vm, tid) {
        if root != 0 {
            ctx.remove_global_root(root);
        }
    }
}

/// `jdk.VirtualThreadPinned`: a virtual thread blocked while pinned to its carrier.
pub fn virtual_thread_pinned(ctx: &mut dyn NativeContext, operation: &str) {
    virtual_thread_pinned_with(ctx, operation, "Native or VM frame on stack", None);
}

/// [`virtual_thread_pinned`] with HotSpot 25's other fields
/// (`JVM_VirtualThreadPinnedEvent` / `post_vthread_pinned_event`): the event
/// starts at `start_ticks` (the failed freeze; `None` = now, a zero
/// duration), so the threshold sees the pinned park's length; `pinnedReason`
/// is `reason`, and `carrierThread` is the carrier, not the virtual thread.
/// Interpreter round i5 wave 105, lane L6.
pub fn virtual_thread_pinned_with(
    ctx: &mut dyn NativeContext,
    operation: &str,
    reason: &str,
    start_ticks: Option<i64>,
) {
    if !any_recording() {
        return;
    }
    let vm = ctx.vm_identity();
    let now = now_ticks();
    let start = start_ticks.filter(|&t| t <= now).unwrap_or(now);
    let duration = now - start;
    let Some(want_stack) = BACKEND.with(vm, |b| admit(b, "jdk.VirtualThreadPinned", duration))
    else {
        return;
    };
    let thread = ctx.current_thread_object();
    let t = register_thread(ctx, thread);
    let carrier = ctx.current_carrier_thread_object();
    let c = if carrier == thread {
        t
    } else {
        register_thread(ctx, carrier)
    };
    let stack = if want_stack {
        jfr_stack_trace_id(ctx, 1, 0)
    } else {
        0
    };
    BACKEND.with(vm, |b| {
        append(
            b,
            "jdk.VirtualThreadPinned",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                ("stackTrace", Val::Ref(stack)),
                ("blockingOperation", s(operation)),
                ("pinnedReason", s(reason)),
                ("carrierThread", Val::Ref(c)),
            ]),
        );
    });
}

/// `jdk.SyncOnValueBasedClass`: a `monitorenter` on an instance of a
/// value-based class under `-XX:DiagnoseSyncOnValueBasedClasses=2`, as
/// HotSpot's `ObjectSynchronizer::handle_sync_on_value_based_class` commits
/// it (an instant event with the locking thread's stack; field
/// `valueBasedClass`). Interpreter round i5 wave 102, lane L6.
pub fn sync_on_value_based_class(ctx: &mut dyn NativeContext, class_id: ClassId) {
    if !any_recording() {
        return;
    }
    let vm = ctx.vm_identity();
    let Some(want_stack) = BACKEND.with(vm, |b| admit(b, "jdk.SyncOnValueBasedClass", 0)) else {
        return;
    };
    let start = now_ticks();
    let class_ref = register_class(ctx, class_id);
    let thread = ctx.current_thread_object();
    let t = register_thread(ctx, thread);
    let stack = if want_stack {
        jfr_stack_trace_id(ctx, 1, 0)
    } else {
        0
    };
    BACKEND.with(vm, |b| {
        append(
            b,
            "jdk.SyncOnValueBasedClass",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("eventThread", Val::Ref(t)),
                ("stackTrace", Val::Ref(stack)),
                ("valueBasedClass", Val::Ref(class_ref)),
            ]),
        );
    });
}

pub fn ticks_now() -> i64 {
    now_ticks()
}

/// A `FileOutputStream` write shadowed by a native finished: do what
/// `FileOutputStream.traceWrite` does, through the JDK's own event class (its
/// enabled flag, threshold and throttle live in Java).
pub fn file_write(ctx: &mut dyn NativeContext, stream: ObjectRef, bytes: i64, start_ticks: i64) {
    let enabled = matches!(
        ctx.invoke_static("jdk/internal/event/FileWriteEvent", "enabled", "()Z", &[]),
        Ok(Some(Value::Int(v))) if v != 0
    );
    if !enabled {
        return;
    }
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let stream_h = scope.root(stream);
    let stream_v = scope.get(&stream_h);
    let path = scope.get_field_by_name(stream_v, "path");
    let _ = scope.invoke_static(
        "jdk/internal/event/FileWriteEvent",
        "offer",
        "(JLjava/lang/String;J)V",
        &[Value::Long(start_ticks), path, Value::Long(bytes)],
    );
}

/// `Thread.beforeSleep`: the JDK's `ThreadSleepEvent` (or none), kept as a global
/// root until [`thread_sleep_end`]. `0` when no recording runs.
pub fn thread_sleep_begin(ctx: &mut dyn NativeContext, nanos: i64) -> usize {
    if !any_recording() {
        return 0;
    }
    match ctx.invoke_static(
        "java/lang/Thread",
        "beforeSleep",
        "(J)Ljdk/internal/event/ThreadSleepEvent;",
        &[Value::Long(nanos)],
    ) {
        Ok(Some(Value::Object(Some(ev)))) => ctx.add_global_root(ev),
        _ => 0,
    }
}

/// `Thread.afterSleep`.
pub fn thread_sleep_end(ctx: &mut dyn NativeContext, root: usize) {
    if root == 0 {
        return;
    }
    if let Some(ev) = ctx.resolve_global_root(root) {
        let _ = ctx.invoke_static(
            "java/lang/Thread",
            "afterSleep",
            "(Ljdk/internal/event/ThreadSleepEvent;)V",
            &[Value::Object(Some(ev))],
        );
    }
    ctx.remove_global_root(root);
}

/// A `Throwable` was constructed by a native constructor shadow: do what the
/// JDK's constructors do under `jfrTracing`, `ThrowableTracer.trace*`.
pub fn throwable_created(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let class_id = ctx.class_id_of_object(this);
    let is_error = ctx
        .class_id_by_name("java/lang/Error")
        .is_some_and(|e| ctx.is_subclass(class_id, e));
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let mirror = scope.get_class_mirror(class_id);
    let mirror_h = scope.root(mirror);
    let message = {
        let t = scope.get(&this_h);
        scope.get_field_by_name(t, "detailMessage")
    };
    let msg_obj = match message {
        Value::Object(Some(m)) => Some(scope.root(m)),
        _ => None,
    };
    let mirror_v = scope.get(&mirror_h);
    let msg_v = msg_obj.as_ref().map(|h| scope.get(h));
    let _ = scope.invoke_static(
        "jdk/internal/event/ThrowableTracer",
        if is_error {
            "traceError"
        } else {
            "traceThrowable"
        },
        "(Ljava/lang/Class;Ljava/lang/String;)V",
        &[Value::Object(Some(mirror_v)), Value::Object(msg_v)],
    );
}
