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
// This dumper IS reachable on a default build. Production triggers:
// `runtime::interpreter::maybe_dump_heap_on_oom`, gated on
// `VmConfig::heap_dump_on_oom` (`-XX:+HeapDumpOnOutOfMemoryError`), fired at
// most once per VM lifetime; and, since gci i3/hd (2026-10-12), `jcmd <pid>
// GC.heap_dump` / `jmap -dump` through `SharedVm::heap_dump`
// ([`dump_heap_for_dcmd`]) and `jcmd <pid> GC.class_histogram` / `jmap -histo`
// through `SharedVm::class_histogram` ([`class_histogram_for_dcmd`]). Both
// attach commands used to walk the heap with the mutators running
// (`gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-FIXED-20261012`); they now hold
// the dump's own pause, retried while another stop-the-world holds the world,
// and refuse rather than walk unpaused.
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
//    gc-common w2-c (2026-09-23): the wait is the collections' cross-thread
//    JIT take-over (freeze-only), not a plain `wait_for_all` that a
//    poll-free compiled spinner could hang forever.
//  * Object IDs are raw heap addresses. Under a relocating collector two
//    dumps of the same logical object will not agree, and an address recycled
//    after a collection can alias.
//  * No `HPROF_GC_ROOT_JNI_LOCAL` / monitor-used roots are emitted, so MAT's
//    "GC root" attribution is incomplete.
//  * gci i2/jn: a JNI in-native thread's compiled band (the band at the pause,
//    default with the JNI package) is emitted as Java-frame roots when the
//    dump holds its own pause. A peer the dump's take-over FROZE in compiled
//    code contributes no roots at all (its frames are not walked), so what
//    only its frames hold reads as unrooted:
//    `docs/feature-designs/jdk-only-mode.md`.
//    gci i3/hd: such a peer's INTERPRETER frames (Java frames) and its thread
//    field roots (thread block) are now emitted, read as the collections'
//    frozen-peer walk reads them ([`HprofDumper::frozen_peer_roots`]).
//    gci i4/hd: its compiled frames and registers too -- the dump's pause is
//    `NonMovingPause::request_dump_words`, whose take-over probes each frozen
//    peer's words at the freeze instead of declining them
//    ([`HprofDumper::frozen_peer_word_roots`], Java-frame roots).
//    Conservative: a stale word that happens to name an object start roots it
//    in the dump (nothing is retained by that; the dump only names it).
//  * gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`):
//    the walk is a visitor (`VmHeap::for_each_object`; G1 lists nothing
//    first), TLAB `int[]` fillers and objects whose class the store does not
//    hold (dead instances of unloaded classes in an uncollected G1 region, VM
//    wrappers) are not written, and an array larger than a segment streams as
//    its own segment (cut to HotSpot's `u4` limit with HotSpot's warning).
//    A live-only dump on G1 can hold dead objects of an Old region the full
//    collection did not compact (item 7): skipped under the opt-in
//    `CRATONVM_G1_LIVE_WALK_SKIPS_PROVEN_DEAD` ([`walk_heap_for_dcmd`]). Each
//    CLASS_DUMP names its defining loader's object (item 10).

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
const GC_ROOT_THREAD_BLOCK: u8 = 0x06;
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

/// gci i2/jn: may the dump read the JNI in-native bands? Only inside its own
/// pause (outside one a band can end, and its stack unmap, under the read) and
/// with the band at the pause on. Pure.
fn in_native_band_roots_readable(in_pause: bool, band_at_pause: bool) -> bool {
    in_pause && band_at_pause
}

/// gci i4/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
/// item 6): is this class a `GC_ROOT_STICKY_CLASS`? Every class of the boot
/// loader (HotSpot's `ClassLoaderDataGraph` roots), plus the old name rules
/// (`java/`, arrays). Pure.
fn sticky_class_root(name: &str, loader: cratonvm_types::ClassLoaderId) -> bool {
    loader == cratonvm_types::ClassLoaderId::Bootstrap
        || name.starts_with("java/")
        || name.starts_with('[')
}

/// gci i18/dc (`gci-i3hd-heap-walks-and-diagnostics-review-findings` item 5):
/// each thread's HPROF serial, its 1-based position in
/// `ThreadRegistry::all_thread_names` (the serials `write_gc_roots`' thread
/// loops and the STACK_TRACE records use). Pure.
fn thread_serials(names: &[(ThreadId, String)]) -> std::collections::HashMap<ThreadId, u32> {
    names
        .iter()
        .enumerate()
        .map(|(i, (tid, _))| (*tid, u32::try_from(i + 1).unwrap_or(u32::MAX)))
        .collect()
}

/// gci i3/hd: may the dump read the frozen peers' frames? Only inside its own
/// pause (outside one nobody is held, and a running thread's frames move under
/// the read) and only when that pause froze someone. Pure.
fn frozen_peer_roots_readable(in_pause: bool, frozen: usize) -> bool {
    in_pause && frozen > 0
}

/// Dump the heap of `vm` into an HPROF binary file at `path`.
/// Returns the number of bytes written, or an error string.
///
/// `initiator` is the calling thread's id, used to request a stop-the-world
/// pause for the duration of the walk — see [`DumpSafepoint`] (obsaudit D11).
pub fn dump_heap(vm: &Arc<SharedVm>, path: &str, initiator: ThreadId) -> Result<u64, String> {
    dump_heap_waiting(vm, path, initiator, None, false)
}

/// zt t28/mf: [`dump_heap`], gzip-framed when `gzip` (the out-of-memory
/// dump under `-XX:HeapDumpGzipLevel` above 0, from a thread whose own
/// `JvmThread` is not in reach).
pub(crate) fn dump_heap_framed(
    vm: &Arc<SharedVm>,
    path: &str,
    initiator: ThreadId,
    gzip: bool,
) -> Result<u64, String> {
    dump_heap_waiting(vm, path, initiator, None, gzip)
}

/// gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
/// item 3): [`dump_heap`] for the `-XX:+HeapDumpOnOutOfMemoryError` path,
/// whose caller is a mutator the refusing pause counts. `gc_safe_slice`
/// waits one short slice with the caller GC-safe (a blocked region, so the
/// other pause counts it and finishes); a refused request is retried after
/// each slice for up to [`OOM_DUMP_PAUSE_RETRY`], and only then does the
/// dump fall back to its documented unpaused walk.
///
/// gci i10/al: the slice is a closure over the caller's own `&mut JvmThread`
/// (`gc_and_alloc::write_heap_dump_on_oom`); it must not re-derive one from a
/// thread-local pointer while the caller's borrow is live.
///
/// zt t28/mf: gzip-framed when `gzip` (`-XX:HeapDumpGzipLevel` above 0).
pub(crate) fn dump_heap_on_oom(
    vm: &Arc<SharedVm>,
    path: &str,
    initiator: ThreadId,
    gzip: bool,
    gc_safe_slice: &mut dyn FnMut(),
) -> Result<u64, String> {
    dump_heap_waiting(vm, path, initiator, Some(gc_safe_slice), gzip)
}

/// [`dump_heap`] and [`dump_heap_on_oom`]: the file, then the pause
/// ([`DumpSafepoint::request_waiting`]), then the walk.
fn dump_heap_waiting(
    vm: &Arc<SharedVm>,
    path: &str,
    initiator: ThreadId,
    gc_safe_slice: Option<&mut dyn FnMut()>,
    gzip: bool,
) -> Result<u64, String> {
    // gci i3/hd: HotSpot's `HeapDumper::dump_heap` opens the file
    // exclusively (`overwrite` false): an existing file is refused, never
    // truncated. The error is the bare reason, which the caller prints as
    // HotSpot does (`Unable to create <path>: <reason>`); it used to repeat
    // the path (`... : cannot create <path>: ...`).
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| open_error_reason(&e))?;
    let w = BufWriter::new(file);

    // obsaudit D11 (2026-07-26): the walk below used to run with no
    // safepoint at all — see the KNOWN FIDELITY GAPS block at the top of
    // this file for the failure mode (torn field reads / use of an object
    // a concurrent GC has since moved or freed). `DumpSafepoint` requests
    // the same stop-the-world barrier real GC cycles use
    // (`SharedVm::mem::gc_barrier`); its `Drop` releases the barrier with an
    // empty pointer map (nothing here moves any object) even if the walk
    // below panics or returns early, so a dump can never leak the VM in a
    // permanently-paused state.
    let safepoint = DumpSafepoint::request_waiting(vm, initiator, gc_safe_slice);
    write_dump_in(vm, &safepoint, w, gzip, false)
}

/// The dump itself, under `safepoint` (granted or declined), into `w`;
/// gzip-framed when `gzip` (`jcmd GC.heap_dump -gz=<n>`, gci i5/hd).
/// Returns the file's size (the compressed size with `gzip`, as HotSpot's
/// `Heap dump file created [<n> bytes ...]` reports it).
fn write_dump_in(
    vm: &SharedVm,
    safepoint: &DumpSafepoint<'_>,
    mut w: BufWriter<std::fs::File>,
    gzip: bool,
    live_only: bool,
) -> Result<u64, String> {
    let mut dumper = HprofDumper::new(vm);
    dumper.live_only = live_only;
    dumper.skip_spans = sorted_spans(safepoint.reserved_tlab_tails());
    dumper.in_pause = safepoint.pause.is_some();
    dumper.frozen_peer_tids = safepoint.frozen_peer_os_tids();
    dumper.frozen_peer_words = safepoint.frozen_peer_words();
    if !dumper.frozen_peer_tids.is_empty()
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_XT_JIT_ROOT_SCAN")
    {
        eprintln!(
            "[hprof] frozen peers {} compiled-frame words {}",
            dumper.frozen_peer_tids.len(),
            dumper.frozen_peer_words.len()
        );
    }
    if gzip {
        let mut z = GzipStoredWriter::new(&mut w).map_err(|e| format!("write error: {}", e))?;
        dumper
            .write_all(&mut z)
            .map_err(|e| format!("write error: {}", e))?;
        z.finish().map_err(|e| format!("write error: {}", e))?;
    } else {
        dumper
            .write_all(&mut w)
            .map_err(|e| format!("write error: {}", e))?;
    }
    w.flush().map_err(|e| format!("flush error: {}", e))?;
    let size = w
        .into_inner()
        .map_err(|e| format!("flush error: {}", e))?
        .metadata()
        .map_err(|e| format!("metadata error: {}", e))?
        .len();
    Ok(size)
}

/// The reason HotSpot prints after `Unable to create <path>: ` for a failed
/// open (`os::strerror`), for the common kinds; the OS text otherwise. Pure.
fn open_error_reason(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::AlreadyExists => "File exists".to_string(),
        io::ErrorKind::NotFound => "No such file or directory".to_string(),
        io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        _ => e.to_string(),
    }
}

/// How long a heap walk asked for by a thread that is no mutator (`jcmd`'s
/// attach listener) retries its pause while another stop-the-world holds the
/// world (gci i3/hd). A collection that long is itself a hang; the command
/// then fails instead of walking unpaused.
const DCMD_PAUSE_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// gci i9/hj: how long the OOME dump ([`dump_heap_on_oom`]) keeps asking for
/// its pause, one GC-safe slice per refusal, before it falls back to the
/// unpaused walk `DumpSafepoint` documents.
const OOM_DUMP_PAUSE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// `jcmd <pid> GC.heap_dump <file>` / `jmap -dump` (gci i3/hd, page
/// `gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-FIXED-20261012`): this file's
/// dumper inside the dump's own pause, requested as
/// [`crate::runtime::interpreter::PAUSE_OUTSIDER`] (the attach thread is no
/// registered mutator). HotSpot's `HeapDumpDCmd` runs `VM_HeapDumper` at a
/// safepoint; `SharedVm::heap_dump` used to walk with the mutators running.
/// `Err` when the file cannot be created or no pause could be had within
/// [`DCMD_PAUSE_RETRY`].
///
/// gci i5/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings` item 10,
/// and i4/hd open item 6): `gzip` (`-gz=<n>`) writes a gzip file (it wrote an
/// uncompressed one); without `overwrite` the file is opened exclusively, as
/// HotSpot's `HeapDumper` opens it, so a file created after the command's own
/// existence check is refused, not truncated (it used `File::create`).
pub(crate) fn dump_heap_for_dcmd(
    vm: &SharedVm,
    path: &str,
    gzip: bool,
    overwrite: bool,
) -> Result<u64, String> {
    dump_heap_for_dcmd_live(vm, path, gzip, overwrite, false)
}

/// gci i14/hd second pass (item 7): [`dump_heap_for_dcmd`], told whether the
/// dump follows the live-only command's full collection (`live_only`; see
/// [`class_histogram_for_dcmd_live`]).
pub(crate) fn dump_heap_for_dcmd_live(
    vm: &SharedVm,
    path: &str,
    gzip: bool,
    overwrite: bool,
    live_only: bool,
) -> Result<u64, String> {
    let mut open = std::fs::OpenOptions::new();
    open.write(true);
    if overwrite {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    let file = open
        .open(path)
        .map_err(|e| format!("Failed to write heap dump to {}: {}", path, e))?;
    let w = BufWriter::new(file);
    let Some(safepoint) = DumpSafepoint::request_outside(vm, false) else {
        // gci i4/hd: do not leave the empty file behind a refused dump.
        drop(w);
        let _ = std::fs::remove_file(path);
        return Err(dcmd_no_pause_message());
    };
    write_dump_in(vm, &safepoint, w, gzip, live_only)
}

/// gci i47w/dh47: `HotSpotDiagnosticMXBean.dumpHeap` (HotSpot's
/// `jmm_DumpHeap0` -> `HeapDumper::dump`). HotSpot's order: the
/// `DumpWriter` opens `path` exclusively before `VM_HeapDumper` runs, so an
/// existing file fails with the bare `os::strerror` text (`File exists`,
/// [`open_error_reason`]) and nothing is collected; then `collect` when
/// `live` (`_gc_before_heap_dump`); then the walk at the dump's pause, told
/// it follows the live collection. The caller is no mutator (the pause is
/// requested as an outsider, as `jcmd`'s).
pub(crate) fn dump_heap_for_mxbean(
    vm: &SharedVm,
    path: &str,
    live: bool,
    collect: &mut dyn FnMut(),
) -> Result<u64, String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| open_error_reason(&e))?;
    if live {
        collect();
    }
    let w = BufWriter::new(file);
    let Some(safepoint) = DumpSafepoint::request_outside(vm, false) else {
        drop(w);
        let _ = std::fs::remove_file(path);
        return Err(dcmd_no_pause_message());
    };
    write_dump_in(vm, &safepoint, w, false, live)
}

/// The largest stored deflate block (RFC 1951 3.2.4: `LEN` is a `u16`).
const GZIP_STORED_BLOCK: usize = 0xffff;

/// IEEE CRC-32 (reflected, polynomial `0xEDB88320`), as gzip's trailer
/// carries it (RFC 1952 8). One 256-entry table, built at compile time.
const GZIP_CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// The running CRC-32 state `crc` (start at `0xFFFF_FFFF`, finish with `!`)
/// over `bytes`. Pure.
fn gzip_crc_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc = GZIP_CRC_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// gci i5/hd: a gzip stream (RFC 1952) whose deflate body is STORED blocks
/// (RFC 1951 `BTYPE=00`): a valid `.gz` every gzip reader opens (MAT,
/// `GZIPInputStream`, `gunzip`), with no compression crate in the vm crate.
/// HotSpot's `-gz=<level>` compresses; this frames the same bytes
/// uncompressed, so the file is slightly larger than the plain dump (5 bytes
/// per 64 KiB plus 18), never smaller. The level is checked (1-9) by the
/// command and not used here.
///
/// Header: `1f 8b`, CM 8 (deflate), no flags, MTIME 0, XFL 0, OS 255
/// (unknown). Each block: one header byte (`BFINAL` in bit 0, `BTYPE` 00; a
/// stored block starts byte-aligned after the previous one), `LEN` and
/// `!LEN` as little-endian `u16`, then the bytes. Trailer: CRC-32 and the
/// input size mod 2^32, little-endian. [`Self::finish`] writes the final
/// block (possibly empty) and the trailer; a writer dropped without it leaves
/// a truncated stream.
struct GzipStoredWriter<W: Write> {
    inner: W,
    block: Vec<u8>,
    crc: u32,
    size: u32,
}

impl<W: Write> GzipStoredWriter<W> {
    fn new(mut inner: W) -> io::Result<Self> {
        inner.write_all(&[0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff])?;
        Ok(Self {
            inner,
            block: Vec::with_capacity(GZIP_STORED_BLOCK),
            crc: 0xFFFF_FFFF,
            size: 0,
        })
    }

    /// One stored block of the buffered bytes, final or not; empties the
    /// buffer.
    fn emit_block(&mut self, last: bool) -> io::Result<()> {
        let len = u16::try_from(self.block.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stored block too long"))?;
        self.inner.write_all(&[u8::from(last)])?;
        self.inner.write_all(&len.to_le_bytes())?;
        self.inner.write_all(&(!len).to_le_bytes())?;
        self.inner.write_all(&self.block)?;
        self.block.clear();
        Ok(())
    }

    /// The final block and the trailer; hands the inner writer back.
    fn finish(mut self) -> io::Result<W> {
        self.emit_block(true)?;
        let crc = !self.crc;
        self.inner.write_all(&crc.to_le_bytes())?;
        self.inner.write_all(&self.size.to_le_bytes())?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for GzipStoredWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let room = GZIP_STORED_BLOCK - self.block.len();
        let take = room.min(buf.len());
        self.block.extend_from_slice(&buf[..take]);
        self.crc = gzip_crc_update(self.crc, &buf[..take]);
        // Truncation intended: ISIZE is the input size modulo 2^32 (RFC 1952).
        self.size = self.size.wrapping_add(take as u32);
        if self.block.len() == GZIP_STORED_BLOCK {
            self.emit_block(false)?;
        }
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The refusal both attach heap walks answer when no pause could be had.
fn dcmd_no_pause_message() -> String {
    format!(
        "no safepoint within {} s: another stop-the-world pause held the world; \
         the heap is not walked unpaused",
        DCMD_PAUSE_RETRY.as_secs()
    )
}

/// `jcmd <pid> GC.class_histogram` / `jmap -histo` (gci i3/hd, page
/// `gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-FIXED-20261012`): the heap's
/// objects by class, walked inside the dump's own pause (as
/// [`dump_heap_for_dcmd`]), in HotSpot 25's `KlassInfoHisto` order and names:
/// largest total first, ties array classes first then by external name; a
/// class row reads `<external name> (<module>[@<version>])` for a class of a
/// named module, `<external name>` otherwise
/// ([`ClassHistogramEntry::class_name`] carries the whole column). Reserved
/// TLAB tails of frozen peers and humongous fillers are not objects. `Err`
/// when no pause could be had within [`DCMD_PAUSE_RETRY`].
///
/// HotSpot counts LIVE objects unless `-all` (its `VM_GC_HeapInspection` runs
/// a full collection first); this walk counts every object the heap holds,
/// dead ones included: `gci-i3hd-jcmd-heap-walks-count-dead-objects-FIXED-20261013`.
pub(crate) fn class_histogram_for_dcmd(
    vm: &SharedVm,
) -> Result<Vec<crate::runtime::serviceability::ClassHistogramEntry>, String> {
    class_histogram_for_dcmd_live(vm, false)
}

/// gci i14/hd second pass (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
/// item 7): [`class_histogram_for_dcmd`], told whether the walk follows the
/// live-only command's full collection (`live_only`). Then, under the opt-in
/// [`live_walk_skips_proven_dead`], objects the last trusted mark proves dead
/// are not counted (G1: the dead objects of Old regions the full
/// compaction skipped).
pub(crate) fn class_histogram_for_dcmd_live(
    vm: &SharedVm,
    live_only: bool,
) -> Result<Vec<crate::runtime::serviceability::ClassHistogramEntry>, String> {
    let Some(safepoint) = DumpSafepoint::request_outside(vm, true) else {
        return Err(dcmd_no_pause_message());
    };
    // gci i14/hd (item 3): sorted once, searched per object (it was a linear
    // scan of every tail per object: O(objects x parked threads) in the pause).
    let tails = sorted_spans(safepoint.reserved_tlab_tails());
    let heap = &vm.mem.heap;
    // (instance count, bytes) per histogram key, read while the pause holds.
    let mut rows: HashMap<HistoKey, (u64, u64)> = HashMap::new();
    // gci i14/hd (item 4): visited in place, no list of every object first.
    let mut visit = |ptr: *mut u8, size: usize| {
        let addr = ptr as usize;
        if in_sorted_spans(&tails, addr) {
            return;
        }
        // SAFETY: the walk yields object starts, and the pause holds:
        // nothing moves, is freed or is half-written under the read.
        let obj = unsafe { ObjectRef::from_raw(ptr) };
        let header = heap.get_header(obj);
        // gci i14/hd (item 1): a retired TLAB's `int[]` filler is no object.
        if is_tlab_filler(header.class_id, header.kind()) {
            return;
        }
        let key = match header.kind() {
            ObjectKind::Object => HistoKey::Instance(header.class_id.as_u32()),
            ObjectKind::Array => match header.element_type() {
                ArrayElementType::Reference => HistoKey::RefArray(header.class_id.as_u32()),
                et => HistoKey::PrimArray(et as u8),
            },
            ObjectKind::HumongousFiller => return,
        };
        let row = rows.entry(key).or_insert((0, 0));
        row.0 += 1;
        row.1 += size as u64;
    };
    walk_heap_for_dcmd(heap, live_only, &mut visit);
    // Names and modules after the walk, still inside the pause (the class
    // manager lock is never taken by a frozen peer's compiled code, and the
    // walk itself took none).
    let cm = vm.classes.class_manager.read();
    let java_base = cm
        .find_unique_class_by_name("java/lang/Object")
        .and_then(|object| crate::vm::vm_exec::thread_dump_frame_module(&cm, Some(object)));
    let mut entries: Vec<(bool, String, crate::runtime::serviceability::ClassHistogramEntry)> =
        rows.into_iter()
            // gci i14/hd (item 2): a row whose class is gone (a dead instance
            // of an unloaded class, still in an uncollected region) or never
            // was a Java class (a VM wrapper) is not printed; it read
            // `class#<id>`, a row HotSpot never has.
            .filter(|(key, _)| histo_key_has_class(&cm, *key))
            .map(|(key, (count, bytes))| {
                let (external, module) = histo_name_and_module(&cm, key, java_base.as_deref());
                let class_name = match module {
                    Some(m) => format!("{external} ({m})"),
                    None => external.clone(),
                };
                (
                    !matches!(key, HistoKey::Instance(_)),
                    external,
                    crate::runtime::serviceability::ClassHistogramEntry {
                        class_name,
                        instance_count: count,
                        total_bytes: bytes,
                    },
                )
            })
            .collect();
    drop(cm);
    drop(safepoint);
    entries.sort_by(|a, b| {
        hotspot_histogram_order(
            (a.2.total_bytes, a.0, a.1.as_str()),
            (b.2.total_bytes, b.0, b.1.as_str()),
        )
    });
    Ok(entries.into_iter().map(|(_, _, e)| e).collect())
}

/// One row of [`class_histogram_for_dcmd`]: what HotSpot's histogram keys by
/// klass. A reference array's header carries its COMPONENT class id
/// (`anewarray`; see [`HprofDumper::array_class_obj_id`]), so it is keyed
/// apart from that class's instances; a primitive array by its element type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum HistoKey {
    Instance(u32),
    RefArray(u32),
    PrimArray(u8),
}

/// gci i14/hd second pass (item 7): `CRATONVM_G1_LIVE_WALK_SKIPS_PROVEN_DEAD`
/// (default OFF, `runtime_flag_on`; read per walk): a live-only attach walk
/// skips the objects the last trusted completed G1 mark proves dead. Opt-in:
/// it adds the finalizable-closure build and one bitmap probe per Old-region
/// object to the walk.
fn live_walk_skips_proven_dead() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_LIVE_WALK_SKIPS_PROVEN_DEAD")
}

/// The attach walks' object visit (gci i14/hd second pass): everything
/// `VmHeap::for_each_object` yields, or -- for a `live_only` walk under
/// [`live_walk_skips_proven_dead`], when a proof exists -- that minus what the
/// last trusted mark proves dead (`VmHeap::for_each_object_skipping_proven_dead`).
/// Under the switch one stderr census line per live walk:
/// `[hprof] live walk proven-dead skip: proof=<0|1> skipped=<n>`.
fn walk_heap_for_dcmd(
    heap: &crate::memory::VmHeap,
    live_only: bool,
    visit: &mut dyn FnMut(*mut u8, usize),
) {
    if live_only && live_walk_skips_proven_dead() {
        let skipped = heap.for_each_object_skipping_proven_dead(visit);
        eprintln!(
            "[hprof] live walk proven-dead skip: proof={} skipped={}",
            u8::from(skipped.is_some()),
            skipped.unwrap_or(0)
        );
        if skipped.is_some() {
            return;
        }
    }
    heap.for_each_object(visit);
}

/// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016` item
/// 2): does a histogram row's class exist in the class store? An instance's
/// own class, a reference array's component (its header's id). A dead
/// instance of an UNLOADED class -- G1 keeps it in an Old region no pause has
/// collected yet, its layout retained so the walkers can size it -- and a VM
/// wrapper whose synthetic id names no class have none. HotSpot never lists
/// either (its walk skips what the last marking proved dead, and an unloaded
/// klass had no live instance). A primitive array always has one.
fn histo_key_has_class(cm: &crate::classloading::ClassManager, key: HistoKey) -> bool {
    match key {
        HistoKey::Instance(cid) | HistoKey::RefArray(cid) => cm.get_class(ClassId::new(cid)).is_some(),
        HistoKey::PrimArray(_) => true,
    }
}

/// gci i14/hd (item 1): is this a retired TLAB's tail filler -- the `int[]`
/// with the synthetic `TLAB_FILLER_CLASS_ID` every backend stamps over an
/// unused TLAB tail (and Generational over a quarantined copy)? Not an object
/// of the program: G1's region walk yields it like any array, and the heap
/// walks counted it as `[I` and dumped its stale bytes as an `int[]`. Pure.
fn is_tlab_filler(class_id: ClassId, kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Array)
        && class_id.as_u32() == cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID.as_u32()
}

/// gci i14/hd (item 3): `spans` (half-open `[start, end)`, pairwise disjoint,
/// as the reserved TLAB tails are) sorted by start, for [`in_sorted_spans`].
fn sorted_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    spans
}

/// Is `addr` inside one of `spans` (sorted by [`sorted_spans`])? One binary
/// search, where the heap walks scanned every span per object. Pure.
fn in_sorted_spans(spans: &[(usize, usize)], addr: usize) -> bool {
    let after = spans.partition_point(|&(start, _)| start <= addr);
    after > 0 && addr < spans[after - 1].1
}

/// HotSpot's `KlassInfoEntry::compare`: more bytes first; on a tie an array
/// class before an instance class, then by external name (`strcmp`). Each
/// side is `(bytes, is_array, external name)`.
fn hotspot_histogram_order(
    a: (u64, bool, &str),
    b: (u64, bool, &str),
) -> std::cmp::Ordering {
    b.0.cmp(&a.0)
        .then_with(|| b.1.cmp(&a.1))
        .then_with(|| a.2.as_bytes().cmp(b.2.as_bytes()))
}

/// The descriptor letter of a primitive array's element (`[B`, `[I`, ...),
/// from the `ArrayElementType` discriminant (`newarray`'s atype).
fn prim_array_letter(et: u8) -> char {
    match et {
        4 => 'Z',
        5 => 'C',
        6 => 'F',
        7 => 'D',
        8 => 'B',
        9 => 'S',
        10 => 'I',
        11 => 'J',
        _ => '?',
    }
}

/// The external name of a histogram row and its module column: an instance
/// class by its own module; a primitive array in `java.base` (`java_base`, the
/// label of `java/lang/Object`'s module); a reference array by its bottom
/// element class's module, as HotSpot's `ArrayKlass::module` answers.
fn histo_name_and_module(
    cm: &crate::classloading::ClassManager,
    key: HistoKey,
    java_base: Option<&str>,
) -> (String, Option<String>) {
    use crate::runtime::serviceability::external_class_name;
    match key {
        HistoKey::Instance(cid) => {
            let id = ClassId::new(cid);
            match cm.get_class(id) {
                Some(c) => (
                    external_class_name(&c.name),
                    crate::vm::vm_exec::thread_dump_frame_module(cm, Some(id)),
                ),
                None => (format!("class#{cid}"), None),
            }
        }
        HistoKey::PrimArray(et) => (
            format!("[{}", prim_array_letter(et)),
            java_base.map(str::to_string),
        ),
        HistoKey::RefArray(component) => {
            let id = ClassId::new(component);
            let Some(c) = cm.get_class(id) else {
                return (format!("[Lclass#{component};"), None);
            };
            let array_name = array_class_name_for(&c.name);
            let bottom = array_name.trim_start_matches('[');
            let module = if bottom.len() == 1 {
                java_base.map(str::to_string)
            } else if !c.name.starts_with('[') {
                crate::vm::vm_exec::thread_dump_frame_module(cm, Some(id))
            } else {
                bottom
                    .strip_prefix('L')
                    .and_then(|b| b.strip_suffix(';'))
                    .and_then(|b| cm.find_unique_class_by_name(b))
                    .and_then(|b| crate::vm::vm_exec::thread_dump_frame_module(cm, Some(b)))
            };
            (external_class_name(&array_name), module)
        }
    }
}

/// RAII stop-the-world pause for an HPROF dump (obsaudit D11).
///
/// Requests the barrier the way every collection does and waits with the same
/// cross-thread JIT take-over (`runtime::interpreter::NonMovingPause`,
/// freeze-only). gc-common w2-c (2026-09-23),
/// `common-a-hprof-dump-plain-wait-for-all`: this used a plain
/// `wait_for_all` on the argument that compiled backward branches poll. They
/// do only while `CRATONVM_JIT_SAFEPOINT_POLLS` is on and only in loops the JIT
/// emitted a poll into; a peer spinning anywhere else never arrived, and the
/// `-XX:+HeapDumpOnOutOfMemoryError` path hung with every other mutator parked
/// instead of dumping. The take-over's job here is only to FREEZE such a peer:
/// nothing moves, so no frozen register can go stale, and nothing is marked, so
/// its roots are not gathered. What a frozen peer does leave behind is an
/// un-retired TLAB tail, which the walk must not report as objects -- see
/// [`HprofDumper::skip_spans`].
///
/// The request (`NonMovingPause::request_dump_words`) can decline (`None`) if
/// another stop-the-world pause is already in flight — e.g. a real GC cycle
/// racing the OOM path that triggered this dump. Rather than block trying to
/// join someone else's pause (real GC's own coordination protocol is
/// considerably more involved than this diagnostic needs), a declined
/// request falls back to the pre-D11 behaviour: dump without a pause. That
/// preserves this path's original best-effort characteristic — a torn dump
/// on a benign race is still strictly better than the OOM handler itself
/// blocking or erroring.
///
/// gci i9/hj: the OOME path ([`dump_heap_on_oom`]) now first waits the other
/// pause out GC-safe, in slices, for up to [`OOM_DUMP_PAUSE_RETRY`]; only a
/// pause still refused after that takes the unpaused fallback.
///
/// Release (skip-span retire, frozen-peer resume, `complete_gc` with the empty
/// map -- the exact answer for a pause in which nothing moved) is the
/// `NonMovingPause`'s own `Drop`, so it runs on an early error or a panic too.
struct DumpSafepoint<'a> {
    pause: Option<crate::runtime::interpreter::NonMovingPause<'a>>,
}

impl<'a> DumpSafepoint<'a> {
    /// gci i4/hd: both requests keep the frozen peers' compiled words
    /// (`NonMovingPause::request_dump_words`), the half of
    /// `gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012` the
    /// interpreter-frame read (i3/hd) left open.
    ///
    /// gci i9/hj: with `gc_safe_slice` (the OOME dump, [`dump_heap_on_oom`])
    /// a refused request is no longer the immediate unpaused fallback: the
    /// caller is a mutator the refusing pause may be waiting for, so it waits
    /// one GC-safe slice and asks again, for up to [`OOM_DUMP_PAUSE_RETRY`].
    /// Without one (a caller that cannot go GC-safe, the unit tests) a single
    /// request, as before: a retry while counted only stalls the other pause.
    fn request_waiting(
        vm: &'a SharedVm,
        initiator: ThreadId,
        mut gc_safe_slice: Option<&mut dyn FnMut()>,
    ) -> Self {
        let deadline = std::time::Instant::now() + OOM_DUMP_PAUSE_RETRY;
        let waits = gc_safe_slice.is_some();
        let mut refusals: u32 = 0;
        let pause = loop {
            if let Some(pause) = crate::runtime::interpreter::NonMovingPause::request_dump_words(vm, initiator) {
                break Some(pause);
            }
            refusals = refusals.saturating_add(1);
            let Some(slice) = gc_safe_slice.as_deref_mut() else {
                break None;
            };
            if std::time::Instant::now() >= deadline {
                break None;
            }
            slice();
        };
        // gci i9/hj: census line (engagement of the retry; stderr only).
        if waits && refusals > 0 && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OOM_DUMP_PAUSE") {
            eprintln!(
                "[hprof] oome dump pause refusals={refusals} granted={}",
                u8::from(pause.is_some())
            );
        }
        Self { pause }
    }

    /// gci i3/hd: the pause for a heap walk asked for by a thread that is no
    /// mutator (`jcmd`'s attach listener, initiator `PAUSE_OUTSIDER`). Unlike
    /// [`Self::request_waiting`] (the OOME path, which keeps its best-effort unpaused
    /// fallback), a declined request is retried in 1 ms steps for up to
    /// [`DCMD_PAUSE_RETRY`]; `None` after that, and the caller refuses.
    ///
    /// gci i4/hd: `inspection` (the class histogram) takes the
    /// `heap-inspection` pause, which keeps no frozen-peer words (the
    /// histogram names no roots); the dump takes the dump's.
    fn request_outside(vm: &'a SharedVm, inspection: bool) -> Option<Self> {
        use crate::runtime::interpreter::{NonMovingPause, PAUSE_OUTSIDER};
        let deadline = std::time::Instant::now() + DCMD_PAUSE_RETRY;
        loop {
            let pause = if inspection {
                NonMovingPause::request_heap_inspection(vm, PAUSE_OUTSIDER)
            } else {
                NonMovingPause::request_dump_words(vm, PAUSE_OUTSIDER)
            };
            if let Some(pause) = pause {
                return Some(Self { pause: Some(pause) });
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// gci i3/hd: the OS tids of the peers this pause froze without a
    /// precise deposit (`NonMovingPause::frozen_unrewritable_os_tids`); empty
    /// when the request was declined.
    fn frozen_peer_os_tids(&self) -> Vec<u32> {
        self.pause
            .as_ref()
            .map(|p| p.frozen_unrewritable_os_tids())
            .unwrap_or_default()
    }

    /// gci i4/hd: the objects the pause's take-over found in the frozen
    /// peers' registers and stack bands (`NonMovingPause::frozen_peer_words`);
    /// empty when the request was declined or nobody was frozen.
    fn frozen_peer_words(&self) -> Vec<ObjectRef> {
        self.pause
            .as_ref()
            .map(|p| p.frozen_peer_words().to_vec())
            .unwrap_or_default()
    }

    /// The reserved TLAB tails to exclude from the walk; empty when the request
    /// was declined (no pause, so nobody was frozen by us).
    fn reserved_tlab_tails(&self) -> Vec<(usize, usize)> {
        self.pause
            .as_ref()
            .map(|p| p.reserved_tlab_tails())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// HprofDumper — orchestrates the dump phases
// ---------------------------------------------------------------------------

pub struct HprofDumper<'a> {
    /// gci i3/hd: a plain `&SharedVm` (was `&Arc<SharedVm>`), so the attach
    /// commands, which hold only `&SharedVm`, run this dumper too; an
    /// `&Arc<SharedVm>` argument still coerces.
    vm: &'a SharedVm,
    /// Interned string table: string → id
    strings: HashMap<String, u64>,
    next_string_id: u64,
    /// ClassId → serial number (1-based)
    class_serials: HashMap<ClassId, u32>,
    next_serial: u32,
    /// Reserved-but-unallocated TLAB tails `[start, end)` at the dump's pause
    /// (gc-common w2-c). A peer the pause FROZE in compiled code never retired
    /// its TLAB, so its tail is raw memory: G1's and ZGC's walks already skip
    /// it, Generational's young walk does not consult the published spans and
    /// can report zeroed or stale bytes there as objects. Anything the walk
    /// yields inside one is dropped. Sorted by start (gci i14/hd,
    /// [`sorted_spans`]): [`Self::in_skip_span`] searches it.
    skip_spans: Vec<(usize, usize)>,
    /// gci i2/jn (`gci-i1jn-heap-dump-omits-in-native-band-roots-FIXED-20261012`):
    /// the dump holds its own stop-the-world pause ([`DumpSafepoint`] was
    /// granted). Only then may it read the JNI in-native bands
    /// (`ThreadRegistry::scan_in_native_bands`' contract: a thread in native
    /// cannot leave it while the pause holds, so its band stays frozen and
    /// mapped). `false` for a declined request and for a bare `new`.
    in_pause: bool,
    /// gci i3/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
    /// the OS tids of the peers the dump's own pause froze without a precise
    /// deposit. Their deposited snapshot does not speak for them, so their
    /// interpreter frames and thread fields are read directly
    /// ([`Self::frozen_peer_roots`]). Read only while [`Self::in_pause`].
    frozen_peer_tids: Vec<u32>,
    /// gci i4/hd: what those peers' registers and stack bands named at the
    /// freeze (the take-over's probe: exact bases and derived pointers), the
    /// compiled frames' share of their roots. Emitted as Java-frame roots
    /// ([`Self::frozen_peer_word_roots`]); read only while [`Self::in_pause`].
    frozen_peer_words: Vec<ObjectRef>,
    /// gci i4/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
    /// item 4): per class, its instance fields' HPROF types, one group per
    /// class of its chain, root first (heap slot order). Built once per class
    /// under one class-manager read; the instance loop used to take the lock
    /// and build the chain for every object, inside the pause.
    layout_cache: std::cell::RefCell<HashMap<ClassId, std::rc::Rc<Vec<Vec<u8>>>>>,
    /// gci i4/hd (item 4): component class -> array class object id, for
    /// [`Self::array_class_obj_id`] (it took the lock and formatted the array
    /// name for every reference array).
    array_class_cache: std::cell::RefCell<HashMap<ClassId, u64>>,
    /// gci i14/hd second pass (item 7): this dump follows the live-only
    /// command's full collection (`GC.heap_dump` without `-all`), so the walk
    /// may skip what the last trusted mark proves dead
    /// ([`walk_heap_for_dcmd`]). `false` for the OOME dump and a bare `new`.
    live_only: bool,
}

impl<'a> HprofDumper<'a> {
    pub fn new(vm: &'a SharedVm) -> Self {
        Self {
            vm,
            strings: HashMap::new(),
            next_string_id: 1,
            class_serials: HashMap::new(),
            next_serial: 1,
            skip_spans: Vec::new(),
            in_pause: false,
            frozen_peer_tids: Vec::new(),
            frozen_peer_words: Vec::new(),
            layout_cache: std::cell::RefCell::new(HashMap::new()),
            array_class_cache: std::cell::RefCell::new(HashMap::new()),
            live_only: false,
        }
    }

    /// Is `addr` inside a reserved TLAB tail of this dump's pause? gci i14/hd
    /// (item 3): a binary search over [`Self::skip_spans`], which are set
    /// sorted ([`sorted_spans`]); it scanned every span per object.
    fn in_skip_span(&self, addr: usize) -> bool {
        in_sorted_spans(&self.skip_spans, addr)
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
        // zt t18/df7 (x13 item 21a): every class walk of the dump skips an
        // unbound or withdrawn define (`ClassStore::iter_named`), as HotSpot's
        // dump lists only loaded classes.
        for class in cm.class_store.iter_named() {
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
        for class in cm.class_store.iter_named() {
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

        // 5c: walk heap objects. gci i14/hd: visited in place (item 4: no
        // list of every object first), skipping TLAB fillers (item 1) and
        // objects whose class is not in the store (item 2: no record may
        // name a class object the dump has no CLASS_DUMP for), and writing
        // an array larger than a segment as its own streamed segment (item
        // 5). The class ids are read once, before the walk.
        let mut known: std::collections::HashSet<u32> = std::collections::HashSet::new();
        {
            let cm = self.vm.classes.class_manager.read();
            known.extend(cm.class_store.iter_named().map(|c| c.id.as_u32()));
        }
        let this: &Self = self;
        let mut result: io::Result<()> = Ok(());
        // gci i14/hd second pass (item 7): a live-only dump may skip what the
        // last trusted mark proves dead (`walk_heap_for_dcmd`, opt-in).
        walk_heap_for_dcmd(&this.vm.mem.heap, this.live_only, &mut |ptr: *mut u8, _size: usize| {
            if result.is_ok() {
                result = this.write_walked_object(&mut seg, &mut *w, ptr, &known);
            }
        });
        result?;

        // Final flush
        seg.flush(w)?;
        Ok(())
    }

    /// One object of the heap walk (gci i14/hd, the loop body of
    /// [`Self::write_heap_segments`]): its sub-record into `seg`, flushed
    /// past [`MAX_SEGMENT_SIZE`]; an array whose sub-record alone exceeds
    /// that is written straight to `w` as its own segment
    /// ([`Self::write_array_segment_streamed`]: a G1 humongous array no longer
    /// grows `seg` to its own size). Nothing for a reserved TLAB tail, a TLAB
    /// filler, a humongous filler, or an object (or reference array) whose
    /// class `known` does not hold.
    fn write_walked_object<W: Write>(
        &self,
        seg: &mut SegmentBuilder,
        w: &mut W,
        ptr: *mut u8,
        known: &std::collections::HashSet<u32>,
    ) -> io::Result<()> {
        if self.in_skip_span(ptr as usize) {
            return Ok(());
        }
        // SAFETY: the walk yields object starts, and the dump holds its
        // stop-the-world pause (`DumpSafepoint`; obsaudit D11), so nothing
        // moves, is freed or is half-written under the read. See the KNOWN
        // FIDELITY GAPS block at the top of this file.
        let obj = unsafe { ObjectRef::from_raw(ptr) };
        let header = self.vm.mem.heap.get_header(obj);
        let class_id = header.class_id;
        let kind = header.kind();
        if is_tlab_filler(class_id, kind) {
            return Ok(());
        }
        match kind {
            ObjectKind::Object => {
                if !known.contains(&class_id.as_u32()) {
                    return Ok(());
                }
                self.write_instance_dump_with_values(seg, obj);
            }
            ObjectKind::Array => {
                let et = header.element_type();
                let reference = matches!(et, ArrayElementType::Reference);
                if reference && !known.contains(&class_id.as_u32()) {
                    return Ok(());
                }
                let length = header.array_length() as usize;
                if array_subrecord_len(et, length) > MAX_SEGMENT_SIZE {
                    seg.flush(w)?;
                    return self.write_array_segment_streamed(w, obj);
                }
                if reference {
                    self.write_obj_array_dump(seg, obj);
                } else {
                    self.write_prim_array_dump(seg, obj);
                }
            }
            // Round-9 gc CRIT-1: humongous-continuation filler is a walker
            // sentinel, never a real object.
            ObjectKind::HumongousFiller => return Ok(()),
        }
        if seg.len() >= MAX_SEGMENT_SIZE {
            seg.flush(w)?;
        }
        Ok(())
    }

    /// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
    /// item 5): one array as a HEAP_DUMP_SEGMENT of its own, written to `w`
    /// in bounded chunks ([`STREAM_CHUNK_BYTES`]) -- the same sub-record
    /// bytes [`Self::write_prim_array_dump`] / [`Self::write_obj_array_dump`]
    /// build in memory. A primitive payload is read in place
    /// (`VmHeap::array_data_ptr`, contiguous on every backend, a G1
    /// humongous span included) instead of one element accessor call (and,
    /// for a humongous array on G1, one region-lock round trip) per element.
    /// An array whose sub-record cannot fit a `u4` record length is cut to
    /// HotSpot's `calculate_array_max_length` with HotSpot's warning on
    /// stderr; the segment length used to be truncated by an `as u32`, which
    /// broke every record after it.
    fn write_array_segment_streamed<W: Write>(&self, w: &mut W, obj: ObjectRef) -> io::Result<()> {
        let heap = &self.vm.mem.heap;
        let header = heap.get_header(obj);
        let et = header.element_type();
        let length = header.array_length() as usize;
        let (head, elem) = array_subrecord_shape(et);
        let n = hprof_array_max_length(length, elem, head);
        if n < length {
            eprintln!(
                "OpenJDK 64-Bit Server VM warning: cannot dump array of type {}[] with length {}; truncating to length {}",
                hotspot_array_type_name(et),
                length,
                n
            );
        }
        let reference = matches!(et, ArrayElementType::Reference);
        let body = head + n * elem;
        let mut out: Vec<u8> = Vec::with_capacity(STREAM_CHUNK_BYTES + 64);
        out.push(HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
        out.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        // `hprof_array_max_length` keeps `body` within a u4.
        out.extend_from_slice(&u32::try_from(body).unwrap_or(u32::MAX).to_be_bytes());
        out.push(if reference { GC_OBJ_ARRAY_DUMP } else { GC_PRIM_ARRAY_DUMP });
        out.extend_from_slice(&(obj.as_ptr() as u64).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
        out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        if reference {
            out.extend_from_slice(&self.array_class_obj_id(header.class_id).to_be_bytes());
        } else {
            out.push(array_element_to_hprof(et));
        }
        let per_chunk = (STREAM_CHUNK_BYTES / elem).max(1);
        let data = heap.array_data_ptr(obj);
        let mut i = 0usize;
        while i < n {
            let end = (i + per_chunk).min(n);
            match (reference, data) {
                (false, Some(base)) => {
                    // SAFETY: `i < end <= n <= length`, so the `end - i`
                    // elements from `base + i * elem` lie inside the array's
                    // contiguous payload (`array_data_ptr`'s contract), and
                    // the pause holds it in place.
                    unsafe { push_prim_elements_be(base.add(i * elem), elem, end - i, &mut out) };
                }
                _ => {
                    for k in i..end {
                        encode_array_element(heap, obj, et, k, &mut out);
                    }
                }
            }
            w.write_all(&out)?;
            out.clear();
            i = end;
        }
        w.write_all(&out)?;
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
        // thousands of threads, on the OOM path. Hoisted out.
        //
        // gci i3/hd: the serials are `all_thread_names`' order (the STACK_TRACE
        // records' too), but the objects came from `alive_thread_objects`, a
        // second, FILTERED list (alive entries with a mirror only) indexed in
        // parallel: after one dead or mirror-less entry every later thread
        // object was attributed to the wrong serial. Each serial now names its
        // own thread's mirror, and only while that thread is alive.
        let registry = &self.vm.threads.thread_registry;
        let thread_names = registry.all_thread_names();
        for (i, (tid, _)) in thread_names.iter().enumerate() {
            let thread_serial = (i + 1) as u32;
            if !registry.is_alive(*tid) {
                continue;
            }
            if let Some(obj) = registry.java_thread_obj(*tid) {
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

        // Thread stack frame roots (GC root snapshots). gci i5/hd (review
        // item 5): each alive thread's own snapshot names that thread's
        // serial (every Java-frame root used to name serial 1, so MAT gave
        // them all to the first thread). What the gather adds beyond the
        // snapshots (mirrors, monitor and synchronizer slots) keeps serial 1,
        // as before: the root SET is unchanged.
        // addr-keyed: local to this root segment (one dump, under the dump's own pause).
        let mut attributed: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (i, (tid, _)) in thread_names.iter().enumerate() {
            if !registry.is_alive(*tid) {
                continue;
            }
            // Truncation: a thread serial is a u32, as in the loop above.
            let thread_serial = (i + 1) as u32;
            for obj in registry.root_snapshot_of(*tid) {
                attributed.insert(obj.as_ptr() as usize);
                seg.push_u8(GC_ROOT_JAVA_FRAME);
                seg.push_u64(obj.as_ptr() as u64);
                seg.push_u32(thread_serial);
                seg.push_u32(0xFFFF_FFFF); // frame depth = unknown
            }
        }
        let roots = registry.collect_all_root_snapshots();
        for obj in &roots {
            if attributed.contains(&(obj.as_ptr() as usize)) {
                continue;
            }
            seg.push_u8(GC_ROOT_JAVA_FRAME);
            seg.push_u64(obj.as_ptr() as u64);
            seg.push_u32(1); // thread serial
            seg.push_u32(0xFFFF_FFFF); // frame depth = unknown
        }
        // gci i18/dc (review item 5's remainder): the in-native band and
        // frozen-peer roots name their own thread's serial too (they named
        // serial 1); a thread the serial list does not hold keeps serial 1.
        let serials = thread_serials(&thread_names);
        let serial_of = |tid: ThreadId| serials.get(&tid).copied().unwrap_or(1);
        // gci i2/jn: the compiled frames a JNI in-native thread left to the
        // pause (the band at the pause, default with the JNI package) are in
        // no snapshot above; the collector reads them from the band
        // (`gc_and_alloc::append_in_native_band_roots`), so does the dump.
        for &(tid, obj) in &self.in_native_band_roots() {
            seg.push_u8(GC_ROOT_JAVA_FRAME);
            seg.push_u64(obj.as_ptr() as u64);
            seg.push_u32(serial_of(tid)); // thread serial
            seg.push_u32(0xFFFF_FFFF); // frame depth = unknown
        }
        // JIT round 23 wave 1 L7: a blocked thread whose deposit deferred its
        // compiled-chain walk (`CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`, default
        // on) has those frames' roots in no snapshot above; the collections
        // resolve the record in their root gathering, so does the dump.
        for &(tid, obj) in &self.deferred_blocked_chain_roots() {
            seg.push_u8(GC_ROOT_JAVA_FRAME);
            seg.push_u64(obj.as_ptr() as u64);
            seg.push_u32(serial_of(tid)); // thread serial
            seg.push_u32(0xFFFF_FFFF); // frame depth = unknown
        }
        // gci i3/hd: a peer the dump's take-over froze deposited nothing for
        // this pause; its interpreter frames are Java-frame roots and its
        // thread fields thread-block roots, as the collections read them.
        let (frame_roots, field_roots) = self.frozen_peer_roots();
        for &(tid, obj) in &frame_roots {
            seg.push_u8(GC_ROOT_JAVA_FRAME);
            seg.push_u64(obj.as_ptr() as u64);
            seg.push_u32(serial_of(tid)); // thread serial
            seg.push_u32(0xFFFF_FFFF); // frame depth = unknown
        }
        for &(tid, obj) in &field_roots {
            seg.push_u8(GC_ROOT_THREAD_BLOCK);
            seg.push_u64(obj.as_ptr() as u64);
            seg.push_u32(serial_of(tid)); // thread serial
        }
        // gci i4/hd: and its compiled frames and registers, as the dump's
        // take-over read them at the freeze (HotSpot names a compiled frame's
        // oops as Java-frame roots too). Still serial 1 (gci i18/dc): the
        // take-over's word list carries no owner (`NonMovingPause::dump_words`).
        for obj in &self.frozen_peer_word_roots() {
            seg.push_u8(GC_ROOT_JAVA_FRAME);
            seg.push_u64(obj.as_ptr() as u64);
            seg.push_u32(1); // thread serial
            seg.push_u32(0xFFFF_FFFF); // frame depth = unknown
        }

        // Sticky class roots (system classes). gci i4/hd (review item 6):
        // HotSpot roots every class of the boot loader, not only `java/`
        // ones (`jdk/`, `sun/`, `javax/` boot classes' statics read as
        // unrooted); the name rules stay for whatever the store files
        // under another loader id.
        let cm = self.vm.classes.class_manager.read();
        for class in cm.class_store.iter_named() {
            if sticky_class_root(&class.name, class.loader_id) {
                seg.push_u8(GC_ROOT_STICKY_CLASS);
                seg.push_u64(class_obj_id_for(class.id));
            }
        }
    }

    /// gci i2/jn (`gci-i1jn-heap-dump-omits-in-native-band-roots-FIXED-20261012`):
    /// the roots of every JNI in-native band, probed as the pause probes them
    /// (`takeover_word_probe` plus its companion). Empty unless the dump holds
    /// its own pause ([`Self::in_pause`]) and the band at the pause is on
    /// (`in_native_band_at_pause_enabled`: the JNI package, its default); a
    /// thread records a band on Generational heaps only.
    ///
    /// gci i18/dc: each root with the thread that owns its band
    /// (`ThreadRegistry::scan_in_native_bands_by_owner`), a word's companion
    /// with the word's thread.
    fn in_native_band_roots(&self) -> Vec<(ThreadId, ObjectRef)> {
        let mut out: Vec<(ThreadId, ObjectRef)> = Vec::new();
        if !in_native_band_roots_readable(
            self.in_pause,
            crate::native::jni::in_native_band_at_pause_enabled_for(&self.vm),
        ) {
            return out;
        }
        use crate::jit::xt_root_scan as xt;
        let heap = &self.vm.mem.heap;
        let companions = std::cell::RefCell::new(Vec::<ObjectRef>::new());
        let probe = |a: usize| {
            let r = xt::takeover_word_probe(heap, a);
            if let Some(c) = xt::takeover_word_companion(heap, a, r) {
                companions.borrow_mut().push(c);
            }
            r
        };
        let mut each = |tid: ThreadId, found: Vec<ObjectRef>| {
            out.extend(found.into_iter().map(|o| (tid, o)));
            out.extend(companions.borrow_mut().drain(..).map(|o| (tid, o)));
        };
        self.vm
            .threads
            .thread_registry
            .scan_in_native_bands_by_owner(&probe, &mut each);
        out
    }

    /// JIT round 23 wave 1 L7 (the lazy blocked deposit's HPROF residual,
    /// `r21-blocking-native-deposits-an-eager-jit-band-scan-FIXED-20261009`):
    /// the compiled-frame roots of every thread blocked with a deferred chain
    /// record, each with its thread. Only inside the dump's own pause: the
    /// pause holds a blocked thread in its window, so the frames the record
    /// names cannot change or unwind while the walk reads them. A root inside
    /// a reserved TLAB tail is not an object the dump writes, and is left out.
    fn deferred_blocked_chain_roots(&self) -> Vec<(ThreadId, ObjectRef)> {
        let mut out: Vec<(ThreadId, ObjectRef)> = Vec::new();
        // `CRATONVM_GC_HPROF_DEFERRED_CHAIN_ROOTS` (default ON, `=0` kill
        // switch), read once per dump.
        if !self.in_pause
            || !cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_HPROF_DEFERRED_CHAIN_ROOTS",
            )
        {
            return out;
        }
        let registry = &self.vm.threads.thread_registry;
        for (tid, _) in registry.all_thread_names() {
            let Some(arcs) = registry.starting_thread_shared(tid) else {
                continue;
            };
            for obj in crate::jit::conservative_roots::deferred_blocked_jit_chain_roots(
                &self.vm.mem.heap,
                &arcs.gc_block_state,
            ) {
                if !self.in_skip_span(obj.as_ptr() as usize) {
                    out.push((tid, obj));
                }
            }
        }
        out
    }

    /// gci i4/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`,
    /// the compiled half): the objects the dump's own take-over found in the
    /// frozen peers' registers and stack bands
    /// ([`Self::frozen_peer_words`]), minus any inside a reserved TLAB tail
    /// (not an object the dump writes). Empty under the same rule as
    /// [`Self::frozen_peer_roots`].
    fn frozen_peer_word_roots(&self) -> Vec<ObjectRef> {
        if !frozen_peer_roots_readable(self.in_pause, self.frozen_peer_tids.len()) {
            return Vec::new();
        }
        self.frozen_peer_words
            .iter()
            .copied()
            .filter(|o| !self.in_skip_span(o.as_ptr() as usize))
            .collect()
    }

    /// gci i3/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
    /// `(frame roots, thread-field roots)` of every peer the dump's own pause
    /// froze without a precise deposit ([`Self::frozen_peer_tids`]). The same
    /// read the collections' frozen-peer walk does in
    /// `gc_and_alloc::stw_take_over_and_wait` (the XT-FRAME-SCAN block): each
    /// interpreter frame's locals, its operand stack screened by
    /// `is_heap_addr`, its exit and block monitors; then every `JvmThread`
    /// field root (`roots::push_off_frame_thread_roots`). Both empty outside
    /// the dump's own pause or when nobody was frozen. Compiled frames and
    /// registers are read by the take-over at the freeze
    /// ([`Self::frozen_peer_word_roots`], gci i4/hd).
    ///
    /// gci i18/dc: each root with the peer it was read from (its
    /// `JvmThread::thread_id`), for the dump's thread serial.
    fn frozen_peer_roots(&self) -> (Vec<(ThreadId, ObjectRef)>, Vec<(ThreadId, ObjectRef)>) {
        let mut owned_frames: Vec<(ThreadId, ObjectRef)> = Vec::new();
        let mut owned_fields: Vec<(ThreadId, ObjectRef)> = Vec::new();
        if !frozen_peer_roots_readable(self.in_pause, self.frozen_peer_tids.len()) {
            return (owned_frames, owned_fields);
        }
        let mut frames: Vec<ObjectRef> = Vec::new();
        let mut fields: Vec<ObjectRef> = Vec::new();
        let shared = self.vm;
        let addrs = shared
            .threads
            .thread_registry
            .frozen_peer_thread_addrs(&self.frozen_peer_tids);
        for addr in addrs {
            // SAFETY: as in the collections' frozen-peer walk: the address was
            // published by its owning thread (stable for the thread's life,
            // cleared before the `JvmThread` drops), and the peer is held by
            // this dump's take-over (OS-frozen in compiled code, or claimed in
            // C) until the pause's `Drop` resumes it, after this read. Neither
            // compiled code nor C mutates the interpreter state, and a held
            // thread cannot exit.
            let peer: &crate::threading::jvm_thread::JvmThread =
                unsafe { &*(addr as *const crate::threading::jvm_thread::JvmThread) };
            for frame in peer.frames.iter() {
                frame.scan_local_objects_for_vm(shared, &mut frames);
                let sb = frames.len();
                frame.stack.scan_object_refs_ruled(
                    &mut frames,
                    &shared.mem.heap,
                    crate::runtime::value_stack::MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
                );
                // The deposit path's operand-stack screen: a pointer-shaped
                // primitive is not a root.
                let mut keep = sb;
                let mut i = sb;
                while i < frames.len() {
                    let o = frames[i];
                    if shared.mem.heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                        frames[keep] = o;
                        keep += 1;
                    }
                    i += 1;
                }
                frames.truncate(keep);
                if let Some(m) = frame.monitor_on_exit {
                    frames.push(m);
                }
                frames.extend_from_slice(frame.held_monitors.as_slice());
            }
            crate::memory::roots::push_off_frame_thread_roots(peer, &mut fields);
            let owner = peer.thread_id;
            owned_frames.extend(frames.drain(..).map(|o| (owner, o)));
            owned_fields.extend(fields.drain(..).map(|o| (owner, o)));
        }
        (owned_frames, owned_fields)
    }

    // ---- CLASS_DUMP sub-records -----------------------------------------

    fn write_class_dumps(&mut self, seg: &mut SegmentBuilder) {
        let cm = self.vm.classes.class_manager.read();
        let statics = self.vm.classes.statics.read();
        // gci i14/hd second pass (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
        // item 10): HotSpot writes the defining loader's object id (0 for the
        // boot loader); every CLASS_DUMP said 0. Read from the class's mirror
        // (`java.lang.Class.classLoader`), as the JVMTI walk reads it.
        let loader_slot = cm
            .find_class_by_name_for_loader("java/lang/Class", crate::classloading::ClassLoaderId::Bootstrap)
            .and_then(|c| crate::vm::vm_exec::resolve_field_index_in_hierarchy(c, "classLoader", &cm.class_store));
        let mirrors = self.vm.classes.class_mirrors.read();

        for class in cm.class_store.iter_named() {
            let class_obj_id = class_obj_id_for(class.id);
            let super_obj_id = class.superclass.map(class_obj_id_for).unwrap_or(0);
            let loader_obj_id =
                class_loader_obj_id(&self.vm.mem.heap, mirrors.get(&class.id).copied(), loader_slot);

            // Compute instance size in HPROF terms (sum of field byte sizes)
            let instance_fields: Vec<_> = class.fields.iter().filter(|f| !f.is_static()).collect();
            let static_fields: Vec<_> = class.fields.iter().filter(|f| f.is_static()).collect();

            // Instance size: all inherited + own instance fields' byte sizes
            let instance_byte_size = compute_instance_byte_size(class.id, &cm.class_store);

            seg.push_u8(GC_CLASS_DUMP);
            seg.push_u64(class_obj_id); // class object ID
            seg.push_u32(0); // stack trace serial
            seg.push_u64(super_obj_id); // super class object ID
            seg.push_u64(loader_obj_id); // classloader object ID (gci i14/hd)
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

        // gci i14/hd: the one element encoder, shared with the streamed
        // segment (`write_array_segment_streamed`).
        for i in 0..length {
            encode_array_element(
                &self.vm.mem.heap,
                obj,
                ArrayElementType::Reference,
                i,
                &mut seg.buf,
            );
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
        if let Some(&id) = self.array_class_cache.borrow().get(&component_class_id) {
            return id;
        }
        let id = self.array_class_obj_id_uncached(component_class_id);
        self.array_class_cache
            .borrow_mut()
            .insert(component_class_id, id);
        id
    }

    fn array_class_obj_id_uncached(&self, component_class_id: ClassId) -> u64 {
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

        // gci i14/hd: the one element encoder (see `write_obj_array_dump`).
        for i in 0..length {
            encode_array_element(&self.vm.mem.heap, obj, elem_type, i, &mut seg.buf);
        }
    }
}

/// Element `i` of the array `obj` (element type `et`) in HPROF's big-endian
/// encoding, appended to `out`: a reference as its object id (0 for null),
/// a primitive by its value's bits; an element the accessor refuses reads as
/// 0 / null. The one encoder of both array sub-records and of the streamed
/// segment's reference arrays (gci i14/hd; the bytes are the pre-i14 loops').
fn encode_array_element(
    heap: &crate::memory::VmHeap,
    obj: ObjectRef,
    et: ArrayElementType,
    i: usize,
    out: &mut Vec<u8>,
) {
    if matches!(et, ArrayElementType::Reference) {
        let val = heap
            .get_array_element(obj, i)
            .unwrap_or(cratonvm_types::Value::Object(None));
        let id = match val {
            cratonvm_types::Value::Object(Some(r)) => r.as_ptr() as u64,
            _ => 0,
        };
        out.extend_from_slice(&id.to_be_bytes());
        return;
    }
    let val = heap
        .get_array_element(obj, i)
        .unwrap_or(cratonvm_types::Value::Int(0));
    match et {
        ArrayElementType::Boolean | ArrayElementType::Byte => {
            out.push(val.as_int().unwrap_or(0) as u8);
        }
        ArrayElementType::Char | ArrayElementType::Short => {
            out.extend_from_slice(&(val.as_int().unwrap_or(0) as u16).to_be_bytes());
        }
        ArrayElementType::Int => {
            out.extend_from_slice(&(val.as_int().unwrap_or(0) as u32).to_be_bytes());
        }
        ArrayElementType::Float => {
            out.extend_from_slice(&val.as_float().unwrap_or(0.0).to_bits().to_be_bytes());
        }
        ArrayElementType::Long => {
            out.extend_from_slice(&(val.as_long().unwrap_or(0) as u64).to_be_bytes());
        }
        ArrayElementType::Double => {
            out.extend_from_slice(&val.as_double().unwrap_or(0.0).to_bits().to_be_bytes());
        }
        ArrayElementType::Reference => {}
    }
}

/// gci i14/hd: the streamed segment writes its payload in pieces of about
/// this many bytes (one `Vec`, reused), whatever the array's size.
const STREAM_CHUNK_BYTES: usize = 1 << 20;

/// `(sub-record header bytes, element bytes)` of an array's HPROF
/// sub-record: tag, object id, stack-trace serial, element count, then the
/// element type (primitive, 18 bytes) or the array class id (reference, 25
/// bytes, HotSpot's `1 + 2 * 4 + 2 * sizeof(address)`). Pure.
fn array_subrecord_shape(et: ArrayElementType) -> (usize, usize) {
    if matches!(et, ArrayElementType::Reference) {
        (1 + 8 + 4 + 4 + 8, 8)
    } else {
        (1 + 8 + 4 + 4 + 1, hprof_type_size(array_element_to_hprof(et)))
    }
}

/// The whole sub-record's byte length for `length` elements. Pure.
fn array_subrecord_len(et: ArrayElementType, length: usize) -> usize {
    let (head, elem) = array_subrecord_shape(et);
    head.saturating_add(length.saturating_mul(elem))
}

/// HotSpot's `DumperSupport::calculate_array_max_length`: the elements of an
/// array that fit one record whose `u4` length also covers `head` bytes of
/// sub-record header (`max_juint - header_size`); `length` when all do. Pure.
fn hprof_array_max_length(length: usize, elem: usize, head: usize) -> usize {
    let max_bytes = (u32::MAX as usize).saturating_sub(head);
    if elem == 0 || length.saturating_mul(elem) <= max_bytes {
        length
    } else {
        max_bytes / elem
    }
}

/// HotSpot's `type2name_tab` entry for an array's element type, as its
/// truncation warning names it. Pure.
fn hotspot_array_type_name(et: ArrayElementType) -> &'static str {
    match et {
        ArrayElementType::Boolean => "boolean",
        ArrayElementType::Char => "char",
        ArrayElementType::Float => "float",
        ArrayElementType::Double => "double",
        ArrayElementType::Byte => "byte",
        ArrayElementType::Short => "short",
        ArrayElementType::Int => "int",
        ArrayElementType::Long => "long",
        ArrayElementType::Reference => "object",
    }
}

/// gci i14/hd: `count` primitive elements of `elem` bytes each, read in
/// place from `src` in the host's byte order (every backend lays a primitive
/// array out packed: `VmHeap::array_data_ptr`), appended big-endian as HPROF
/// stores them -- the bytes [`encode_array_element`] writes for each.
///
/// # Safety
/// `src .. src + count * elem` is readable and not written meanwhile.
unsafe fn push_prim_elements_be(src: *const u8, elem: usize, count: usize, out: &mut Vec<u8>) {
    // SAFETY: the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(src, count * elem) };
    if elem <= 1 || cfg!(target_endian = "big") {
        out.extend_from_slice(bytes);
        return;
    }
    for e in bytes.chunks_exact(elem) {
        out.extend(e.iter().rev());
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

/// gci i14/hd (item 10): a CLASS_DUMP's class loader object id -- the
/// `classLoader` field (`slot`) of the class's mirror, 0 for the boot loader,
/// for a class with no mirror yet, or when `java.lang.Class` has no such
/// field (a synthetic JDK).
fn class_loader_obj_id(
    heap: &crate::memory::VmHeap,
    mirror: Option<ObjectRef>,
    slot: Option<usize>,
) -> u64 {
    let (Some(mirror), Some(slot)) = (mirror, slot) else {
        return 0;
    };
    match heap.get_field(mirror, slot) {
        cratonvm_types::Value::Object(Some(loader)) => loader.as_ptr() as u64,
        _ => 0,
    }
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
///      16-byte header ([`cratonvm_types::HEADER_SIZE`]), and its body is
///      either the LEGACY one, where every instance field — `boolean`
///      included — occupies a 16-byte slot ([`cratonvm_types::SLOT_SIZE`]),
///      or, for a class with a registered compact layout, the COMPACT one the
///      allocator packs by declared field. See [`class_instance_size`].
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
    class_instance_size(
        store.layout_domain(),
        class_id,
        count_instance_fields(class_id, store),
    )
}

/// The heap footprint of one instance of `class_id` with `fields` instance
/// fields, header included, as the allocator sizes it: `HEADER_SIZE + body`,
/// where the body is the class's registered COMPACT body when
/// [`cratonvm_types::compact_object_body_size`] answers for this
/// `(domain, class, field count)` — the same predicate every collector's
/// allocation path plans with — and `fields * SLOT_SIZE` otherwise.
///
/// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`): this
/// was the legacy formula unconditionally, so with compact reference fields
/// on (the default, on every backend) a class of eight `boolean`s was dumped
/// at `16 + 8 * 16 = 144` bytes while the heap had charged a few. Shared by
/// both CLASS_DUMP writers (this dumper and `vm_init.rs`'s `heap_dump`). A
/// compact body is never larger than the legacy one; the `min` keeps a
/// hypothetical larger answer from ever inflating a report.
pub(crate) fn class_instance_size(domain: u32, class_id: ClassId, fields: usize) -> usize {
    let legacy = cratonvm_types::HEADER_SIZE
        .saturating_add(fields.saturating_mul(cratonvm_types::SLOT_SIZE));
    match cratonvm_types::compact_object_total_size(domain, class_id.as_u32(), fields) {
        Some(total) => total.min(legacy),
        None => legacy,
    }
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
    /// gci i4/hd (review item 4): `class_id`'s instance-field HPROF types,
    /// one group per class of its chain (`class_hierarchy_chain`, root first),
    /// statics skipped -- exactly what the instance loop read per object
    /// before. Cached per class for the dump.
    fn instance_layout(&self, class_id: ClassId) -> std::rc::Rc<Vec<Vec<u8>>> {
        if let Some(layout) = self.layout_cache.borrow().get(&class_id) {
            return std::rc::Rc::clone(layout);
        }
        let layout = {
            let cm = self.vm.classes.class_manager.read();
            let chain = class_hierarchy_chain(class_id, &cm.class_store);
            let mut groups: Vec<Vec<u8>> = Vec::with_capacity(chain.len());
            for cid in &chain {
                let mut types = Vec::new();
                if let Some(cls) = cm.class_store.get(*cid) {
                    for f in &cls.fields {
                        if f.is_static() {
                            continue;
                        }
                        types.push(descriptor_to_hprof_type(&f.descriptor));
                    }
                }
                groups.push(types);
            }
            std::rc::Rc::new(groups)
        };
        self.layout_cache
            .borrow_mut()
            .insert(class_id, std::rc::Rc::clone(&layout));
        layout
    }

    fn write_instance_dump_with_values(&self, seg: &mut SegmentBuilder, obj: ObjectRef) {
        let header = self.vm.mem.heap.get_header(obj);
        let class_id = header.class_id;
        let num_fields = header.num_slots() as usize;

        // gci i4/hd (review item 4): the layout once per class, not the
        // class-manager lock and the chain per object.
        let layout = self.instance_layout(class_id);

        // Read root-first (heap slot order), buffering one group per class.
        let mut per_class: Vec<Vec<u8>> = Vec::with_capacity(layout.len());
        let mut slot_idx = 0usize;

        for types in layout.iter() {
            let mut group = Vec::new();
            for &htype in types {
                let val = if slot_idx < num_fields {
                    self.vm.mem.heap.get_field(obj, slot_idx)
                } else {
                    cratonvm_types::Value::Int(0)
                };
                write_value_to_vec(&mut group, htype, &val);
                slot_idx += 1;
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

    /// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`): a
    /// class with a registered compact layout is dumped at `HEADER_SIZE +
    /// compact body`; anything the layout does not describe keeps the legacy
    /// `HEADER_SIZE + fields * SLOT_SIZE`, byte for byte.
    #[test]
    fn class_instance_size_reports_the_compact_body() {
        use cratonvm_types::{FieldStorageKind, HEADER_SIZE, SLOT_SIZE};
        // A fresh domain and a class id far above any test VM's class count,
        // so the process-global layout registry has no other owner for it.
        const CID: u32 = 200_003;
        let domain = cratonvm_types::next_layout_domain();
        cratonvm_types::register_class_layout(
            domain,
            CID,
            Arc::new(cratonvm_types::CompactLayout {
                field_disps: (8..16).collect(),
                is_ref: vec![false; 8],
                field_kinds: vec![FieldStorageKind::Boolean; 8],
                ref_disps: Vec::new(),
                total_size: 16,
            }),
        );

        let legacy = HEADER_SIZE + 8 * SLOT_SIZE;
        let expected = if cratonvm_types::compact_ref_fields_enabled() {
            cratonvm_types::COMPACT_HEADER_SIZE + 8
        } else {
            legacy
        };
        assert_eq!(
            class_instance_size(domain, ClassId::new(CID), 8),
            expected,
            "eight booleans: a packed 8-byte body, not 8 * 16"
        );
        assert_eq!(
            class_instance_size(domain, ClassId::new(CID), 3),
            HEADER_SIZE + 3 * SLOT_SIZE,
            "a field count the layout does not describe is legacy"
        );
        assert_eq!(
            class_instance_size(domain, ClassId::new(CID + 1), 2),
            HEADER_SIZE + 2 * SLOT_SIZE,
            "a class with no layout is legacy"
        );
        cratonvm_types::unregister_class_layout(CID);
    }

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
    /// footprint (for a LEGACY class, a `HEADER_SIZE` header + one 16-byte cell
    /// per instance field; a compact class is covered by
    /// `class_instance_size_reports_the_compact_body`), not HotSpot's packed
    /// field-byte sum. Guards the fix for MAT reporting an
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
        // gci i3/hd: `dump_heap` refuses an existing file, as HotSpot does.
        let _ = std::fs::remove_file(&tmp);
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

    /// gc-common w2-c: the skip test is half-open `[start, end)`, like the
    /// published TLAB tails it is fed from.
    #[test]
    fn w2c_skip_span_is_half_open() {
        let vm = make_test_vm();
        let mut dumper = HprofDumper::new(&vm);
        assert!(!dumper.in_skip_span(0x1000), "no spans, nothing skipped");
        dumper.skip_spans = vec![(0x1000, 0x1040), (0x2000, 0x2008)];
        assert!(dumper.in_skip_span(0x1000));
        assert!(dumper.in_skip_span(0x1038));
        assert!(!dumper.in_skip_span(0x1040), "end is exclusive");
        assert!(!dumper.in_skip_span(0x0ff8));
        assert!(dumper.in_skip_span(0x2000));
        assert!(!dumper.in_skip_span(0x2008));
    }

    /// gc-common w2-c: an object the heap walk yields inside a reserved TLAB
    /// tail (a frozen peer's un-retired tail, at the dump's pause) is not
    /// emitted as an instance.
    #[test]
    fn w2c_walk_drops_objects_inside_a_reserved_tlab_tail() {
        let vm = make_test_vm();
        let obj = vm.mem.heap.alloc_object(ClassId::new(0), 1);
        let addr = obj.as_ptr() as usize;
        let mut needle = vec![GC_INSTANCE_DUMP];
        needle.extend_from_slice(&(addr as u64).to_be_bytes());
        let count = |out: &[u8]| {
            out.windows(needle.len())
                .filter(|w| *w == needle.as_slice())
                .count()
        };

        let mut skipped = Vec::new();
        let mut dumper = HprofDumper::new(&vm);
        dumper.skip_spans = vec![(addr, addr + 8)];
        dumper.write_all(&mut skipped).unwrap();
        assert_eq!(
            count(&skipped),
            0,
            "an object inside a reserved tail must not be dumped"
        );

        // The same dump without the span names it, whenever this backend's
        // walk reports a freshly allocated object at all.
        let mut plain = Vec::new();
        HprofDumper::new(&vm).write_all(&mut plain).unwrap();
        let listed = vm
            .mem
            .heap
            .walk_objects()
            .iter()
            .any(|(p, _)| *p as usize == addr);
        if listed {
            assert!(count(&plain) >= 1, "the unskipped dump must name it");
        }
        let _keep = obj;
    }

    /// gci i2/jn (`gci-i1jn-heap-dump-omits-in-native-band-roots-FIXED-20261012`):
    /// the dump reads the JNI in-native bands only inside its own pause and
    /// only with the band at the pause on; a dumper made with `new` (no pause)
    /// reads none, whatever the switch, and still writes a whole dump.
    #[test]
    fn gci_i2jn_dump_reads_in_native_bands_only_inside_its_pause() {
        assert!(in_native_band_roots_readable(true, true));
        assert!(!in_native_band_roots_readable(false, true), "no pause: a band can end under the read");
        assert!(!in_native_band_roots_readable(true, false), "band at the pause off: no band recorded");
        assert!(!in_native_band_roots_readable(false, false));

        let vm = make_test_vm();
        let dumper = HprofDumper::new(&vm);
        assert!(!dumper.in_pause, "a bare dumper holds no pause");
        assert!(dumper.in_native_band_roots().is_empty());
        let mut out = Vec::new();
        HprofDumper::new(&vm).write_all(&mut out).unwrap();
        assert!(!out.is_empty());
    }

    /// gci i3/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
    /// frozen peers' frames are read only inside the dump's own pause and only
    /// when it froze someone; a bare dumper (no pause, no frozen set) reads
    /// none even when handed tids.
    #[test]
    fn gci_i3hd_frozen_peer_roots_only_inside_the_pause() {
        assert!(frozen_peer_roots_readable(true, 1));
        assert!(!frozen_peer_roots_readable(true, 0), "nobody frozen");
        assert!(!frozen_peer_roots_readable(false, 3), "no pause: nobody is held");
        let vm = make_test_vm();
        let mut dumper = HprofDumper::new(&vm);
        assert!(dumper.frozen_peer_tids.is_empty());
        dumper.frozen_peer_tids = vec![u32::MAX];
        let (frames, fields) = dumper.frozen_peer_roots();
        assert!(frames.is_empty() && fields.is_empty());
    }

    /// gci i4/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`,
    /// the compiled half): the words the dump's take-over kept are Java-frame
    /// roots only inside the dump's own pause with someone frozen, never one
    /// inside a reserved TLAB tail; and the dump writes them.
    #[test]
    fn gci_i4hd_frozen_peer_words_are_java_frame_roots_inside_the_pause() {
        let vm = make_test_vm();
        let a = vm.mem.heap.alloc_object(ClassId::new(0), 1);
        let b = vm.mem.heap.alloc_object(ClassId::new(0), 1);
        let (aa, ba) = (a.as_ptr() as usize, b.as_ptr() as usize);
        let mut dumper = HprofDumper::new(&vm);
        assert!(dumper.frozen_peer_words.is_empty());
        dumper.frozen_peer_words = vec![a, b];
        dumper.frozen_peer_tids = vec![u32::MAX];
        assert!(dumper.frozen_peer_word_roots().is_empty(), "no pause: nobody is held");
        dumper.in_pause = true;
        let got: Vec<usize> = dumper
            .frozen_peer_word_roots()
            .iter()
            .map(|o| o.as_ptr() as usize)
            .collect();
        assert_eq!(got, vec![aa, ba]);
        dumper.frozen_peer_tids.clear();
        assert!(dumper.frozen_peer_word_roots().is_empty(), "nobody frozen");
        dumper.frozen_peer_tids = vec![u32::MAX];
        dumper.skip_spans = vec![(ba, ba + 8)];
        let got: Vec<usize> = dumper
            .frozen_peer_word_roots()
            .iter()
            .map(|o| o.as_ptr() as usize)
            .collect();
        assert_eq!(got, vec![aa], "a word inside a reserved tail is no object");
        dumper.skip_spans.clear();
        let mut out = Vec::new();
        dumper.write_all(&mut out).unwrap();
        let mut needle = vec![GC_ROOT_JAVA_FRAME];
        needle.extend_from_slice(&(aa as u64).to_be_bytes());
        assert!(
            out.windows(needle.len()).any(|w| w == needle.as_slice()),
            "the dump names the frozen word as a Java-frame root"
        );
        let _keep = (a, b);
    }

    /// gci i4/hd (review item 6): every boot-loader class is a sticky root;
    /// the name rules stay.
    #[test]
    fn gci_i4hd_sticky_class_roots_are_the_boot_loaders() {
        use cratonvm_types::ClassLoaderId;
        assert!(sticky_class_root("jdk/internal/misc/Unsafe", ClassLoaderId::Bootstrap));
        assert!(sticky_class_root("sun/nio/cs/UTF_8", ClassLoaderId::Bootstrap));
        assert!(sticky_class_root("java/lang/Object", ClassLoaderId::Application));
        assert!(sticky_class_root("[I", ClassLoaderId::Application));
        assert!(!sticky_class_root("com/example/App", ClassLoaderId::Application));
        assert!(!sticky_class_root("jdk/Foo", ClassLoaderId::UserDefined(7)));
    }

    /// gci i4/hd (review item 4): the per-class layout is built once and
    /// reused; the array class id once per component.
    #[test]
    fn gci_i4hd_dump_caches_layouts_and_array_class_ids() {
        let vm = make_test_vm();
        let object = vm
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name("java/lang/Object")
            .expect("loaded");
        let dumper = HprofDumper::new(&vm);
        let first = dumper.instance_layout(object);
        let again = dumper.instance_layout(object);
        assert!(std::rc::Rc::ptr_eq(&first, &again), "built once per class");
        assert_eq!(first.len(), 1, "Object's chain is itself");
        let a = dumper.array_class_obj_id(object);
        assert_eq!(dumper.array_class_obj_id(object), a);
        assert_eq!(dumper.array_class_cache.borrow().len(), 1);
    }

    /// gci i4/hd: both dump pauses keep the frozen peers' compiled words
    /// (source pin; a frozen OS thread cannot be staged in a unit test).
    #[test]
    fn gci_i4hd_dump_pauses_keep_frozen_peer_words() {
        let src = include_str!("hprof.rs").replace("\r\n", "\n");
        let ds = &src[src.find("\nimpl<'a> DumpSafepoint<'a> {").expect("DumpSafepoint")..];
        let ds = &ds[..ds.find("\n}\n").expect("end of impl")];
        assert_eq!(ds.matches("NonMovingPause::request_dump_words(").count(), 2);
        assert!(!ds.contains("NonMovingPause::request("));
    }

    /// gci i3/hd: HotSpot's `KlassInfoEntry::compare` -- more bytes first; on
    /// a tie arrays before instances, then by name.
    #[test]
    fn gci_i3hd_histogram_order_is_hotspots() {
        use std::cmp::Ordering;
        assert_eq!(
            hotspot_histogram_order((100, false, "b"), (50, true, "a")),
            Ordering::Less
        );
        assert_eq!(
            hotspot_histogram_order((50, false, "a"), (50, true, "[B")),
            Ordering::Greater,
            "an array class first on a tie"
        );
        assert_eq!(
            hotspot_histogram_order((50, false, "java.lang.String"), (50, false, "java.lang.Object")),
            Ordering::Greater
        );
        assert_eq!(
            hotspot_histogram_order((50, true, "[C"), (50, true, "[B")),
            Ordering::Greater
        );
        assert_eq!(hotspot_histogram_order((1, false, "x"), (1, false, "x")), Ordering::Equal);
    }

    /// gci i3/hd: a histogram row's external name -- a primitive array by its
    /// element letter, a reference array by its array class (the header holds
    /// the COMPONENT id), an instance by its own class.
    #[test]
    fn gci_i3hd_histogram_names() {
        let vm = make_test_vm();
        let cm = vm.classes.class_manager.read();
        let object = cm
            .find_unique_class_by_name("java/lang/Object")
            .expect("the test VM loads java/lang/Object");
        let base = Some("java.base@25");
        let (n, m) = histo_name_and_module(&cm, HistoKey::PrimArray(ArrayElementType::Byte as u8), base);
        assert_eq!((n.as_str(), m.as_deref()), ("[B", Some("java.base@25")));
        let (n, _) = histo_name_and_module(&cm, HistoKey::PrimArray(ArrayElementType::Long as u8), base);
        assert_eq!(n, "[J");
        let (n, _) = histo_name_and_module(&cm, HistoKey::Instance(object.as_u32()), base);
        assert_eq!(n, "java.lang.Object");
        let (n, _) = histo_name_and_module(&cm, HistoKey::RefArray(object.as_u32()), base);
        assert_eq!(n, "[Ljava.lang.Object;");
        let (n, m) = histo_name_and_module(&cm, HistoKey::Instance(u32::MAX - 1), base);
        assert_eq!((n.as_str(), m), ("class#4294967294", None));
        for (et, letter) in [(4u8, 'Z'), (5, 'C'), (6, 'F'), (7, 'D'), (8, 'B'), (9, 'S'), (10, 'I'), (11, 'J')] {
            assert_eq!(prim_array_letter(et), letter);
        }
    }

    /// gci i3/hd (`gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-FIXED-20261012`):
    /// the attach histogram runs in a pause (granted at once with no mutator)
    /// and keys a primitive array apart from instances.
    #[test]
    fn gci_i3hd_class_histogram_for_dcmd_walks_in_a_pause() {
        let vm = make_test_vm();
        let object = vm
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name("java/lang/Object")
            .expect("loaded");
        let a = vm.mem.heap.alloc_object(object, 0);
        let b = vm.mem.heap.alloc_object(object, 0);
        let arr = vm.mem.heap.alloc_array(object, ArrayElementType::Byte, 16);
        let listed = |o: ObjectRef| {
            vm.mem
                .heap
                .walk_objects()
                .iter()
                .any(|(p, _)| *p as usize == o.as_ptr() as usize)
        };
        let rows = class_histogram_for_dcmd(&vm).expect("a pause with no mutator is granted");
        if listed(a) && listed(b) {
            let row = rows
                .iter()
                .find(|e| e.class_name.starts_with("java.lang.Object"))
                .expect("an Object row");
            assert!(row.instance_count >= 2);
        }
        if listed(arr) {
            assert!(rows.iter().any(|e| e.class_name.starts_with("[B")));
        }
        for w in rows.windows(2) {
            assert!(w[0].total_bytes >= w[1].total_bytes, "largest first");
        }
        // The pause was released: a second one is granted too.
        assert!(class_histogram_for_dcmd(&vm).is_ok());
        let _keep = (a, b, arr);
    }

    /// gci i3/hd: the attach dump writes this file's HPROF inside a pause, and
    /// an uncreatable path fails before any pause is requested.
    #[test]
    fn gci_i3hd_dump_heap_for_dcmd_writes_and_refuses_a_bad_path() {
        let vm = make_test_vm();
        let tmp = std::env::temp_dir().join("cratonvm_gci_i3hd_dcmd.hprof");
        let path = tmp.to_str().unwrap();
        let size = dump_heap_for_dcmd(&vm, path, false, true).expect("dump");
        let data = std::fs::read(&tmp).unwrap();
        assert_eq!(data.len() as u64, size);
        assert!(data.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
        let _ = std::fs::remove_file(&tmp);
        let err = dump_heap_for_dcmd(&vm, "/nonexistent/dir/for/gci/i3hd/x.hprof", false, true)
            .unwrap_err();
        assert!(err.contains("Failed to write heap dump to"), "{err}");
    }

    /// The payload of a stored-block gzip stream, checked as a reader checks
    /// it (header, block lengths, CRC-32, ISIZE). Panics on a malformed one.
    fn gunzip_stored(data: &[u8]) -> Vec<u8> {
        assert_eq!(&data[..4], &[0x1f, 0x8b, 0x08, 0x00], "gzip header, deflate, no flags");
        let mut at = 10;
        let mut out = Vec::new();
        loop {
            let head = data[at];
            assert_eq!(head & 0b110, 0, "BTYPE 00 (stored)");
            let len = u16::from_le_bytes([data[at + 1], data[at + 2]]);
            let nlen = u16::from_le_bytes([data[at + 3], data[at + 4]]);
            assert_eq!(nlen, !len);
            at += 5;
            out.extend_from_slice(&data[at..at + usize::from(len)]);
            at += usize::from(len);
            if head & 1 == 1 {
                break;
            }
        }
        let crc = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
        let isize = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap());
        assert_eq!(at + 8, data.len(), "nothing after the trailer");
        assert_eq!(crc, !gzip_crc_update(0xFFFF_FFFF, &out));
        assert_eq!(isize as usize, out.len());
        out
    }

    /// gci i5/hd (review item 10): the CRC is IEEE CRC-32 (the RFC 1952 check
    /// value), and the stored-block writer frames any length -- empty, one
    /// byte, exactly one block, a block and a byte -- as a stream that reads
    /// back byte for byte.
    #[test]
    fn gci_i5hd_gzip_stored_writer_frames_every_length() {
        assert_eq!(!gzip_crc_update(0xFFFF_FFFF, b"123456789"), 0xCBF4_3926);
        assert_eq!(!gzip_crc_update(0xFFFF_FFFF, b""), 0);
        let block = GZIP_STORED_BLOCK;
        for n in [0usize, 1, block - 1, block, block + 1, 200_000] {
            // Truncation intended: a byte pattern.
            let input: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
            let mut z = GzipStoredWriter::new(Vec::new()).unwrap();
            // Odd chunk sizes, so blocks fill across `write` calls.
            for chunk in input.chunks(7_777) {
                z.write_all(chunk).unwrap();
            }
            let gz = z.finish().unwrap();
            assert_eq!(gunzip_stored(&gz), input, "n={n}");
        }
    }

    /// gci i5/hd (review item 10, i4 open item 6): `-gz` writes a gzip file
    /// holding the HPROF dump, its size is the file's; without `overwrite`
    /// an existing file is refused and left alone.
    #[test]
    fn gci_i5hd_dcmd_dump_gzips_and_opens_exclusively() {
        let vm = make_test_vm();
        let tmp = std::env::temp_dir().join("cratonvm_gci_i5hd_dcmd.hprof.gz");
        let path = tmp.to_str().unwrap();
        let _ = std::fs::remove_file(&tmp);
        let size = dump_heap_for_dcmd(&vm, path, true, false).expect("dump");
        let data = std::fs::read(&tmp).unwrap();
        assert_eq!(data.len() as u64, size);
        let hprof = gunzip_stored(&data);
        assert!(hprof.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
        let err = dump_heap_for_dcmd(&vm, path, false, false).unwrap_err();
        assert!(err.contains("Failed to write heap dump to"), "{err}");
        assert_eq!(std::fs::read(&tmp).unwrap(), data, "the existing file is untouched");
        let again = dump_heap_for_dcmd(&vm, path, false, true).expect("overwrite");
        assert!(std::fs::read(&tmp).unwrap().starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
        assert!(again > 0);
        let _ = std::fs::remove_file(&tmp);
    }

    /// gci i3/hd: each THREAD_OBJ root names its own thread's serial, and a
    /// dead thread's mirror is no root (the serials used to be paired with a
    /// second, filtered list by index).
    #[test]
    fn gci_i3hd_thread_roots_name_their_own_serial() {
        let vm = make_test_vm();
        let reg = &vm.threads.thread_registry;
        let dead = vm.mem.heap.alloc_object(ClassId::new(0), 1);
        let live = vm.mem.heap.alloc_object(ClassId::new(0), 1);
        reg.register(ThreadId(9_101), "gci-i3hd-dead", Some(dead));
        reg.mark_dead(ThreadId(9_101));
        reg.register(ThreadId(9_102), "gci-i3hd-live", Some(live));
        let names = reg.all_thread_names();
        let serial = names
            .iter()
            .position(|(t, _)| *t == ThreadId(9_102))
            .expect("registered") as u32
            + 1;
        let mut out = Vec::new();
        HprofDumper::new(&vm).write_all(&mut out).unwrap();
        let root = |obj: ObjectRef| {
            let mut n = vec![GC_ROOT_THREAD_OBJ];
            n.extend_from_slice(&(obj.as_ptr() as u64).to_be_bytes());
            n
        };
        let mut want = root(live);
        want.extend_from_slice(&serial.to_be_bytes());
        want.extend_from_slice(&serial.to_be_bytes());
        assert!(out.windows(want.len()).any(|w| w == want.as_slice()), "live thread at its serial");
        let gone = root(dead);
        assert!(
            !out.windows(gone.len()).any(|w| w == gone.as_slice()),
            "a dead thread's mirror is not a thread root"
        );
    }

    /// gci i5/hd (review item 5): a Java-frame root from a thread's own
    /// snapshot names that thread's serial, not serial 1.
    #[test]
    fn gci_i5hd_snapshot_frame_roots_name_their_threads_serial() {
        let vm = make_test_vm();
        let reg = &vm.threads.thread_registry;
        // Three holders: at most one of them can sit at serial 1, so the
        // others tell the fix from the old constant.
        let mut held = Vec::new();
        for k in 0..3u64 {
            let tid = ThreadId(9_201 + k);
            let obj = vm.mem.heap.alloc_object(ClassId::new(0), 1);
            reg.register(tid, "gci-i5hd-holder", None);
            reg.set_root_snapshot(tid, Arc::new(parking_lot::Mutex::new(vec![obj])));
            held.push((tid, obj));
        }
        let names = reg.all_thread_names();
        let mut out = Vec::new();
        HprofDumper::new(&vm).write_all(&mut out).unwrap();
        for &(tid, obj) in &held {
            let serial = names
                .iter()
                .position(|(t, _)| *t == tid)
                .expect("registered") as u32
                + 1;
            let mut want = vec![GC_ROOT_JAVA_FRAME];
            want.extend_from_slice(&(obj.as_ptr() as u64).to_be_bytes());
            want.extend_from_slice(&serial.to_be_bytes());
            want.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
            assert!(
                out.windows(want.len()).any(|w| w == want.as_slice()),
                "the held object is a Java-frame root of its own thread's serial {serial}"
            );
        }
        let _keep = held;
    }

    /// gci i3/hd: the OOME dump (`dump_heap`) refuses an existing file and
    /// leaves it untouched, answering HotSpot's reason.
    #[test]
    fn gci_i3hd_oome_dump_refuses_an_existing_file() {
        let vm = make_test_vm();
        let tmp = std::env::temp_dir().join("cratonvm_gci_i3hd_oome_exists.hprof");
        std::fs::write(&tmp, b"keep").unwrap();
        let err = dump_heap(&vm, tmp.to_str().unwrap(), ThreadId(0)).unwrap_err();
        assert_eq!(err, "File exists");
        assert_eq!(std::fs::read(&tmp).unwrap(), b"keep");
        let _ = std::fs::remove_file(&tmp);
        let nf = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(open_error_reason(&nf), "No such file or directory");
    }

    /// gci i47w/dh47: `HotSpotDiagnosticMXBean.dumpHeap`'s dump writes an
    /// HPROF file and collects first only when `live`; an existing file is
    /// HotSpot's bare `File exists`, left untouched, and nothing is collected
    /// (HotSpot's `DumpWriter` opens before `VM_HeapDumper` runs).
    #[test]
    fn gci_i47w_dh47_mxbean_dump_opens_exclusively_before_collecting() {
        let vm = make_test_vm();
        let tmp = std::env::temp_dir().join("cratonvm_gci_i47w_dh47_mxbean.hprof");
        let path = tmp.to_str().unwrap();
        let _ = std::fs::remove_file(&tmp);
        for live in [true, false] {
            let _ = std::fs::remove_file(&tmp);
            let mut collected = 0;
            let size = dump_heap_for_mxbean(&vm, path, live, &mut || collected += 1)
                .expect("dump");
            assert_eq!(collected, usize::from(live), "live={live}");
            let data = std::fs::read(&tmp).unwrap();
            assert!(size > 0);
            assert_eq!(data.len() as u64, size);
            assert!(data.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
        }
        let before = std::fs::read(&tmp).unwrap();
        let mut collected = 0;
        let err = dump_heap_for_mxbean(&vm, path, true, &mut || collected += 1).unwrap_err();
        assert_eq!(err, "File exists");
        assert_eq!(collected, 0, "no collection for a refused file");
        assert_eq!(std::fs::read(&tmp).unwrap(), before, "the existing file is untouched");
        let _ = std::fs::remove_file(&tmp);
    }

    /// gci i47w/dh47: source witness -- the MXBean path opens with
    /// `create_new` before its collection and its pause, and answers the bare
    /// reason (`open_error_reason`), not the `jcmd` text.
    #[test]
    fn gci_i47w_dh47_mxbean_dump_source_order() {
        let src = include_str!("hprof.rs");
        let start = src
            .find("pub(crate) fn dump_heap_for_mxbean(")
            .expect("dump_heap_for_mxbean");
        let body = &src[start..start + src[start..].find("\n}\n").expect("end")];
        let open = body.find(".create_new(true)").expect("exclusive open");
        let reason = body.find("open_error_reason(&e)").expect("bare reason");
        let collect = body.find("collect();").expect("collect");
        let pause = body.find("DumpSafepoint::request_outside(").expect("pause");
        assert!(open < reason && reason < collect && collect < pause);
        assert!(!body.contains("Failed to write heap dump"));
    }

    /// gci i3/hd: source witness -- both attach walks take the pause before
    /// they walk, and neither falls back to an unpaused walk.
    #[test]
    fn gci_i3hd_attach_walks_hold_the_pause() {
        let src = include_str!("hprof.rs");
        // gci i14/hd second pass: the bodies moved to the `_live` variants.
        for f in ["pub(crate) fn dump_heap_for_dcmd_live(", "pub(crate) fn class_histogram_for_dcmd_live("] {
            let start = src.find(f).expect(f);
            let body = &src[start..];
            let body = &body[..body.find("\n}\n").expect("end")];
            let pause = body.find("DumpSafepoint::request_outside(vm, ").expect("pause");
            let refuse = body.find("dcmd_no_pause_message()").expect("refusal");
            // gci i14/hd: the histogram visits (`for_each_object`).
            let walk = body
                .find("write_dump_in(")
                .or_else(|| body.find(".walk_objects()"))
                .or_else(|| body.find(".for_each_object("))
                .or_else(|| body.find("walk_heap_for_dcmd("))
                .expect("walk");
            assert!(pause < refuse && refuse < walk, "{f}");
        }
    }

    /// gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
    /// item 3): the OOME dump's pause request waits GC-safe between refusals,
    /// bounded, and only a caller with a slice retries. Live half: on a test VM
    /// a granted request calls no slice; a refused one called at least one
    /// before its unpaused fallback.
    #[test]
    fn gci_i9hj_oom_dump_retries_its_pause_gc_safe() {
        let vm = make_test_vm();
        let mut slices = 0u32;
        let mut slice = || slices += 1;
        let granted = DumpSafepoint::request_waiting(&vm, ThreadId(0), Some(&mut slice as &mut dyn FnMut()))
            .pause
            .is_some();
        if granted {
            assert_eq!(slices, 0, "a granted pause waits no slice");
        } else {
            assert!(slices >= 1, "a refused pause waits GC-safe before the fallback");
        }
        // Source half.
        // gci i10/al: the escapes were literal line breaks (a text-mode
        // rewrite turned `\r\n` into one); same meaning on an LF file, and
        // CRLF-safe again.
        let src = include_str!("hprof.rs").replace("\r\n", "\n");
        let rw = &src[src.find("    fn request_waiting(").expect("request_waiting")..];
        let rw = &rw[..rw.find("\n    }\n").expect("end")];
        let req = rw.find("request_dump_words(vm, initiator)").expect("request");
        let none = rw.find("gc_safe_slice.as_deref_mut() else").expect("no slice: one request");
        let bound = rw.find(">= deadline").expect("bounded");
        let wait = rw.find("slice();").expect("the GC-safe wait");
        assert!(req < none && none < bound && bound < wait);
        assert!(rw.contains("std::time::Instant::now() + OOM_DUMP_PAUSE_RETRY"));
        let dh = &src[src.find("pub fn dump_heap(").expect("dump_heap")..];
        let dh = &dh[..dh.find("\n}\n").expect("end")];
        assert!(dh.contains("dump_heap_waiting(vm, path, initiator, None, false)"));
        let oom = &src[src.find("pub(crate) fn dump_heap_on_oom(").expect("dump_heap_on_oom")..];
        let oom = &oom[..oom.find("\n}\n").expect("end")];
        assert!(oom.contains("dump_heap_waiting(vm, path, initiator, Some(gc_safe_slice), gzip)"));
    }

    // ---- gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`) ----

    /// A G1 test VM (64 MiB) with `java/lang/Object` loaded.
    fn make_g1_test_vm() -> Arc<SharedVm> {
        let config = crate::config::VmConfig {
            use_synthetic_jdk: true,
            gc_algorithm: crate::config::GcAlgorithm::G1,
            max_heap_size: 64 * 1024 * 1024,
            ..Default::default()
        };
        let vm = Arc::new(crate::vm::SharedVm::new(config));
        let _ = vm.load_class_concurrent("java/lang/Object");
        vm
    }

    fn object_class(vm: &SharedVm) -> ClassId {
        vm.classes
            .class_manager
            .read()
            .find_unique_class_by_name("java/lang/Object")
            .expect("the test VM loads java/lang/Object")
    }

    /// Item 3: the binary search answers exactly as the per-object linear
    /// scan it replaced, half-open, on unsorted input once sorted.
    #[test]
    fn gci_i14hd_sorted_spans_answer_as_the_linear_scan() {
        let spans = sorted_spans(vec![(0x3000, 0x3010), (0x1000, 0x1040), (0x2000, 0x2008)]);
        assert_eq!(spans, vec![(0x1000, 0x1040), (0x2000, 0x2008), (0x3000, 0x3010)]);
        let linear = |a: usize| spans.iter().any(|&(s, e)| a >= s && a < e);
        for a in (0x0ff0..0x3020).step_by(4) {
            assert_eq!(in_sorted_spans(&spans, a), linear(a), "{a:#x}");
        }
        assert!(!in_sorted_spans(&[], 0x1000));
    }

    /// Item 5: HotSpot's `calculate_array_max_length` and the sub-record
    /// shapes the streamed segment is sized from.
    #[test]
    fn gci_i14hd_array_max_length_is_hotspots() {
        let max = u32::MAX as usize;
        assert_eq!(hprof_array_max_length(10, 8, 25), 10);
        assert_eq!(hprof_array_max_length(max, 8, 25), (max - 25) / 8);
        assert_eq!(hprof_array_max_length(max - 18, 1, 18), max - 18, "exactly fits");
        assert_eq!(hprof_array_max_length(max - 17, 1, 18), max - 18, "one byte over");
        assert_eq!(array_subrecord_shape(ArrayElementType::Reference), (25, 8));
        assert_eq!(array_subrecord_shape(ArrayElementType::Char), (18, 2));
        assert_eq!(array_subrecord_shape(ArrayElementType::Boolean), (18, 1));
        assert_eq!(array_subrecord_len(ArrayElementType::Long, 3), 18 + 24);
        assert_eq!(hotspot_array_type_name(ArrayElementType::Reference), "object");
        assert_eq!(hotspot_array_type_name(ArrayElementType::Int), "int");
    }

    /// Item 5: the in-place payload read writes the bytes the per-element
    /// encoder writes: big-endian, bytes as they are.
    #[test]
    fn gci_i14hd_prim_elements_are_big_endian() {
        let ints: [i32; 3] = [1, -2, 0x0102_0304];
        let mut out = Vec::new();
        // SAFETY: 3 elements of 4 bytes in a live local array.
        unsafe { push_prim_elements_be(ints.as_ptr() as *const u8, 4, 3, &mut out) };
        let mut want = Vec::new();
        for v in ints {
            want.extend_from_slice(&(v as u32).to_be_bytes());
        }
        assert_eq!(out, want);
        let bytes = [7u8, 8, 9];
        out.clear();
        // SAFETY: as above, 3 one-byte elements.
        unsafe { push_prim_elements_be(bytes.as_ptr(), 1, 3, &mut out) };
        assert_eq!(out, vec![7, 8, 9]);
    }

    /// Item 1: a TLAB filler is neither an object nor a program `int[]`.
    #[test]
    fn gci_i14hd_tlab_filler_is_recognised() {
        let filler = cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID;
        assert!(is_tlab_filler(filler, ObjectKind::Array));
        assert!(!is_tlab_filler(filler, ObjectKind::Object));
        assert!(!is_tlab_filler(ClassId::new(0), ObjectKind::Array));
    }

    /// Item 4: G1's visitor yields exactly `walk_objects`' list, a humongous
    /// array included.
    #[test]
    fn gci_i14hd_g1_visitor_yields_the_walk() {
        let vm = make_g1_test_vm();
        let object = object_class(&vm);
        let mut keep = Vec::new();
        for _ in 0..64 {
            keep.push(vm.mem.heap.alloc_object(object, 0));
        }
        // 1.2 MB of int: humongous at the 64 MiB heap's region size.
        keep.push(vm.mem.heap.alloc_array(object, ArrayElementType::Int, 300_000));
        let listed = vm.mem.heap.walk_objects();
        let mut visited = Vec::new();
        vm.mem
            .heap
            .for_each_object(&mut |p: *mut u8, s: usize| visited.push((p, s)));
        assert!(!visited.is_empty());
        assert_eq!(visited, listed);
        drop(keep);
    }

    /// Items 1 and 2: the dump writes no record for a TLAB filler or an
    /// object whose class the store does not hold (it would name a class
    /// object with no CLASS_DUMP), and the histogram prints no `class#<id>`
    /// row; an ordinary object is still dumped. On the default backend and
    /// on G1.
    #[test]
    fn gci_i14hd_dump_skips_fillers_and_classless_objects() {
        for vm in [make_test_vm(), make_g1_test_vm()] {
            let object = object_class(&vm);
            let live = vm.mem.heap.alloc_object(object, 0);
            let filler = vm.mem.heap.alloc_array(
                cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID,
                ArrayElementType::Int,
                6,
            );
            let classless = vm.mem.heap.alloc_object(ClassId::new(0x00AB_CDEF), 0);
            let listed: Vec<usize> = vm
                .mem
                .heap
                .walk_objects()
                .iter()
                .map(|(p, _)| *p as usize)
                .collect();
            let mut out = Vec::new();
            HprofDumper::new(&vm).write_all(&mut out).unwrap();
            let count = |tag: u8, o: ObjectRef| {
                let mut needle = vec![tag];
                needle.extend_from_slice(&(o.as_ptr() as u64).to_be_bytes());
                out.windows(needle.len())
                    .filter(|w| *w == needle.as_slice())
                    .count()
            };
            assert_eq!(count(GC_PRIM_ARRAY_DUMP, filler), 0, "a TLAB filler is no int[]");
            assert_eq!(count(GC_INSTANCE_DUMP, classless), 0, "no record names a missing class");
            if listed.contains(&(live.as_ptr() as usize)) {
                assert!(count(GC_INSTANCE_DUMP, live) >= 1, "an ordinary object is dumped");
            }
            let rows = class_histogram_for_dcmd(&vm).expect("a pause with no mutator is granted");
            assert!(
                rows.iter().all(|e| !e.class_name.starts_with("class#")),
                "no row for a class the store does not hold"
            );
            let _keep = (live, filler, classless);
        }
    }

    /// Item 5: an array larger than a segment is written as its own segment
    /// whose sub-record is byte for byte the in-memory one (primitive: read
    /// in place; reference: the shared encoder), on a G1 heap where both are
    /// humongous.
    #[test]
    fn gci_i14hd_large_array_streams_the_same_record() {
        let vm = make_g1_test_vm();
        let object = object_class(&vm);
        let target = vm.mem.heap.alloc_object(object, 0);
        let n = 3 * 1024 * 1024; // 12 MiB of int
        let ints = vm.mem.heap.alloc_array(object, ArrayElementType::Int, n);
        for (i, v) in [(0usize, 1i32), (1, -2), (n / 2, 0x0102_0304), (n - 1, 7)] {
            vm.mem
                .heap
                .set_array_element(ints, i, cratonvm_types::Value::Int(v))
                .unwrap();
        }
        let m = 1_100_000; // 8.8 MB of references
        let refs = vm.mem.heap.alloc_array(object, ArrayElementType::Reference, m);
        vm.mem
            .heap
            .set_array_element(refs, 5, cratonvm_types::Value::Object(Some(target)))
            .unwrap();
        assert!(array_subrecord_len(ArrayElementType::Int, n) > MAX_SEGMENT_SIZE);
        assert!(array_subrecord_len(ArrayElementType::Reference, m) > MAX_SEGMENT_SIZE);
        let dumper = HprofDumper::new(&vm);
        for (arr, reference) in [(ints, false), (refs, true)] {
            let mut seg = SegmentBuilder::new();
            if reference {
                dumper.write_obj_array_dump(&mut seg, arr);
            } else {
                dumper.write_prim_array_dump(&mut seg, arr);
            }
            let mut streamed = Vec::new();
            dumper.write_array_segment_streamed(&mut streamed, arr).unwrap();
            assert_eq!(streamed[0], HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
            let body = u32::from_be_bytes([streamed[5], streamed[6], streamed[7], streamed[8]]) as usize;
            assert_eq!(body, seg.len(), "reference={reference}");
            assert!(streamed[9..] == seg.buf[..], "reference={reference}: same sub-record bytes");
        }
        let _keep = (target, ints, refs);
    }

    /// Second pass, item 7: the opt-in is OFF by default; with it on and no
    /// trusted completed mark (a fresh G1 heap), the live walk visits what
    /// `for_each_object` visits; the skipping visitor alone visits nothing.
    #[test]
    fn gci_i14hd_live_walk_falls_back_without_a_proof() {
        assert!(!live_walk_skips_proven_dead(), "default OFF");
        let vm = make_g1_test_vm();
        let mut none_visited = 0usize;
        assert!(vm
            .mem
            .heap
            .for_each_object_skipping_proven_dead(&mut |_: *mut u8, _: usize| none_visited += 1)
            .is_none());
        assert_eq!(none_visited, 0);
        let listed = vm.mem.heap.walk_objects().len();
        let mut n = 0usize;
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_G1_LIVE_WALK_SKIPS_PROVEN_DEAD", Some("1"))],
            || {
                assert!(live_walk_skips_proven_dead());
                walk_heap_for_dcmd(&vm.mem.heap, true, &mut |_: *mut u8, _: usize| n += 1);
            },
        );
        assert_eq!(n, listed);
    }

    /// Second pass, item 10: the CLASS_DUMP loader id is the mirror's
    /// `classLoader` referent, 0 without a mirror, a slot or a loader.
    #[test]
    fn gci_i14hd_class_dump_names_the_loader_object() {
        let vm = make_g1_test_vm();
        let heap = &vm.mem.heap;
        let holder = heap.alloc_object(ClassId::new(0x00AB_CDE0), 1);
        let loader = heap.alloc_object(ClassId::new(0x00AB_CDE1), 0);
        assert_eq!(class_loader_obj_id(heap, None, Some(0)), 0);
        assert_eq!(class_loader_obj_id(heap, Some(holder), None), 0);
        assert_eq!(class_loader_obj_id(heap, Some(holder), Some(0)), 0, "a null loader is the boot loader");
        heap.set_field(holder, 0, cratonvm_types::Value::Object(Some(loader)));
        assert_eq!(class_loader_obj_id(heap, Some(holder), Some(0)), loader.as_ptr() as u64);
        let _keep = (holder, loader);
    }

    /// Second pass, item 4 on Generational: the visitor (young listed, old in
    /// 64 Ki slices) yields exactly `walk_objects`' list, across a slice
    /// boundary.
    #[test]
    fn gci_i14hd_generational_visitor_yields_the_walk() {
        let vm = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig {
            use_synthetic_jdk: true,
            gc_algorithm: crate::config::GcAlgorithm::Generational,
            max_heap_size: 64 * 1024 * 1024,
            ..Default::default()
        }));
        let _ = vm.load_class_concurrent("java/lang/Object");
        let object = object_class(&vm);
        let mut keep = Vec::new();
        for _ in 0..16 {
            keep.push(vm.mem.heap.alloc_object(object, 0));
        }
        for _ in 0..70_000 {
            match vm.mem.heap.try_alloc_object_old(object, 0) {
                Some(o) => keep.push(o),
                None => break,
            }
        }
        let listed = vm.mem.heap.walk_objects();
        let mut visited = Vec::new();
        vm.mem
            .heap
            .for_each_object(&mut |p: *mut u8, s: usize| visited.push((p, s)));
        assert_eq!(visited.len(), listed.len());
        assert!(visited == listed, "same objects, same order");
        drop(keep);
    }

    /// gci i18/dc (`gci-i3hd-heap-walks-and-diagnostics-review-findings` item
    /// 5): the serial map is the thread loops' `i + 1` over the name list.
    #[test]
    fn gci_i18dc_thread_serials_follow_the_name_list() {
        let names = vec![
            (ThreadId(40), "a".to_string()),
            (ThreadId(7), "b".to_string()),
            (ThreadId(12), "c".to_string()),
        ];
        let serials = thread_serials(&names);
        assert_eq!(serials.len(), 3);
        assert_eq!(serials[&ThreadId(40)], 1);
        assert_eq!(serials[&ThreadId(7)], 2);
        assert_eq!(serials[&ThreadId(12)], 3);
        assert!(!serials.contains_key(&ThreadId(99)));
    }

    /// gci i18/dc (review item 5's remainder): a frozen peer's roots carry
    /// that peer's thread id, and the dump names its serial on the
    /// thread-block root (it named serial 1).
    #[test]
    fn gci_i18dc_frozen_peer_roots_name_their_threads_serial() {
        let vm = make_test_vm();
        let reg = &vm.threads.thread_registry;
        if reg.current_os_tid_claimed() != Some(false) {
            return; // no OS-tid backend here, or this OS thread is claimed already
        }
        for k in 0..3u64 {
            reg.register(ThreadId(9_300 + k), "gci-i18dc-other", None);
        }
        let tid = ThreadId(9_310);
        reg.register(tid, "gci-i18dc-peer", None);
        reg.set_os_tid_current(tid);
        let Some(os_tid) = reg
            .thread_dump_entries()
            .iter()
            .find(|e| e.thread_id == tid)
            .and_then(|e| e.os_tid)
        else {
            return;
        };
        let held = vm.mem.heap.alloc_object(ClassId::new(0), 1);
        let mut thread = crate::threading::jvm_thread::JvmThread::new(tid, "gci-i18dc-peer");
        thread.jit_pending_exception = Some(held);
        reg.set_gc_block_state(tid, thread.gc_block_state.clone());
        reg.set_jvm_thread_addr(tid, &thread as *const _ as usize);
        let serial = thread_serials(&reg.all_thread_names())[&tid];

        let mut dumper = HprofDumper::new(&vm);
        dumper.frozen_peer_tids = vec![os_tid];
        let (frames, fields) = dumper.frozen_peer_roots();
        assert!(frames.is_empty() && fields.is_empty(), "no pause: nobody is held");
        dumper.in_pause = true;
        let (frames, fields) = dumper.frozen_peer_roots();
        assert!(frames.is_empty(), "the peer has no interpreter frame");
        assert!(fields.contains(&(tid, held)), "the field root carries its owner");
        assert!(fields.iter().all(|(owner, _)| *owner == tid));

        let mut out = Vec::new();
        dumper.write_all(&mut out).unwrap();
        let mut want = vec![GC_ROOT_THREAD_BLOCK];
        want.extend_from_slice(&(held.as_ptr() as u64).to_be_bytes());
        want.extend_from_slice(&serial.to_be_bytes());
        assert!(
            out.windows(want.len()).any(|w| w == want.as_slice()),
            "the thread-block root names the peer's serial {serial}"
        );
        reg.set_jvm_thread_addr(tid, 0);
        drop(thread);
    }

    /// gci i18/dc: the in-native band roots are read per owning thread
    /// (`scan_in_native_bands_by_owner`) and written at that thread's serial.
    #[test]
    fn gci_i18dc_in_native_band_roots_are_read_per_owner() {
        let src = include_str!("hprof.rs").replace("\r\n", "\n");
        // One method's text: from its signature to the first line that closes
        // an impl-level item.
        let method = |sig: &str| -> String {
            let start = src.find(sig).expect(sig);
            let rest = &src[start..];
            rest[..rest.find("\n    \u{7d}\n").expect("its end")].to_string()
        };
        let body = method("\n    fn in_native_band_roots(&self)");
        assert!(body.contains(".scan_in_native_bands_by_owner(&probe, &mut each)"));
        assert!(!body.contains(".scan_in_native_bands(&probe"));
        assert!(body.contains("companions.borrow_mut().drain(..)"));
        let roots = method("\n    fn write_gc_roots(&self");
        assert!(roots.contains("for &(tid, obj) in &self.in_native_band_roots()"));
        assert!(roots.contains("for &(tid, obj) in &frame_roots"));
        assert!(roots.contains("for &(tid, obj) in &field_roots"));
        assert!(roots.contains("seg.push_u32(serial_of(tid));"));
    }
}
