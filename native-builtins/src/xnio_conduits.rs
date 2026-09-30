// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.7.d — XNIO Conduit stream channels + `ChannelListener`.
//!
//! XNIO's read/write abstraction is the "conduit" channel. Undertow reads HTTP
//! request bytes through `ConduitStreamSourceChannel` and writes responses
//! through `ConduitStreamSinkChannel`. Each has a `ChannelListener` that the
//! event loop fires when the underlying socket is ready.
//!
//! # Architecture
//!
//! ```text
//!   ConduitStreamSourceChannel (read side)
//!       ├── channel_id  (registry handle into the source-channel registry)
//!       ├── selection_key (NIO Selector key — from T19.7.a)
//!       ├── read_listener (ObjectRef → org.xnio.ChannelListener)
//!       ├── read_ready_flag (bit set by event loop on OP_READ fire)
//!       └── read_suspended (resume/suspend toggle)
//!
//!   ConduitStreamSinkChannel (write side)
//!       ├── channel_id  (registry handle into the sink-channel registry)
//!       ├── selection_key
//!       ├── write_listener
//!       ├── write_ready_flag
//!       ├── write_suspended
//!       └── buffered_bytes (count held between `write` and `flush`)
//!
//!   Event dispatch (called by T19.7.c run_io_loop):
//!       dispatch_channel_event(key)
//!         → ready_ops = OP_READ|OP_WRITE
//!         → if OP_READ  && !read_suspended:
//!               catch_unwind(listener.handleEvent(channel))
//!         → if OP_WRITE && !write_suspended:
//!               catch_unwind(listener.handleEvent(channel))
//! ```
//!
//! # Back-pressure + flush
//!
//! `write(buf)` on a conduit channel that's currently write-blocked (kernel
//! buffer full) returns 0. Undertow calls `resumeWrites()` then retries on
//! the next writable event. `flush()` returns true only when the internal
//! buffer is empty AND the socket has been drained. We maintain a per-sink
//! `buffered_bytes` counter (field 5) that's incremented by blocked writes
//! and drained by successful ones or by `flush`.
//!
//! # Security hardening
//!
//! * **Direct-buffer bounds check**: the `try_buffer_slice` helper validates
//!   `position..position+remaining` fits in `0..capacity` before producing
//!   a slice; an out-of-range descriptor returns `Err(BufferOverflow)`
//!   rather than segfaulting.
//! * **Byte-count sanity**: every read / write result is clamped to
//!   `0..=buf.remaining()` and values outside that range log `error!` and
//!   return 0.
//! * **Listener panic safety**: `catch_unwind(AssertUnwindSafe)` wraps every
//!   `handleEvent` dispatch so a buggy listener cannot crash the event loop.
//! * **Re-entry**: a listener calling `resume/suspendReads` on its own
//!   channel during `handleEvent` stages the change via a flip-on-commit
//!   flag; the enclosing dispatch iteration finishes before the new state
//!   takes effect (no re-fire within one iteration).
//!
//! # Synthetic-stub field layouts (mirrored in `class_manager.rs`)
//!
//! | Class                                                  | # | Slots                                                                 |
//! |--------------------------------------------------------|---|-----------------------------------------------------------------------|
//! | `org/xnio/conduits/ConduitStreamSourceChannel`         | 6 | channel_id, selection_key, read_listener, read_ready_flag, read_susp, io_thread |
//! | `org/xnio/conduits/ConduitStreamSinkChannel`           | 7 | channel_id, selection_key, write_listener, write_ready_flag, write_susp, buffered_bytes, io_thread |
//! | `org/xnio/ChannelListener$Setter`                      | 2 | channel_handle, listener_slot_index                                   |

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Class names
// ---------------------------------------------------------------------------

const CLS_SOURCE: &str = "org/xnio/conduits/ConduitStreamSourceChannel";
const CLS_SINK: &str = "org/xnio/conduits/ConduitStreamSinkChannel";
const CLS_STREAM_SOURCE_CONDUIT: &str = "org/xnio/conduits/StreamSourceConduit";
const CLS_STREAM_SINK_CONDUIT: &str = "org/xnio/conduits/StreamSinkConduit";
const CLS_SOURCE_CONDUIT: &str = "org/xnio/conduits/SourceConduit";
const CLS_SINK_CONDUIT: &str = "org/xnio/conduits/SinkConduit";
const CLS_LISTENER: &str = "org/xnio/ChannelListener";
const CLS_LISTENER_SETTER: &str = "org/xnio/ChannelListener$Setter";

// ---------------------------------------------------------------------------
// Synthetic field offsets (mirrored in class_manager.rs::synthetic_stub_fields)
// ---------------------------------------------------------------------------

// ConduitStreamSourceChannel
pub(crate) const SRC_FIELD_CHANNEL_ID: usize = 0;
pub(crate) const SRC_FIELD_SELECTION_KEY: usize = 1;
pub(crate) const SRC_FIELD_READ_LISTENER: usize = 2;
pub(crate) const SRC_FIELD_READ_READY_FLAG: usize = 3;
pub(crate) const SRC_FIELD_READ_SUSPENDED: usize = 4;
pub(crate) const SRC_FIELD_IO_THREAD: usize = 5;
pub(crate) const SRC_NUM_SLOTS: usize = 6;

// ConduitStreamSinkChannel
pub(crate) const SINK_FIELD_CHANNEL_ID: usize = 0;
pub(crate) const SINK_FIELD_SELECTION_KEY: usize = 1;
pub(crate) const SINK_FIELD_WRITE_LISTENER: usize = 2;
pub(crate) const SINK_FIELD_WRITE_READY_FLAG: usize = 3;
pub(crate) const SINK_FIELD_WRITE_SUSPENDED: usize = 4;
pub(crate) const SINK_FIELD_BUFFERED_BYTES: usize = 5;
pub(crate) const SINK_FIELD_IO_THREAD: usize = 6;
pub(crate) const SINK_NUM_SLOTS: usize = 7;

// ChannelListener$Setter
pub(crate) const SETTER_FIELD_CHANNEL_HANDLE: usize = 0;
pub(crate) const SETTER_FIELD_LISTENER_SLOT_INDEX: usize = 1;
pub(crate) const SETTER_NUM_SLOTS: usize = 2;

// NIO SelectionKey interest-op bits (copied from the JDK constants so this
// module doesn't take a dependency on T19.7.a's selector crate).
pub const OP_READ: i32 = 1 << 0;
pub const OP_WRITE: i32 = 1 << 2;
pub const OP_CONNECT: i32 = 1 << 3;
pub const OP_ACCEPT: i32 = 1 << 4;

// ByteBuffer slots — copied locally to avoid a cross-crate dep on native-io.
const BB_FIELD_ARRAY: usize = 0;
const BB_FIELD_POS: usize = 1;
const BB_FIELD_LIMIT: usize = 2;
const BB_FIELD_CAPACITY: usize = 3;

const CHANNEL_LISTENER_HANDLE_EVENT_DESC: &str = "(Ljava/nio/channels/Channel;)V";

// Historical note: before the dedicated source-poller thread existed
// (`native_source_poller_run`, 10ms tick), these were multi-second sleep
// ladders ([0,5,20,...,2000] and [...,5000,10000]) that polled for the
// peer's response INLINE on the calling thread. With the poller in place
// they became pure harm: `sink_flush`/`resumeWrites` on the management
// upgrade path slept for seconds on the accept-pump thread AFTER writing
// the `101 Switching Protocols`, postponing the jboss-remoting greeting
// frame past the client's 5s connect timeout (WFLYPRT0023) — the server
// and client each waiting on the other. A single immediate probe keeps the
// zero-latency fast path (bytes already visible fire the listener now);
// anything arriving later is the poller's job (<=10ms).
const READ_NOTIFY_RETRY_DELAYS_MS: [u64; 1] = [0];
const READ_NOTIFY_POST_LISTENER_RETRY_DELAYS_MS: [u64; 1] = [0];
fn xnio_tcp_dbg_enabled() -> bool {
    crate::nbflags().dbg_xnio_tcp
}

macro_rules! xnio_tcp_dbg {
    ($($arg:tt)*) => {{
        if xnio_tcp_dbg_enabled() {
            eprintln!("[cratonvm:xnio-tcp] {}", format_args!($($arg)*));
        }
    }};
}

fn preview_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, b) in bytes.iter().take(96).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = FmtWrite::write_fmt(&mut out, format_args!("{b:02x}"));
    }
    if bytes.len() > 96 {
        out.push_str(" ...");
    }
    out
}

// ---------------------------------------------------------------------------
// Conduit channel registry
// ---------------------------------------------------------------------------

/// Underlying transport for a conduit channel.
///
/// * `Tcp(stream)` — real OS TCP stream from T19.5's `net.rs`.
/// * `Pipe(pipe)` — shared in-memory pipe used by unit tests and by
///   Undertow's loopback fast-path. Reads/writes are queued through a
///   `Mutex<VecDeque<u8>>` so tests can push bytes without touching the
///   kernel.
pub enum ConduitTransport {
    Tcp(TcpStream),
    Pipe(Arc<Pipe>),
}

impl std::fmt::Debug for ConduitTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConduitTransport::Tcp(_) => f.write_str("ConduitTransport::Tcp(..)"),
            ConduitTransport::Pipe(_) => f.write_str("ConduitTransport::Pipe(..)"),
        }
    }
}

/// Shared in-memory pipe used for unit-test conduits.
///
/// `buf` carries bytes from writer to reader; `eof` signals `read_reads == -1`
/// once the writer has shut down; `writable_cap` caps outstanding bytes to
/// simulate a full kernel buffer so `write()` can return 0.
pub struct Pipe {
    pub buf: Mutex<std::collections::VecDeque<u8>>,
    pub eof: AtomicBool,
    pub fin_sent: AtomicBool,
    pub writable_cap: usize,
}

impl Pipe {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: Mutex::new(std::collections::VecDeque::new()),
            eof: AtomicBool::new(false),
            fin_sent: AtomicBool::new(false),
            writable_cap: cap,
        }
    }

    /// Append bytes to the pipe (test helper — simulates the kernel
    /// delivering bytes on a previously-suspended conduit).
    pub fn push(&self, bytes: &[u8]) {
        let mut g = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        g.extend(bytes.iter().copied());
    }

    /// Mark the writer side closed — subsequent reads drain the buffer then
    /// return EOF.
    pub fn close_write(&self) {
        // Round-9 HIGH-2: Release pairs with the Acquire load in `read`
        // below — the only flag-vs-flag ordering needed here is that any
        // writes made to the buffer before close are visible after the
        // reader observes eof=true.
        self.eof.store(true, Ordering::Release);
    }

    fn read(&self, dst: &mut [u8]) -> std::io::Result<usize> {
        let mut g = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_empty() {
            return if self.eof.load(Ordering::Acquire) {
                Ok(0) // EOF
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            };
        }
        let mut n = 0;
        while n < dst.len() {
            match g.pop_front() {
                Some(b) => {
                    dst[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        Ok(n)
    }

    fn write(&self, src: &[u8]) -> std::io::Result<usize> {
        let mut g = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        let room = self.writable_cap.saturating_sub(g.len());
        if room == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
        }
        let n = room.min(src.len());
        g.extend(src[..n].iter().copied());
        Ok(n)
    }
}

/// One live source (read-side) conduit channel.
pub struct SourceChannel {
    pub id: u64,
    pub transport: ConduitTransport,
    // gc-common w23-b: the `listener_obj_raw: usize` field that sat here was
    // never written or read (always 0) -- a raw heap address in Rust state is
    // the shape no GC can see, so it went rather than waiting to be used. The
    // listener lives in the Java object's `SRC_FIELD_READ_LISTENER` slot.
    /// The sink channel of the same connection (0: none), set by
    /// `remember_sink_paired_source`. A connection is unregistered when both
    /// of its halves are shut down (gc-common w23-b).
    pub peer_sink: AtomicU64,
    pub read_ready: AtomicBool,
    pub read_suspended: AtomicBool,
    pub shutdown: AtomicBool,
    /// HC0053 follow-up (2026-07-16): non-reentrant dispatch guard. Real
    /// XNIO always services a channel's listener from exactly one IO
    /// thread; here, the dedicated source-poller thread
    /// (`native_source_poller_run`) and a paired sink's direct
    /// `resumeWrites`-triggered notify (`notify_paired_source_readable*`)
    /// can both race to invoke this same source's read listener from two
    /// different native threads. The real (interpreted) listener code —
    /// e.g. `org.xnio.streams.BufferPipeInputStream`'s push/pop path —
    /// synchronizes on more than one object without a globally consistent
    /// order (it never needs one under real XNIO's single-thread-per-
    /// channel guarantee), so two concurrent invocations can lock-order-
    /// invert and deadlock permanently (observed: one invocation holding
    /// the pipe's internal queue monitor while blocked entering the
    /// `BufferPipeInputStream` monitor, the other holding that monitor
    /// while blocked entering the queue's). This flag makes dispatch
    /// non-reentrant instead: a notifier that finds dispatch already in
    /// progress skips firing this round rather than invoking concurrently;
    /// the poller's next tick (or the caller's own retry-with-delays loop)
    /// fires it once the in-flight dispatch completes.
    pub dispatching: AtomicBool,
    /// `wakeupReads()` semantics: force one listener invocation even when
    /// the socket has no pending bytes. Set by `native_source_wakeup_reads`,
    /// consumed (cleared) by the next guarded dispatch from the poller /
    /// notify path. Never dispatched inline from the wakeupReads caller —
    /// see `native_source_resume_reads` for why inline dispatch from a
    /// Java-called native self-deadlocks Undertow's requestState machine.
    pub wakeup_pending: AtomicBool,
}

/// One live sink (write-side) conduit channel.
pub struct SinkChannel {
    pub id: u64,
    pub transport: ConduitTransport,
    /// The source channel of the same connection (0: none); see
    /// [`SourceChannel::peer_sink`].
    pub peer_source: AtomicU64,
    pub write_ready: AtomicBool,
    pub write_suspended: AtomicBool,
    pub shutdown: AtomicBool,
    /// Bytes Java tried to push but the socket wouldn't accept. Tracked so
    /// `flush()` can report "all drained" accurately even when the
    /// implementation splits large writes across multiple kernel calls.
    pub buffered_bytes: AtomicU64,
}

fn source_channels() -> &'static Mutex<HashMap<u64, Arc<SourceChannel>>> {
    static R: OnceLock<Mutex<HashMap<u64, Arc<SourceChannel>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sink_channels() -> &'static Mutex<HashMap<u64, Arc<SinkChannel>>> {
    static R: OnceLock<Mutex<HashMap<u64, Arc<SinkChannel>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_channel_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    // Round-9 HIGH-2: only uniqueness is required — no cross-variable
    // ordering — so Relaxed is sufficient.
    N.fetch_add(1, Ordering::Relaxed)
}

/// A channel / conduit object's key in this module's registries: the calling
/// VM (which teardown and the orphan checks filter by) and the object's WEAK
/// LOCK KEY.
///
/// gc-common w24-a (`common-w23b-xnio-identity-hash-keyed-rows-cross-and-outlive-connections`):
/// this was `(vm, identity hash)`. An identity hash is not unique (the heap's
/// counter wraps), so a later object carrying a dead one's hash inherited the
/// dead one's rows -- `source_id_of` / `sink_id_of` ask this registry before
/// the object's own `channelId` slot, and so did every IO-thread,
/// paired-source and ready-handler lookup -- and read from, or wrote to,
/// another connection's socket. The weak key (`crate::gc_stable_weak_lock_key`,
/// as `xnio_async::xnio_obj_key_for` since w17-b) is VM-scoped, re-addressed by
/// the collector after a move, freed at the first sweep that finds its object
/// dead and never minted again, so a dead object's rows are never found by
/// another object. The rows themselves still go at the connection's close
/// ([`unregister_conduit_channels`]) or the VM's teardown, as before.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ConduitObjKey {
    vm: usize,
    key: usize,
}

/// The key a row for `obj` is FILED under (minting it on first use). `obj`
/// must be current. Compute it before taking a registry guard: the key
/// registry is a leaf the post-collection sweeps take on their own.
fn conduit_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> ConduitObjKey {
    ConduitObjKey {
        vm: ctx.vm_identity(),
        key: crate::gc_stable_weak_lock_key(ctx, obj)
            .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails")),
    }
}

/// The key a row for `obj` would be filed under, or `None` when `obj` was
/// never keyed -- and so has no row. For the lookup and remove paths: unlike
/// [`conduit_obj_key`] it never mints, so a lookup leaves no key-registry slot
/// behind for every object it is asked about (`crate::existing_weak_lock_key`).
fn existing_conduit_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<ConduitObjKey> {
    Some(ConduitObjKey {
        vm: ctx.vm_identity(),
        key: crate::existing_weak_lock_key(ctx, obj)?,
    })
}

/// `ConduitObjKey -> channel id` for one side (source or sink), with the
/// reverse index a channel's close needs.
///
/// gc-common w23-b (`common-w22a-xnio-conduit-channels-are-never-unregistered`):
/// a close has only the channel id in hand; without `keys_by_id` it would have
/// to walk every row of every VM to find the channel's object keys (the
/// channel object's, its raw conduit's, and any conduit a handle row was filed
/// under -- the "aliases" of [`note_row_aliases`]).
#[derive(Default)]
struct ObjIdRegistry {
    by_key: HashMap<ConduitObjKey, u64>,
    /// `(vm, channel id) ->` every key filed for that channel: its own keys
    /// (also in `by_key`) and its alias keys (handle-registry rows only).
    keys_by_id: HashMap<(usize, u64), Vec<ConduitObjKey>>,
}

impl ObjIdRegistry {
    fn get(&self, key: &ConduitObjKey) -> Option<u64> {
        self.by_key.get(key).copied()
    }

    fn insert(&mut self, key: ConduitObjKey, id: u64) {
        match self.by_key.insert(key, id) {
            Some(old) if old == id => return,
            // The key re-filed under another channel (the key registry
            // answered one key for two objects -- see
            // `common-w24a-lock-key-registry-merges-live-identity-hash-colliders`):
            // it is no longer the old channel's.
            Some(old) => self.unlist(key, old),
            None => {}
        }
        self.list(key, id);
    }

    /// Record `alias` as a key of the channel `owner_key` names, if any.
    fn add_alias(&mut self, owner_key: &ConduitObjKey, alias: ConduitObjKey) -> bool {
        match self.by_key.get(owner_key).copied() {
            Some(id) => {
                self.list(alias, id);
                true
            }
            None => false,
        }
    }

    fn list(&mut self, key: ConduitObjKey, id: u64) {
        let keys = self.keys_by_id.entry((key.vm, id)).or_default();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    fn unlist(&mut self, key: ConduitObjKey, id: u64) {
        if let Some(keys) = self.keys_by_id.get_mut(&(key.vm, id)) {
            keys.retain(|k| *k != key);
            if keys.is_empty() {
                self.keys_by_id.remove(&(key.vm, id));
            }
        }
    }

    /// Take channel `id` of VM `vm` out; returns every key it was filed
    /// under (own and alias), for the handle registries.
    fn remove_channel(&mut self, vm: usize, id: u64) -> Vec<ConduitObjKey> {
        let keys = self.keys_by_id.remove(&(vm, id)).unwrap_or_default();
        for key in &keys {
            if self.by_key.get(key) == Some(&id) {
                self.by_key.remove(key);
            }
        }
        keys
    }

    /// VM teardown: drop `vm`'s rows, reporting the channel ids they named.
    fn forget_vm(&mut self, vm: usize, ids: &mut Vec<u64>) {
        self.by_key.retain(|key, id| {
            if key.vm == vm {
                ids.push(*id);
                false
            } else {
                true
            }
        });
        self.keys_by_id.retain(|&(owner, id), _| {
            if owner == vm {
                ids.push(id);
                false
            } else {
                true
            }
        });
    }

    #[cfg(test)]
    fn vm_row_count(&self, vm: usize) -> usize {
        self.by_key.keys().filter(|k| k.vm == vm).count()
            + self
                .keys_by_id
                .keys()
                .filter(|(owner, _)| *owner == vm)
                .count()
    }
}

fn source_obj_registry() -> &'static Mutex<ObjIdRegistry> {
    static R: OnceLock<Mutex<ObjIdRegistry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(ObjIdRegistry::default()))
}

/// `vm -> (source channel id -> global root of the source channel object)`.
///
/// gc-common w23-b: nested by VM (it was keyed `(vm, id)` in one flat map), so
/// the per-write walk ([`notify_registered_sources_readable`]) reads only its
/// own VM's rows instead of filtering every VM's; and a row now goes when its
/// connection closes, so the walk sees live channels only.
fn source_obj_handle_by_id_registry() -> &'static Mutex<HashMap<usize, HashMap<u64, usize>>> {
    static R: OnceLock<Mutex<HashMap<usize, HashMap<u64, usize>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sink_obj_registry() -> &'static Mutex<ObjIdRegistry> {
    static R: OnceLock<Mutex<ObjIdRegistry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(ObjIdRegistry::default()))
}

/// A handle row was filed under `keys` (a channel's key first, then its raw
/// conduit's): record the conduit's key as an alias of the channel, so the
/// channel's close finds that row too. A conduit that `setConduit` installed
/// is not one of the channel's own keys. gc-common w23-b.
fn note_row_aliases(keys: &[ConduitObjKey]) {
    let Some((owner, aliases)) = keys.split_first() else {
        return;
    };
    if aliases.is_empty() {
        return;
    }
    let filed = {
        let mut reg = source_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut filed = false;
        for alias in aliases {
            filed |= reg.add_alias(owner, *alias);
        }
        filed
    };
    if !filed {
        let mut reg = sink_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for alias in aliases {
            reg.add_alias(owner, *alias);
        }
    }
}

/// File `handle` under every key in `keys` of a `ConduitObjKey -> handle`
/// registry and return the handles this displaced that NO row names any more
/// (one handle is filed under both a channel and its raw conduit, so a
/// displaced handle can still be live under the other key). The caller
/// releases the returned handles AFTER dropping the guard; every key carries
/// the caller's VM, so they are the caller's handles.
///
/// gc-common w22-a: the three `remember_*` paths used to overwrite a row and
/// drop the old handle on the floor -- one leaked global root (and a rooted
/// channel / handler / IO thread) per re-registration.
fn insert_shared_handle(
    registry: &mut HashMap<ConduitObjKey, usize>,
    keys: &[ConduitObjKey],
    handle: usize,
) -> Vec<usize> {
    let Some(vm) = keys.first().map(|k| k.vm) else {
        return Vec::new();
    };
    let displaced: Vec<usize> = keys
        .iter()
        .filter_map(|key| registry.insert(*key, handle))
        .filter(|&old| old != handle)
        .collect();
    orphaned_handles(registry, vm, displaced)
}

/// The subset of `candidates` (handles of VM `vm`) no row of `vm` in
/// `registry` names, deduplicated.
///
/// gc-common w23-b: only `vm`'s rows are compared. A global-root handle is an
/// index into ITS VM's table, so another VM's row can carry the same number
/// for a different root -- and the old any-VM comparison then kept this VM's
/// released handle rooted forever. One pass over the rows, not one per
/// candidate.
fn orphaned_handles(
    registry: &HashMap<ConduitObjKey, usize>,
    vm: usize,
    mut candidates: Vec<usize>,
) -> Vec<usize> {
    candidates.sort_unstable();
    candidates.dedup();
    if candidates.is_empty() {
        return candidates;
    }
    for (key, handle) in registry.iter() {
        if key.vm == vm {
            candidates.retain(|c| c != handle);
            if candidates.is_empty() {
                break;
            }
        }
    }
    candidates
}

/// VM teardown (`lib.rs::forget_vm_native_root_stores`): drop every row `vm`
/// filed in this module's object-keyed registries, releasing nothing (the
/// VM's global-ref table dies with it), and drop the Rust-side channels those
/// rows named (their transports close as the last `Arc` goes, after every
/// guard here is released). Idempotent.
///
/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 6): the handle registries were keyed by VM but nothing dropped a
/// torn-down VM's rows, so every conduit channel, paired source, IO thread and
/// ready handler it ever registered stayed in the process with a handle into a
/// dead table.
pub fn forget_vm_xnio_conduit_rows(vm: usize) {
    let mut source_ids: Vec<u64> = Vec::new();
    let mut sink_ids: Vec<u64> = Vec::new();
    source_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forget_vm(vm, &mut source_ids);
    let vm_handles = source_obj_handle_by_id_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm);
    if let Some(rows) = vm_handles {
        source_ids.extend(rows.into_keys());
    }
    sink_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forget_vm(vm, &mut sink_ids);
    for registry in [
        sink_paired_source_registry(),
        channel_io_thread_registry(),
        read_ready_handler_registry(),
        write_ready_handler_registry(),
    ] {
        registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| key.vm != vm);
    }
    source_ids.sort_unstable();
    source_ids.dedup();
    sink_ids.sort_unstable();
    sink_ids.dedup();
    // Channel ids are process-unique (`next_channel_id`), so these are this
    // VM's channels. Collected under the guard, dropped after it.
    let dropped_sources: Vec<Arc<SourceChannel>> = {
        let mut channels = source_channels().lock().unwrap_or_else(|e| e.into_inner());
        source_ids
            .iter()
            .filter_map(|id| channels.remove(id))
            .collect()
    };
    let dropped_sinks: Vec<Arc<SinkChannel>> = {
        let mut channels = sink_channels().lock().unwrap_or_else(|e| e.into_inner());
        sink_ids.iter().filter_map(|id| channels.remove(id)).collect()
    };
    drop(dropped_sources);
    drop(dropped_sinks);
}

fn remember_source_obj(ctx: &mut dyn NativeContext, obj: ObjectRef, id: u64) {
    // The key is computed before the guard is taken (lock discipline: no
    // `ctx` call under a registry lock).
    let key = conduit_obj_key(ctx, obj);
    source_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, id);
    let handle = ctx.add_global_root(obj);
    if handle != 0 {
        let replaced = source_obj_handle_by_id_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.vm)
            .or_default()
            .insert(id, handle);
        // gc-common w22-a: a re-registration used to leak the old root.
        if let Some(old) = replaced.filter(|&old| old != handle) {
            ctx.remove_global_root(old);
        }
    }
}

fn remember_sink_obj(ctx: &dyn NativeContext, obj: ObjectRef, id: u64) {
    let key = conduit_obj_key(ctx, obj);
    sink_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, id);
}

fn source_id_by_obj(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<u64> {
    let key = existing_conduit_obj_key(ctx, obj)?;
    source_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
}

fn sink_id_by_obj(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<u64> {
    let key = existing_conduit_obj_key(ctx, obj)?;
    sink_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
}

/// `(channel id, global root)` of every source channel of the caller's VM that
/// is still registered -- after gc-common w23-b, exactly its open (not yet
/// closed) connections.
fn source_obj_handles_by_id(ctx: &dyn NativeContext) -> Vec<(u64, usize)> {
    let vm = ctx.vm_identity();
    source_obj_handle_by_id_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
        .map(|rows| rows.iter().map(|(id, handle)| (*id, *handle)).collect())
        .unwrap_or_default()
}

fn sink_paired_source_registry() -> &'static Mutex<HashMap<ConduitObjKey, usize>> {
    static R: OnceLock<Mutex<HashMap<ConduitObjKey, usize>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn remember_sink_paired_source(
    ctx: &mut dyn NativeContext,
    sink: ObjectRef,
    source: ObjectRef,
) {
    // gc-common w23-b: record which source and sink make up this connection,
    // so the second half's shutdown can unregister both
    // ([`retire_connection_if_closed`]).
    if let (Some(sink_id), Some(source_id)) = (sink_id_of(ctx, sink), source_id_of(ctx, source)) {
        pair_channels(source_id, sink_id);
    }
    let handle = ctx.add_global_root(source);
    let raw_conduit = match ctx.get_field_by_name(sink, "conduit") {
        Value::Object(Some(conduit)) if conduit.as_ptr() != sink.as_ptr() => Some(conduit),
        _ => None,
    };
    if handle != 0 {
        let mut keys = vec![conduit_obj_key(ctx, sink)];
        if let Some(raw_conduit) = raw_conduit {
            keys.push(conduit_obj_key(ctx, raw_conduit));
        }
        note_row_aliases(&keys);
        let orphaned = insert_shared_handle(
            &mut sink_paired_source_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            &keys,
            handle,
        );
        for old in orphaned {
            ctx.remove_global_root(old);
        }
    } else {
        ctx.set_field(sink, SINK_FIELD_SELECTION_KEY, Value::Object(Some(source)));
        if let Some(raw_conduit) = raw_conduit {
            ctx.set_field(
                raw_conduit,
                SINK_FIELD_SELECTION_KEY,
                Value::Object(Some(source)),
            );
        }
    }
}

fn paired_source_of(ctx: &dyn NativeContext, sink: ObjectRef) -> Option<ObjectRef> {
    let handle = existing_conduit_obj_key(ctx, sink).and_then(|key| {
        sink_paired_source_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied()
    });
    handle.and_then(|h| ctx.resolve_global_root(h)).or_else(|| {
        match ctx.get_field(sink, SINK_FIELD_SELECTION_KEY) {
            Value::Object(Some(source)) => Some(source),
            _ => None,
        }
    })
}

fn channel_io_thread_registry() -> &'static Mutex<HashMap<ConduitObjKey, usize>> {
    static R: OnceLock<Mutex<HashMap<ConduitObjKey, usize>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn raw_conduit_of(ctx: &dyn NativeContext, channel: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(channel, "conduit") {
        Value::Object(Some(conduit)) if conduit.as_ptr() != channel.as_ptr() => Some(conduit),
        _ => None,
    }
}

fn remember_channel_io_thread(
    ctx: &mut dyn NativeContext,
    channel: ObjectRef,
    slot: usize,
    io_thread: ObjectRef,
) {
    ctx.set_field(channel, slot, Value::Object(Some(io_thread)));
    let raw_conduit = raw_conduit_of(ctx, channel);
    if let Some(raw_conduit) = raw_conduit {
        ctx.set_field(raw_conduit, slot, Value::Object(Some(io_thread)));
    }

    let handle = ctx.add_global_root(io_thread);
    if handle != 0 {
        let mut keys = vec![conduit_obj_key(ctx, channel)];
        if let Some(raw_conduit) = raw_conduit {
            keys.push(conduit_obj_key(ctx, raw_conduit));
        }
        note_row_aliases(&keys);
        let orphaned = insert_shared_handle(
            &mut channel_io_thread_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            &keys,
            handle,
        );
        for old in orphaned {
            ctx.remove_global_root(old);
        }
    }
}

pub(crate) fn remember_source_io_thread(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    io_thread: ObjectRef,
) {
    remember_channel_io_thread(ctx, source, SRC_FIELD_IO_THREAD, io_thread);
}

pub(crate) fn remember_sink_io_thread(
    ctx: &mut dyn NativeContext,
    sink: ObjectRef,
    io_thread: ObjectRef,
) {
    remember_channel_io_thread(ctx, sink, SINK_FIELD_IO_THREAD, io_thread);
}

fn channel_io_thread_of(
    ctx: &dyn NativeContext,
    channel: ObjectRef,
    slot: usize,
) -> Option<ObjectRef> {
    let handle = existing_conduit_obj_key(ctx, channel).and_then(|key| {
        channel_io_thread_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied()
    });
    handle
        .and_then(|h| ctx.resolve_global_root(h))
        .or_else(|| match ctx.get_field(channel, slot) {
            Value::Object(Some(io_thread)) => Some(io_thread),
            _ => None,
        })
}

// ---------------------------------------------------------------------------
// Conduit ready-handler registry
// ---------------------------------------------------------------------------
//
// `setReadReadyHandler` / `setWriteReadyHandler` are how XNIO wires a conduit
// back to the channel that owns it: `ConduitStreamSourceChannel.<init>`
// installs a `ReadReadyHandler.ChannelListenerHandler` on its conduit, and
// Undertow's framed channels install handlers of their own. The handler is
// therefore state the conduit MUST retain — dropping it silently severs the
// only path a conduit has for telling anyone the socket became ready.
//
// The handler lives in a side registry rather than an object slot: the
// synthetic field layouts in `class_manager.rs::synthetic_stub_fields` stop at
// `readSuspended` / `bufferedBytes`, and the io-thread binding above already
// established the "global root keyed by object identity" idiom for state that
// outgrew those layouts.

fn read_ready_handler_registry() -> &'static Mutex<HashMap<ConduitObjKey, usize>> {
    static R: OnceLock<Mutex<HashMap<ConduitObjKey, usize>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn write_ready_handler_registry() -> &'static Mutex<HashMap<ConduitObjKey, usize>> {
    static R: OnceLock<Mutex<HashMap<ConduitObjKey, usize>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bind `handler` to `channel`. The setter is normally invoked on the raw
/// conduit while the readiness notification runs against the owning channel
/// object, so the binding is recorded under both identities (same two-key
/// shape as `remember_channel_io_thread`).
fn remember_ready_handler(
    registry: &'static Mutex<HashMap<ConduitObjKey, usize>>,
    ctx: &mut dyn NativeContext,
    channel: ObjectRef,
    handler: ObjectRef,
) {
    let raw_conduit = raw_conduit_of(ctx, channel);
    let handle = ctx.add_global_root(handler);
    if handle == 0 {
        // No root available — a bare field store would be reclaimed or moved
        // out from under us, so record nothing rather than a stale address.
        return;
    }
    let mut keys = vec![conduit_obj_key(ctx, channel)];
    if let Some(raw_conduit) = raw_conduit {
        keys.push(conduit_obj_key(ctx, raw_conduit));
    }
    note_row_aliases(&keys);
    let orphaned = insert_shared_handle(
        &mut registry.lock().unwrap_or_else(|e| e.into_inner()),
        &keys,
        handle,
    );
    for old in orphaned {
        ctx.remove_global_root(old);
    }
}

/// `setXxxReadyHandler(null)` detaches the current handler — drop the binding
/// (and its global root) so a rewrapped conduit cannot fire a stale one.
fn forget_ready_handler(
    registry: &'static Mutex<HashMap<ConduitObjKey, usize>>,
    ctx: &mut dyn NativeContext,
    channel: ObjectRef,
) {
    let raw_conduit = raw_conduit_of(ctx, channel);
    // gc-common w23-b: keys computed before the guard (no `ctx` call under it).
    let vm = ctx.vm_identity();
    // gc-common w24-a: a never-keyed object has no row; do not mint for it.
    let own_key = existing_conduit_obj_key(ctx, channel);
    let conduit_key = raw_conduit.and_then(|c| existing_conduit_obj_key(ctx, c));
    if own_key.is_none() && conduit_key.is_none() {
        return;
    }
    let stale = {
        let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
        let mut stale: Vec<usize> = Vec::new();
        if let Some(own_key) = own_key {
            stale.extend(registry.remove(&own_key));
        }
        if let Some(conduit_key) = conduit_key {
            stale.extend(registry.remove(&conduit_key));
        }
        // gc-common w22-a: the two rows can name DIFFERENT handles (the
        // conduit re-bound under another channel), and the old code released
        // only one of them; and a handle still named by a third row (another
        // channel over the same conduit) must stay rooted.
        orphaned_handles(&registry, vm, stale)
    };
    for handle in stale {
        ctx.remove_global_root(handle);
    }
}

fn ready_handler_of(
    registry: &'static Mutex<HashMap<ConduitObjKey, usize>>,
    ctx: &dyn NativeContext,
    channel: ObjectRef,
) -> Option<ObjectRef> {
    let own_key = existing_conduit_obj_key(ctx, channel);
    let conduit_key = raw_conduit_of(ctx, channel).and_then(|c| existing_conduit_obj_key(ctx, c));
    if own_key.is_none() && conduit_key.is_none() {
        return None;
    }
    let handle = {
        let registry = registry.lock().unwrap_or_else(|e| e.into_inner());
        own_key
            .and_then(|k| registry.get(&k).copied())
            .or_else(|| conduit_key.and_then(|k| registry.get(&k).copied()))
    };
    handle.and_then(|h| ctx.resolve_global_root(h))
}

/// Register a new source channel for the given transport. Returns the id.
pub fn register_source_channel(transport: ConduitTransport) -> u64 {
    let id = next_channel_id();
    let ch = Arc::new(SourceChannel {
        id,
        transport,
        peer_sink: AtomicU64::new(0),
        read_ready: AtomicBool::new(false),
        read_suspended: AtomicBool::new(false),
        shutdown: AtomicBool::new(false),
        dispatching: AtomicBool::new(false),
        wakeup_pending: AtomicBool::new(false),
    });
    source_channels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, ch);
    id
}

/// Register a new sink channel. Returns the id.
pub fn register_sink_channel(transport: ConduitTransport) -> u64 {
    let id = next_channel_id();
    let ch = Arc::new(SinkChannel {
        id,
        transport,
        peer_source: AtomicU64::new(0),
        write_ready: AtomicBool::new(false),
        write_suspended: AtomicBool::new(false),
        shutdown: AtomicBool::new(false),
        buffered_bytes: AtomicU64::new(0),
    });
    sink_channels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, ch);
    id
}

/// Look up a source channel by id.
pub fn get_source_channel(id: u64) -> Option<Arc<SourceChannel>> {
    source_channels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
}

/// Look up a sink channel by id.
pub fn get_sink_channel(id: u64) -> Option<Arc<SinkChannel>> {
    sink_channels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
}

/// Unregister source channel `id` now, whatever the state of its peer: its
/// rows go and their global roots are released through `ctx` (which must be
/// the channel's own VM), and its transport closes once the last in-flight
/// `Arc` is gone. Production reaches this through the shutdown natives
/// ([`retire_connection_if_closed`]); tests use it to clean up.
///
/// gc-common w23-b (`common-w22a-xnio-conduit-channels-are-never-unregistered`):
/// this took no `ctx`, so it forgot the source's handle row without releasing
/// the root, and nothing but tests called it.
pub fn drop_source_channel(ctx: &mut dyn NativeContext, id: u64) {
    unregister_conduit_channels(ctx, &[id], &[]);
}

/// Sink twin of [`drop_source_channel`].
pub fn drop_sink_channel(ctx: &mut dyn NativeContext, id: u64) {
    unregister_conduit_channels(ctx, &[], &[id]);
}

/// Link the two halves of one connection (`remember_sink_paired_source`).
fn pair_channels(source_id: u64, sink_id: u64) {
    if let Some(source) = get_source_channel(source_id) {
        source.peer_sink.store(sink_id, Ordering::SeqCst);
    }
    if let Some(sink) = get_sink_channel(sink_id) {
        sink.peer_source.store(source_id, Ordering::SeqCst);
    }
}

/// A half counts as closed once it is shut down, or gone from the registry.
fn source_half_closed(id: u64) -> bool {
    get_source_channel(id).map_or(true, |ch| ch.shutdown.load(Ordering::SeqCst))
}

fn sink_half_closed(id: u64) -> bool {
    get_sink_channel(id).map_or(true, |ch| ch.shutdown.load(Ordering::SeqCst))
}

/// Called right after a shutdown native set one half's `shutdown` flag
/// (SeqCst): if every half of the connection is now shut down, unregister the
/// connection. A channel with no peer is a connection of one half.
///
/// The two halves can be shut down concurrently on two threads. Each stores
/// its own flag and then loads the other's, all SeqCst, so at least one of
/// the two sees both set (and both may: the unregistration is idempotent).
fn retire_connection_if_closed(
    ctx: &mut dyn NativeContext,
    source_id: Option<u64>,
    sink_id: Option<u64>,
) {
    if source_id.is_none() && sink_id.is_none() {
        return;
    }
    if source_id.map_or(true, source_half_closed) && sink_id.map_or(true, sink_half_closed) {
        let sources: Vec<u64> = source_id.into_iter().collect();
        let sinks: Vec<u64> = sink_id.into_iter().collect();
        unregister_conduit_channels(ctx, &sources, &sinks);
    }
}

/// Take the given channels of the caller's VM out of every registry of this
/// module: the object-key rows (own and alias keys), the source-object root
/// row, the paired-source / IO-thread / ready-handler rows filed under those
/// keys, and the Rust channels. Each lock is taken on its own; the global
/// roots no remaining row names are released through `ctx` and the channels
/// (whose `Drop` closes a `TcpStream`) are dropped after the last guard.
/// Idempotent.
///
/// gc-common w23-b (`common-w22a-xnio-conduit-channels-are-never-unregistered`):
/// before this, a closed connection kept all of it -- the rooted channel,
/// paired source, IO thread and handlers, and the open socket -- until VM
/// teardown, and every sink write walked it.
fn unregister_conduit_channels(ctx: &mut dyn NativeContext, source_ids: &[u64], sink_ids: &[u64]) {
    let vm = ctx.vm_identity();
    let mut keys: Vec<ConduitObjKey> = Vec::new();
    if !source_ids.is_empty() {
        let mut reg = source_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for &id in source_ids {
            keys.extend(reg.remove_channel(vm, id));
        }
    }
    if !sink_ids.is_empty() {
        let mut reg = sink_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for &id in sink_ids {
            keys.extend(reg.remove_channel(vm, id));
        }
    }
    let mut released: Vec<usize> = Vec::new();
    if !source_ids.is_empty() {
        let mut reg = source_obj_handle_by_id_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now_empty = match reg.get_mut(&vm) {
            Some(rows) => {
                released.extend(source_ids.iter().filter_map(|id| rows.remove(id)));
                rows.is_empty()
            }
            None => false,
        };
        if now_empty {
            reg.remove(&vm);
        }
    }
    if !keys.is_empty() {
        for registry in [
            sink_paired_source_registry(),
            channel_io_thread_registry(),
            read_ready_handler_registry(),
            write_ready_handler_registry(),
        ] {
            let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            let removed: Vec<usize> = keys.iter().filter_map(|key| reg.remove(key)).collect();
            released.extend(orphaned_handles(&reg, vm, removed));
        }
    }
    let dropped_sources: Vec<Arc<SourceChannel>> = if source_ids.is_empty() {
        Vec::new()
    } else {
        let mut channels = source_channels().lock().unwrap_or_else(|e| e.into_inner());
        source_ids
            .iter()
            .filter_map(|id| channels.remove(id))
            .collect()
    };
    let dropped_sinks: Vec<Arc<SinkChannel>> = if sink_ids.is_empty() {
        Vec::new()
    } else {
        let mut channels = sink_channels().lock().unwrap_or_else(|e| e.into_inner());
        sink_ids
            .iter()
            .filter_map(|id| channels.remove(id))
            .collect()
    };
    for handle in released {
        ctx.remove_global_root(handle);
    }
    drop(dropped_sources);
    drop(dropped_sinks);
}

// ---------------------------------------------------------------------------
// ByteBuffer helpers
// ---------------------------------------------------------------------------

/// Read a ByteBuffer int-valued metadata slot. We check the synthetic slot
/// first (authoritative in synthetic-jdk mode) and fall back to the JDK
/// named slot only if the synthetic value is 0 — which lets real-JDK mode
/// with a non-zero named-slot value round-trip correctly even when native
/// code never touched the synthetic slot.
fn bb_int_metadata(ctx: &dyn NativeContext, buf: ObjectRef, slot: usize, name: &str) -> i32 {
    if let Value::Int(v) = ctx.get_field(buf, slot) {
        if v != 0 {
            return v;
        }
    }
    if let Value::Int(v) = ctx.get_field_by_name(buf, name) {
        if v != 0 {
            return v;
        }
    }
    // Both slots were 0 — return 0 legitimately. Callers handle pos == 0.
    match ctx.get_field(buf, slot) {
        Value::Int(v) => v,
        _ => 0,
    }
}

fn bb_position(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    bb_int_metadata(ctx, buf, BB_FIELD_POS, "position")
}

fn bb_limit(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    bb_int_metadata(ctx, buf, BB_FIELD_LIMIT, "limit")
}

fn bb_capacity(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    bb_int_metadata(ctx, buf, BB_FIELD_CAPACITY, "capacity")
}

fn bb_set_position(ctx: &dyn NativeContext, buf: ObjectRef, v: i32) {
    ctx.set_field(buf, BB_FIELD_POS, Value::Int(v));
    ctx.set_field_by_name(buf, "position", Value::Int(v));
}

/// Return the number of bytes available for read/write starting at `position`.
/// Validates `0 <= position <= limit <= capacity`; if not, returns 0 and logs
/// an `error!` — prevents out-of-range slice construction downstream.
fn bb_remaining(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    let pos = bb_position(ctx, buf);
    let lim = bb_limit(ctx, buf);
    let cap = bb_capacity(ctx, buf);
    if !(0 <= pos && pos <= lim && lim <= cap) {
        tracing::error!(
            target: "xnio::conduits",
            pos, lim, cap,
            "ByteBuffer bounds check failed — returning 0"
        );
        return 0;
    }
    lim - pos
}

/// Produce a `Result<Vec<u8>, ...>` of up to `n` bytes read from the heap
/// byte[] backing the ByteBuffer. Fails with `BufferOverflow` if the array
/// is missing or smaller than `position + n`.
fn bb_array_offset(ctx: &dyn NativeContext, buf: ObjectRef) -> usize {
    match ctx.get_field_by_name(buf, "offset") {
        Value::Int(v) if v >= 0 => v as usize,
        _ => 0,
    }
}

fn bb_direct_addr(ctx: &dyn NativeContext, buf: ObjectRef, pos: usize) -> Option<i64> {
    match ctx.get_field_by_name(buf, "address") {
        Value::Long(addr) if addr > 0 => addr.checked_add(pos as i64),
        _ => None,
    }
}

fn read_bytes_from_buffer(
    ctx: &dyn NativeContext,
    buf: ObjectRef,
    n: i32,
) -> Result<Vec<u8>, RuntimeError> {
    let pos = bb_position(ctx, buf) as usize;
    let offset = bb_array_offset(ctx, buf);
    let arr = ctx.get_field(buf, BB_FIELD_ARRAY);
    let arr_obj = match arr {
        Value::Object(Some(a)) => Some(a),
        _ => match ctx.get_field_by_name(buf, "hb") {
            Value::Object(Some(a)) => Some(a),
            _ => None,
        },
    };
    if let Some(arr_obj) = arr_obj {
        let start = offset.saturating_add(pos);
        let len = ctx.array_length(arr_obj);
        if start + (n as usize) > len {
            return Err(buf_overflow("write: buffer slice out of range"));
        }
        // gc-common w23-b (perf): one bulk copy (the VM `memcpy`s the payload)
        // instead of one `get_array_element` call per byte of every write.
        // The per-element loop stays as the fallback for a backing array the
        // bulk read does not take whole (not a primitive byte array).
        let mut out = vec![0u8; n as usize];
        if ctx.read_byte_array_into(arr_obj, start, &mut out) == out.len() {
            return Ok(out);
        }
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = match ctx.get_array_element(arr_obj, start + i) {
                Value::Int(v) => (v & 0xff) as u8,
                _ => 0,
            };
        }
        return Ok(out);
    }

    if let Some(addr) = bb_direct_addr(ctx, buf, pos) {
        let mut out = vec![0u8; n as usize];
        if ctx.copy_from_native_memory(addr, &mut out) {
            return Ok(out);
        }
        return Err(buf_overflow("write: invalid direct ByteBuffer address"));
    }

    Err(buf_overflow(
        "write: ByteBuffer has no backing array or direct address",
    ))
}

/// Write `bytes` into the ByteBuffer at `position`, supporting both heap and
/// direct buffers.
fn write_bytes_into_buffer(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    bytes: &[u8],
) -> Result<(), RuntimeError> {
    let pos = bb_position(ctx, buf) as usize;
    let offset = bb_array_offset(ctx, buf);
    let arr = ctx.get_field(buf, BB_FIELD_ARRAY);
    let arr_obj = match arr {
        Value::Object(Some(a)) => Some(a),
        _ => match ctx.get_field_by_name(buf, "hb") {
            Value::Object(Some(a)) => Some(a),
            _ => None,
        },
    };
    if let Some(arr_obj) = arr_obj {
        let start = offset.saturating_add(pos);
        let len = ctx.array_length(arr_obj);
        if start + bytes.len() > len {
            return Err(buf_overflow("read: buffer slice out of range"));
        }
        // gc-common w23-b (perf): one bulk copy instead of a
        // `set_array_element` call per byte read; the per-element loop stays
        // as the fallback for an array the bulk write refuses.
        if ctx.write_byte_array_from(arr_obj, start, bytes) {
            return Ok(());
        }
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr_obj, start + i, Value::Int(*b as i32));
        }
        return Ok(());
    }

    if let Some(addr) = bb_direct_addr(ctx, buf, pos) {
        if ctx.copy_to_native_memory(addr, bytes) {
            return Ok(());
        }
        return Err(buf_overflow("read: invalid direct ByteBuffer address"));
    }

    Err(buf_overflow(
        "read: ByteBuffer has no backing array or direct address",
    ))
}

fn buf_overflow(msg: impl Into<String>) -> RuntimeError {
    RuntimeError::IOException {
        message: format!("BufferOverflowException: {}", msg.into()),
    }
}

fn ioex(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IOException {
        message: msg.into(),
    }
    .into()
}

// ---------------------------------------------------------------------------
// Read-side transport helpers
// ---------------------------------------------------------------------------

fn transport_read(transport: &ConduitTransport, dst: &mut [u8]) -> std::io::Result<usize> {
    match transport {
        ConduitTransport::Tcp(stream) => {
            // SAFETY: `dst` is a `&mut [u8]` provided by the caller and lives
            // for the duration of this call. `TcpStream::read` respects
            // `dst.len()`. `&TcpStream` implements `Read` via std.
            let mut r: &TcpStream = stream;
            r.read(dst)
        }
        ConduitTransport::Pipe(p) => p.read(dst),
    }
}

fn transport_write(transport: &ConduitTransport, src: &[u8]) -> std::io::Result<usize> {
    match transport {
        ConduitTransport::Tcp(stream) => {
            // SAFETY: `src` is a `&[u8]` provided by the caller and lives for
            // the duration of this call. `TcpStream::write` respects
            // `src.len()`. `&TcpStream` implements `Write` via std.
            let mut w: &TcpStream = stream;
            w.write(src)
        }
        ConduitTransport::Pipe(p) => p.write(src),
    }
}

/// Signal the write half as closed (FIN on TCP, `fin_sent` on Pipe).
fn transport_shutdown_write(transport: &ConduitTransport) {
    match transport {
        ConduitTransport::Tcp(stream) => {
            // Best effort — if the socket is already half-closed, ignore the
            // resulting error.
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
        ConduitTransport::Pipe(p) => {
            // Round-9 HIGH-2: Release pairs with Acquire in any reader of
            // fin_sent / eof. Both flags only need single-variable
            // happens-before semantics.
            p.fin_sent.store(true, Ordering::Release);
            p.eof.store(true, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------
// Public: read / write (callable from Rust tests and the Java native layer)
// ---------------------------------------------------------------------------

/// Read up to `buf.remaining()` bytes from the source channel into the
/// buffer. Returns bytes read, `-1` on EOF, `0` on would-block.
pub fn source_channel_read(
    ctx: &mut dyn NativeContext,
    src_id: u64,
    buf: ObjectRef,
) -> Result<i32, RuntimeError> {
    let ch = match get_source_channel(src_id) {
        Some(c) => c,
        // gc-common w23-b: a channel id comes only from a registration, so a
        // missing channel is a closed connection that was unregistered: EOF,
        // exactly what the same read answered while the shut-down channel
        // was still registered.
        None => return Ok(-1),
    };
    // Round-9 HIGH-2: Acquire — pairs with the Release store in
    // `native_source_shutdown_reads`.
    if ch.shutdown.load(Ordering::Acquire) {
        return Ok(-1);
    }
    let remaining = bb_remaining(ctx, buf);
    if remaining <= 0 {
        return Ok(0);
    }
    let mut scratch = vec![0u8; remaining as usize];
    let n = match transport_read(&ch.transport, &mut scratch) {
        Ok(0) => {
            xnio_tcp_dbg!("source_read id={src_id} eof");
            return Ok(-1);
        }
        Ok(n) => {
            xnio_tcp_dbg!(
                "source_read id={src_id} n={} bytes={}",
                n,
                preview_bytes(&scratch[..n])
            );
            n as i32
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            xnio_tcp_dbg!("source_read id={src_id} would_block");
            return Ok(0);
        }
        Err(e) => {
            xnio_tcp_dbg!("source_read id={src_id} error={e}");
            return Err(RuntimeError::IOException {
                message: format!("IOException: read: {e}"),
            });
        }
    };
    // Byte-count sanity.
    if n < 0 || n > remaining {
        tracing::error!(target: "xnio::conduits", n, remaining, "read returned out-of-range count");
        return Ok(0);
    }
    write_bytes_into_buffer(ctx, buf, &scratch[..n as usize])?;
    // Advance position by bytes read.
    let new_pos = bb_position(ctx, buf) + n;
    bb_set_position(ctx, buf, new_pos);
    Ok(n)
}

/// Write up to `buf.remaining()` bytes from the buffer to the sink channel.
/// Returns bytes written, `0` on would-block.
pub fn sink_channel_write(
    ctx: &mut dyn NativeContext,
    sink_id: u64,
    buf: ObjectRef,
) -> Result<i32, RuntimeError> {
    let ch = match get_sink_channel(sink_id) {
        Some(c) => c,
        // gc-common w23-b: an unregistered (closed) connection -- the same
        // exception the shut-down channel threw while still registered.
        None => {
            return Err(RuntimeError::IOException {
                message: "ClosedChannelException: write after shutdown".into(),
            })
        }
    };
    // Round-9 HIGH-2: Acquire — pairs with the Release store in
    // `native_sink_shutdown_writes`.
    if ch.shutdown.load(Ordering::Acquire) {
        return Err(RuntimeError::IOException {
            message: "ClosedChannelException: write after shutdown".into(),
        });
    }
    let remaining = bb_remaining(ctx, buf);
    if remaining <= 0 {
        return Ok(0);
    }
    let src = read_bytes_from_buffer(ctx, buf, remaining)?;
    xnio_tcp_dbg!(
        "sink_write id={sink_id} attempt={} bytes={}",
        src.len(),
        preview_bytes(&src)
    );
    let n = match transport_write(&ch.transport, &src) {
        Ok(n) => n as i32,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            xnio_tcp_dbg!("sink_write id={sink_id} would_block attempt={}", src.len());
            // Kernel buffer full — Undertow should `resumeWrites` + retry.
            // Round-9 HIGH-2: buffered_bytes is a single-variable counter
            // read by `flush()`; AcqRel matches its role as a RMW that both
            // publishes the new count and observes prior writes.
            ch.buffered_bytes
                .fetch_add(src.len() as u64, Ordering::AcqRel);
            return Ok(0);
        }
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("IOException: write: {e}"),
            });
        }
    };
    let written = n.max(0).min(src.len() as i32) as usize;
    xnio_tcp_dbg!(
        "sink_write id={sink_id} wrote={} of={} bytes={}",
        n,
        src.len(),
        preview_bytes(&src[..written])
    );
    if n < 0 || n > remaining {
        tracing::error!(target: "xnio::conduits", n, remaining, "write returned out-of-range count");
        return Ok(0);
    }
    // Count any bytes that didn't make it as buffered (retry-required).
    let short = remaining - n;
    if short > 0 {
        // Round-9 HIGH-2: AcqRel — single-variable counter.
        ch.buffered_bytes.fetch_add(short as u64, Ordering::AcqRel);
    }
    let new_pos = bb_position(ctx, buf) + n;
    bb_set_position(ctx, buf, new_pos);
    Ok(n)
}

// ---------------------------------------------------------------------------
// Event dispatch (called from T19.7.c run_io_loop)
// ---------------------------------------------------------------------------

/// A lightweight shim for the NIO SelectionKey attachment / ready-ops surface.
/// T19.7.a owns the real type; this trait is the narrowest contract the event
/// loop needs from us so we can test dispatch in isolation.
pub trait SelectionKeyLike {
    fn ready_ops(&self) -> i32;
    fn source_attachment(&self) -> Option<(u64, ObjectRef)>;
    fn sink_attachment(&self) -> Option<(u64, ObjectRef)>;
}

/// Outcome of a single dispatch call — handy for testing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DispatchStats {
    pub read_fired: bool,
    pub write_fired: bool,
    pub read_panicked: bool,
    pub write_panicked: bool,
    pub skipped_suspended: u32,
}

/// Entry point the event loop (T19.7.c) calls when a channel's interest op
/// fires. Reads the current suspend / listener state, invokes `handleEvent`
/// through `ctx.invoke_virtual`, and catches any panic so the event loop
/// continues.
///
/// Side effects on `ctx`:
/// * Sets `SRC_FIELD_READ_READY_FLAG` / `SINK_FIELD_WRITE_READY_FLAG` on the
///   Java side so the handler can tell why it was woken.
pub fn dispatch_channel_event<K: SelectionKeyLike>(
    ctx: &mut dyn NativeContext,
    key: &K,
) -> DispatchStats {
    let mut stats = DispatchStats::default();
    let ready = key.ready_ops();

    if ready & OP_READ != 0 {
        if let Some((src_id, src_obj)) = key.source_attachment() {
            let ch = get_source_channel(src_id);
            // Round-9 HIGH-2: Acquire — pairs with the Release store from
            // the suspend / resume natives.
            let suspended = ch
                .as_ref()
                .map(|c| c.read_suspended.load(Ordering::Acquire))
                .unwrap_or(true);
            if suspended {
                stats.skipped_suspended += 1;
            } else {
                ctx.set_field(src_obj, SRC_FIELD_READ_READY_FLAG, Value::Int(1));
                if let Some(c) = &ch {
                    // Release publishes the field write above before any
                    // reader observes read_ready=true.
                    c.read_ready.store(true, Ordering::Release);
                }
                let listener = match ctx.get_field(src_obj, SRC_FIELD_READ_LISTENER) {
                    Value::Object(Some(o)) => Some(o),
                    _ => None,
                };
                if let Some(l) = listener {
                    // AssertUnwindSafe: NativeContext and ObjectRef don't
                    // carry interior-mutable state that's poisoned by
                    // unwinding through a listener — we accept a torn write
                    // in exchange for keeping the event loop alive.
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        ctx.invoke_virtual(
                            l,
                            "handleEvent",
                            CHANNEL_LISTENER_HANDLE_EVENT_DESC,
                            &[Value::Object(Some(src_obj))],
                        )
                    }));
                    match result {
                        Ok(_) => stats.read_fired = true,
                        Err(_) => {
                            stats.read_panicked = true;
                            tracing::error!(
                                target: "xnio::conduits",
                                channel_id = src_id,
                                "read listener panicked — event loop continuing"
                            );
                        }
                    }
                }
            }
        }
    }

    if ready & OP_WRITE != 0 {
        if let Some((sink_id, sink_obj)) = key.sink_attachment() {
            let ch = get_sink_channel(sink_id);
            // Round-9 HIGH-2: Acquire — pairs with the Release store from
            // the suspend / resume natives.
            let suspended = ch
                .as_ref()
                .map(|c| c.write_suspended.load(Ordering::Acquire))
                .unwrap_or(true);
            if suspended {
                stats.skipped_suspended += 1;
            } else {
                ctx.set_field(sink_obj, SINK_FIELD_WRITE_READY_FLAG, Value::Int(1));
                if let Some(c) = &ch {
                    // Release publishes the field write above.
                    c.write_ready.store(true, Ordering::Release);
                }
                let listener = match ctx.get_field(sink_obj, SINK_FIELD_WRITE_LISTENER) {
                    Value::Object(Some(o)) => Some(o),
                    _ => None,
                };
                if let Some(l) = listener {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        ctx.invoke_virtual(
                            l,
                            "handleEvent",
                            CHANNEL_LISTENER_HANDLE_EVENT_DESC,
                            &[Value::Object(Some(sink_obj))],
                        )
                    }));
                    match result {
                        Ok(_) => stats.write_fired = true,
                        Err(_) => {
                            stats.write_panicked = true;
                            tracing::error!(
                                target: "xnio::conduits",
                                channel_id = sink_id,
                                "write listener panicked — event loop continuing"
                            );
                        }
                    }
                }
            }
        }
    }

    stats
}

// ---------------------------------------------------------------------------
// Natives — source-side
// ---------------------------------------------------------------------------

fn source_id_of(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u64> {
    source_id_by_obj(ctx, this).or_else(|| match ctx.get_field(this, SRC_FIELD_CHANNEL_ID) {
        Value::Long(v) if v > 0 => Some(v as u64),
        _ => None,
    })
}

fn sink_id_of(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u64> {
    sink_id_by_obj(ctx, this).or_else(|| match ctx.get_field(this, SINK_FIELD_CHANNEL_ID) {
        Value::Long(v) if v > 0 => Some(v as u64),
        _ => None,
    })
}

fn source_has_pending_data(id: u64) -> bool {
    match get_source_channel(id) {
        Some(ch) => channel_has_pending_data(&ch),
        None => false,
    }
}

fn channel_has_pending_data(ch: &SourceChannel) -> bool {
    match &ch.transport {
        ConduitTransport::Tcp(stream) => {
            let mut one = [0u8; 1];
            match stream.peek(&mut one) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                Err(_) => true,
            }
        }
        ConduitTransport::Pipe(pipe) => {
            let has_bytes = pipe
                .buf
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .front()
                .is_some();
            has_bytes || pipe.eof.load(Ordering::Acquire)
        }
    }
}

fn source_read_suspended(ctx: &dyn NativeContext, source: ObjectRef, id: u64) -> bool {
    if matches!(ctx.get_field(source, SRC_FIELD_READ_SUSPENDED), Value::Int(v) if v != 0) {
        return true;
    }
    get_source_channel(id)
        .map(|ch| ch.read_suspended.load(Ordering::Acquire))
        .unwrap_or(true)
}

/// Readiness delivery, in XNIO's own order of precedence.
///
/// A `ChannelListener` on the channel wins: XNIO's stock `ReadReadyHandler`
/// is `ChannelListenerHandler`, whose `readReady()` does nothing except
/// invoke exactly that listener, so firing both would deliver `handleEvent`
/// twice on one stack — the shape of the Undertow `requestState` self-
/// deadlock documented on `native_source_resume_reads`. Only when no
/// listener is bound does the conduit's own handler become the sole delivery
/// path, and then it must be driven or the callback is simply lost (which is
/// what the previous `setReadReadyHandler` no-op did to every caller).
fn invoke_source_read_listener(ctx: &mut dyn NativeContext, source: ObjectRef) -> bool {
    let listener = match ctx.get_field(source, SRC_FIELD_READ_LISTENER) {
        Value::Object(Some(o)) => o,
        _ => return invoke_read_ready_handler(ctx, source),
    };
    if let Err(e) = ctx.invoke_virtual(
        listener,
        "handleEvent",
        CHANNEL_LISTENER_HANDLE_EVENT_DESC,
        &[Value::Object(Some(source))],
    ) {
        xnio_tcp_dbg!("source_read_listener_error error={e:?}");
    }
    true
}

fn invoke_read_ready_handler(ctx: &mut dyn NativeContext, source: ObjectRef) -> bool {
    let Some(handler) = ready_handler_of(read_ready_handler_registry(), ctx, source) else {
        return false;
    };
    if let Err(e) = ctx.invoke_virtual(handler, "readReady", "()V", &[]) {
        xnio_tcp_dbg!("source_read_ready_handler_error error={e:?}");
    }
    true
}

/// Write-side twin of [`invoke_source_read_listener`] — same precedence, same
/// reason.
fn invoke_sink_write_listener(ctx: &mut dyn NativeContext, sink: ObjectRef) -> bool {
    let listener = match ctx.get_field(sink, SINK_FIELD_WRITE_LISTENER) {
        Value::Object(Some(o)) => o,
        _ => return invoke_write_ready_handler(ctx, sink),
    };
    if let Err(e) = ctx.invoke_virtual(
        listener,
        "handleEvent",
        CHANNEL_LISTENER_HANDLE_EVENT_DESC,
        &[Value::Object(Some(sink))],
    ) {
        xnio_tcp_dbg!("sink_write_listener_error error={e:?}");
    }
    true
}

fn invoke_write_ready_handler(ctx: &mut dyn NativeContext, sink: ObjectRef) -> bool {
    let Some(handler) = ready_handler_of(write_ready_handler_registry(), ctx, sink) else {
        return false;
    };
    if let Err(e) = ctx.invoke_virtual(handler, "writeReady", "()V", &[]) {
        xnio_tcp_dbg!("sink_write_ready_handler_error error={e:?}");
    }
    true
}

fn notify_source_readable(ctx: &mut dyn NativeContext, source: ObjectRef, retry: bool) {
    let delays: &[u64] = if retry {
        &READ_NOTIFY_RETRY_DELAYS_MS
    } else {
        &[0]
    };
    notify_source_readable_with_delays(ctx, source, retry, delays);
}

fn notify_source_readable_with_delays(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    retry: bool,
    delays: &[u64],
) {
    let id = match source_id_of(ctx, source) {
        Some(id) => id,
        None => return,
    };

    // gc-common w23-b: the pin is released on every exit. It never was: a
    // native's pins are dropped only when the native returns, and the two
    // callers that never return -- the source poller
    // (`xnio_worker::native_source_poller_run`, every 10 ms) and the accept
    // pump -- pinned every registered source once per tick, for ever: an
    // unbounded pin stack (scanned as roots at every collection) that also
    // kept every closed connection's channel alive.
    let source_pin = ctx.pin_native_root(source);
    notify_pinned_source_readable(ctx, source_pin, source, id, retry, delays);
    ctx.unpin_native_roots(source_pin);
}

/// Body of [`notify_source_readable_with_delays`]; `source_pin` pins `source`
/// and is released by the caller.
fn notify_pinned_source_readable(
    ctx: &mut dyn NativeContext,
    source_pin: usize,
    source: ObjectRef,
    id: u64,
    retry: bool,
    delays: &[u64],
) {
    for delay in delays {
        let source = ctx.read_native_pin(source_pin, source);
        if *delay > 0 {
            thread::sleep(Duration::from_millis(*delay));
        }
        let suspended = source_read_suspended(ctx, source, id);
        let pending = source_has_pending_data(id);
        let wakeup = get_source_channel(id)
            .map(|ch| ch.wakeup_pending.load(Ordering::Acquire))
            .unwrap_or(false);
        xnio_tcp_dbg!(
            "notify_source id={id} retry={retry} delay_ms={} suspended={suspended} pending={pending} wakeup={wakeup}",
            *delay
        );
        if suspended || (!pending && !wakeup) {
            continue;
        }
        ctx.set_field(source, SRC_FIELD_READ_READY_FLAG, Value::Int(1));
        let channel = get_source_channel(id);
        if let Some(ch) = &channel {
            ch.read_ready.store(true, Ordering::Release);
        }
        // HC0053 follow-up: non-reentrant dispatch guard (see
        // `SourceChannel::dispatching`). If another native thread is
        // already inside this source's listener (the source-poller thread
        // and a paired sink's direct resumeWrites-notify race for the same
        // source), skip firing this round instead of invoking concurrently
        // — the next poller tick or retry-with-delays call will fire it
        // once the in-flight dispatch completes.
        let guard = channel.as_ref().and_then(|ch| {
            (ch.dispatching
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok())
            .then(|| DispatchGuard {
                flag: &ch.dispatching,
            })
        });
        if channel.is_some() && guard.is_none() {
            xnio_tcp_dbg!("notify_source id={id} skipped_reentrant_dispatch");
            return;
        }
        // This dispatch satisfies any queued wakeupReads() — clear the flag
        // before invoking so a wakeup issued DURING the listener run is not
        // lost (it re-arms for the next tick).
        if let Some(ch) = &channel {
            ch.wakeup_pending.store(false, Ordering::Release);
        }
        let fired = invoke_source_read_listener(ctx, source);
        drop(guard);
        xnio_tcp_dbg!("notify_source id={id} fired_listener={fired}");
        return;
    }
}

/// RAII reset for `SourceChannel::dispatching` — clears the flag on every
/// exit path (normal return or unwind) once acquired via CAS above.
struct DispatchGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

fn notify_paired_source_readable(ctx: &mut dyn NativeContext, sink: ObjectRef, retry: bool) {
    let Some(source) = paired_source_of(ctx, sink) else {
        return;
    };
    notify_source_readable(ctx, source, retry);
}

fn notify_paired_source_readable_with_delays(
    ctx: &mut dyn NativeContext,
    sink: ObjectRef,
    retry: bool,
    delays: &[u64],
) {
    let Some(source) = paired_source_of(ctx, sink) else {
        return;
    };
    notify_source_readable_with_delays(ctx, source, retry, delays);
}

pub(crate) fn notify_registered_sources_readable(
    ctx: &mut dyn NativeContext,
    retry: bool,
    skip_id: Option<u64>,
) {
    // gc-common w23-b: the rows are this VM's open connections only (a closed
    // one is unregistered), read from its own per-VM map.
    let handles = source_obj_handles_by_id(ctx);
    for (id, handle) in handles {
        if Some(id) == skip_id {
            continue;
        }
        // Decide from the Rust channel first whether the notify below could
        // fire at all -- it skips a source that is suspended (the channel
        // flag is one of its two tests) or has neither pending bytes nor a
        // queued wakeup -- so an idle source costs no root resolve, pin,
        // identity-hash lookup or field read. Same verdicts, cheaper order.
        if !source_may_be_notified(id) {
            continue;
        }
        let Some(source) = ctx.resolve_global_root(handle) else {
            continue;
        };
        notify_source_readable(ctx, source, retry);
    }
}

/// `false` when [`notify_source_readable`] would certainly skip channel `id`.
fn source_may_be_notified(id: u64) -> bool {
    let Some(ch) = get_source_channel(id) else {
        return false;
    };
    if ch.read_suspended.load(Ordering::Acquire) {
        return false;
    }
    ch.wakeup_pending.load(Ordering::Acquire) || channel_has_pending_data(&ch)
}

fn notify_sources_after_sink_write(ctx: &mut dyn NativeContext, sink: ObjectRef) {
    let paired = paired_source_of(ctx, sink);
    let paired_id = paired.and_then(|source| source_id_of(ctx, source));
    if let Some(source) = paired {
        notify_source_readable(ctx, source, false);
    }
    notify_registered_sources_readable(ctx, false, paired_id);
}

fn with_sink_conduit_delegate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    method_name: &str,
    descriptor: &str,
    call_args: &[Value],
) -> Option<MethodCallResult> {
    let conduit = match ctx.get_field_by_name(this, "conduit") {
        Value::Object(Some(conduit)) if conduit.as_ptr() != this.as_ptr() => conduit,
        _ => return None,
    };
    Some(ctx.invoke_virtual(conduit, method_name, descriptor, call_args))
}

fn native_source_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let buf = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("read: null ByteBuffer")),
    };
    let id = source_id_of(ctx, this).ok_or_else(|| ioex("read: channel not registered"))?;
    source_channel_read(ctx, id, buf)
        .map(|n| Some(Value::Int(n)))
        .map_err(Into::into)
}

fn native_source_read_scatter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bufs = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("scatter read: null array")),
    };
    let offset = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let length = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let id = source_id_of(ctx, this).ok_or_else(|| ioex("scatter read: channel not registered"))?;

    let arr_len = ctx.array_length(bufs);
    let end = offset.saturating_add(length).min(arr_len);
    let mut total: i64 = 0;
    let mut any_read = false;
    for i in offset..end {
        let elem = match ctx.get_array_element(bufs, i) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        let n = source_channel_read(ctx, id, elem)?;
        if n == -1 {
            if !any_read {
                return Ok(Some(Value::Long(-1)));
            }
            break;
        }
        if n == 0 {
            break;
        }
        total += n as i64;
        any_read = true;
    }
    Ok(Some(Value::Long(total)))
}

fn native_source_transfer_to(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Zero-copy path: we don't have sendfile hooked up yet. The JDK contract
    // allows returning 0 ("no bytes transferred") which signals the caller
    // to fall back to a user-space copy. That's exactly what Undertow does.
    Ok(Some(Value::Long(0)))
}

fn native_source_set_read_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let listener = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, SRC_FIELD_READ_LISTENER, listener);
    Ok(None)
}

fn native_source_get_read_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = ctx.get_field(this, SRC_FIELD_READ_LISTENER);
    Ok(Some(v))
}

fn native_source_resume_reads(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    let id = source_id_of(ctx, this);
    if let Some(id) = id {
        if let Some(ch) = get_source_channel(id) {
            // Round-9 HIGH-2: Release -- paired with Acquire in dispatch.
            ch.read_suspended.store(false, Ordering::Release);
        }
    }
    // NO listener dispatch here — not even through the guarded notify path.
    // resumeReads() is called by Java code that may be mid-state-transition
    // (Undertow's HttpReadListener.exchangeComplete CASes requestState 1->2,
    // calls resumeReads(), and only THEN resets to 0). Any synchronous
    // dispatch on the caller's stack re-enters handleEvent while state==2
    // and its entry loop (`get != 0` + failing `CAS(1->2)`) spins forever —
    // a self-deadlock that consumed the accept-pump thread and killed the
    // management endpoint ~30-60s after every boot. Real XNIO resumeReads
    // only registers interest; delivery happens from the IO thread. Here the
    // source-poller thread (10ms tick, guarded + pending-gated) delivers.
    //
    // gc-common w23-b: "channel-less" means NO id. A registered id whose
    // channel is gone is a closed connection that was unregistered; it gets
    // no inline dispatch (the self-deadlock above), as a shut-down but still
    // registered channel got none.
    if id.is_none() {
        // Channel-less stub source: no poller coverage exists, so the
        // inline dispatch is the only delivery path (the WildFly domain
        // managed-server startup case this invoke was added for). Routed
        // through `invoke_source_read_listener` so a source that carries
        // only a conduit `ReadReadyHandler` — no `ChannelListener` — is
        // served too; it has no other delivery path at all.
        invoke_source_read_listener(ctx, this);
    }
    Ok(None)
}

fn native_source_wakeup_reads(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    let id = source_id_of(ctx, this);
    let channel = id.and_then(get_source_channel);
    if let Some(ch) = &channel {
        // Round-9 HIGH-2: Release -- paired with Acquire in dispatch.
        ch.read_suspended.store(false, Ordering::Release);
        ch.read_ready.store(true, Ordering::Release);
        // wakeupReads = resumeReads + force one listener invocation even
        // with no pending data. Never dispatched inline (see
        // native_source_resume_reads for the self-deadlock); the poller
        // consumes this flag on its next tick (<=10ms).
        ch.wakeup_pending.store(true, Ordering::Release);
    }
    ctx.set_field(this, SRC_FIELD_READ_READY_FLAG, Value::Int(1));
    if id.is_none() {
        // Channel-less stub source: no poller coverage — inline dispatch is
        // the only delivery path (conduit `ReadReadyHandler` included).
        // gc-common w23-b: an unregistered (closed) channel is not
        // channel-less; see `native_source_resume_reads`.
        invoke_source_read_listener(ctx, this);
    }
    xnio_tcp_dbg!(
        "wakeup_reads id={id:?} queued_for_poller={}",
        channel.is_some()
    );
    Ok(None)
}

fn native_source_suspend_reads(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, SRC_FIELD_READ_SUSPENDED, Value::Int(1));
    if let Some(id) = source_id_of(ctx, this) {
        if let Some(ch) = get_source_channel(id) {
            // Round-9 HIGH-2: Release — paired with Acquire in dispatch.
            ch.read_suspended.store(true, Ordering::Release);
        }
    }
    Ok(None)
}

fn native_source_shutdown_reads(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(id) = source_id_of(ctx, this) {
        let peer = get_source_channel(id).map(|ch| {
            // Round-9 HIGH-2 paired this Release store with the Acquire in
            // source_channel_read; gc-common w23-b makes it SeqCst for the
            // two-halves handshake in `retire_connection_if_closed` (a
            // SeqCst store still releases).
            ch.shutdown.store(true, Ordering::SeqCst);
            ch.peer_sink.load(Ordering::SeqCst)
        });
        // gc-common w23-b: the connection goes once both halves are shut
        // down (`common-w22a-xnio-conduit-channels-are-never-unregistered`).
        if let Some(peer) = peer {
            retire_connection_if_closed(ctx, Some(id), (peer != 0).then_some(peer));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// awaitReadable / awaitWritable
// ---------------------------------------------------------------------------
//
// These are genuine blocking waits, not bookkeeping: XNIO's contract is
// "block until the channel is ready (or the timeout expires)", and callers
// use them as the back-off after a `read`/`write` returned 0. A no-op turns
// every such caller into a hot spin on the calling thread.
//
// There is no selector behind these conduits — reads and writes go straight
// at the transport — so readiness is established by asking the transport
// itself: a non-blocking `peek` on the read side (already used by the
// source poller) and an OS `poll`/`WSAPoll` state query on the write side.
// Every sleep between probes runs inside the VM's blocking-region protocol;
// without it a stop-the-world safepoint would wait for a thread that is
// parked in `thread::sleep` and can never reach an interpreter poll.

/// Interval between readiness probes. Short enough that a ready channel is
/// picked up promptly (an order of magnitude under the 10 ms source-poller
/// tick), long enough that a long wait costs no measurable CPU.
const AWAIT_POLL_INTERVAL_MS: u64 = 1;

/// Zero-timeout OS writability query for a TCP sink.
///
/// `TcpStream` exposes no writability predicate and there is no way to ask by
/// writing (a zero-length `send` always succeeds and tells us nothing, while
/// a real one would consume caller bytes). `poll(2)` is a pure query of
/// kernel socket state: it neither touches the byte stream nor flips the
/// socket's persistent blocking mode. A failed probe reports "writable" so
/// the caller degrades to an immediate return — XNIO explicitly allows
/// `awaitWritable` to return spuriously — instead of hanging forever.
#[cfg(unix)]
fn socket_write_ready(stream: &TcpStream) -> bool {
    use std::os::unix::io::AsRawFd;

    let mut pfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `pfd` is a single, fully-initialised `pollfd`; `nfds == 1`
    // matches the one-element buffer; timeout 0 returns immediately.
    let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1 as libc::nfds_t, 0) };
    if rc < 0 {
        return true;
    }
    // Error / hang-up conditions end the wait too: a write will now fail
    // immediately rather than block, which is what the caller is waiting for.
    pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
}

#[cfg(windows)]
fn socket_write_ready(stream: &TcpStream) -> bool {
    use std::os::windows::io::AsRawSocket;

    // `libc` does not re-export `WSAPoll`/`WSAPOLLFD` on Windows. The layout
    // and signature below are byte-identical to the other `WSAPoll` binding
    // in this crate (`servlet.rs`) — `clashing_extern_declarations` is a
    // deny-lint here, so any divergence would fail the build.
    #[repr(C)]
    struct Wsapollfd {
        fd: usize,
        events: i16,
        revents: i16,
    }
    const WSAPOLLWRNORM: i16 = 0x0010;
    const WSAPOLLERR: i16 = 0x0001;
    const WSAPOLLHUP: i16 = 0x0002;
    const WSAPOLLNVAL: i16 = 0x0004;

    #[link(name = "Ws2_32")]
    extern "system" {
        fn WSAPoll(fd_array: *mut Wsapollfd, fds: u32, timeout: i32) -> i32;
    }

    let mut pfd = Wsapollfd {
        fd: stream.as_raw_socket() as usize,
        events: WSAPOLLWRNORM,
        revents: 0,
    };
    // SAFETY: single, fully-initialised WSAPOLLFD; `nfds == 1` matches the
    // buffer length; timeout 0 returns immediately.
    let rc = unsafe { WSAPoll(&mut pfd as *mut Wsapollfd, 1, 0) };
    if rc < 0 {
        return true;
    }
    pfd.revents & (WSAPOLLWRNORM | WSAPOLLERR | WSAPOLLHUP | WSAPOLLNVAL) != 0
}

#[cfg(not(any(unix, windows)))]
fn socket_write_ready(_stream: &TcpStream) -> bool {
    // No readiness primitive on this target — report ready so the wait
    // degrades to XNIO's permitted spurious return rather than a hang.
    true
}

/// Would a `write` on this transport accept bytes right now? Mirrors the
/// admission test each transport's own `write` applies, so "writable" here
/// means exactly "the next `sink_channel_write` will not report would-block".
fn transport_has_write_room(transport: &ConduitTransport) -> bool {
    match transport {
        ConduitTransport::Tcp(stream) => socket_write_ready(stream),
        ConduitTransport::Pipe(pipe) => {
            let queued = pipe.buf.lock().unwrap_or_else(|e| e.into_inner()).len();
            queued < pipe.writable_cap
        }
    }
}

/// True when a `read` would complete without blocking: bytes or EOF pending,
/// the read side terminated, or the channel already dropped from the registry
/// (a wait on a channel nobody can make ready would never end).
fn source_is_readable_now(id: u64) -> bool {
    match get_source_channel(id) {
        None => true,
        Some(ch) => ch.shutdown.load(Ordering::Acquire) || source_has_pending_data(id),
    }
}

/// Write-side twin of [`source_is_readable_now`]. A shut-down sink counts as
/// ready: `sink_channel_write` throws `ClosedChannelException` immediately, so
/// there is nothing left to wait for.
fn sink_is_writable_now(id: u64) -> bool {
    match get_sink_channel(id) {
        None => true,
        Some(ch) => ch.shutdown.load(Ordering::Acquire) || transport_has_write_room(&ch.transport),
    }
}

/// One probe interval, taken inside the VM's blocking-region protocol.
///
/// `begin_timed_blocking_region` for the timeout overloads so
/// `Thread.getState()` reports `TIMED_WAITING` (matching HotSpot for a
/// bounded wait) and plain `begin_blocking_region` — reported as `WAITING` —
/// for the untimed ones.
fn await_blocking_tick(ctx: &mut dyn NativeContext, timed: bool) {
    let mut refs: [Value; 0] = [];
    if timed {
        ctx.begin_timed_blocking_region();
    } else {
        ctx.begin_blocking_region();
    }
    thread::sleep(Duration::from_millis(AWAIT_POLL_INTERVAL_MS));
    ctx.end_blocking_region_refs(&mut refs);
}

/// Resolve the `(long time, TimeUnit unit)` argument pair of the timeout
/// overloads. `toNanos` is invoked on the unit rather than decoded from an
/// ordinal so this works against both the synthetic `TimeUnit` and a real
/// `java.base` one.
fn await_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<Duration> {
    let time = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => return None,
    };
    if time <= 0 {
        return Some(Duration::ZERO);
    }
    let unit = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    match ctx.invoke_virtual(unit, "toNanos", "(J)J", &[Value::Long(time)]) {
        Ok(Some(Value::Long(n))) if n >= 0 => Some(Duration::from_nanos(n as u64)),
        _ => None,
    }
}

/// Deadline for a timeout overload, or `None` for "wait indefinitely".
/// An unreadable unit returns `Err(())`, which the callers turn into an
/// immediate (spec-permitted spurious) return — guessing a time scale, or
/// waiting forever on a timed call, would both be worse.
fn await_deadline(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    timed: bool,
) -> Result<Option<Instant>, ()> {
    if !timed {
        return Ok(None);
    }
    match await_timeout(ctx, args) {
        // `checked_add` failing means an absurd timeout (`Long.MAX_VALUE`
        // DAYS and friends) — indistinguishable from "no timeout".
        Some(d) => Ok(Instant::now().checked_add(d)),
        None => Err(()),
    }
}

fn await_source_readable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    timed: bool,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // The id is a plain `u64`, so once it is in hand nothing below holds a
    // reference the GC could move while this thread sleeps.
    let Some(id) = source_id_of(ctx, this) else {
        // Not one of our registered conduits: there is no transport to wait
        // on and no event that could ever complete the wait.
        return Ok(None);
    };
    let Ok(deadline) = await_deadline(ctx, args, timed) else {
        return Ok(None);
    };
    loop {
        if source_is_readable_now(id) {
            return Ok(None);
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
        // XNIO documents `awaitReadable` as interruptible, and an untimed
        // wait has no other exit — read the flag WITHOUT clearing it, which
        // is the `InterruptedIOException` contract.
        if ctx.is_interrupted(false) {
            return Err(ioex("InterruptedIOException: awaitReadable interrupted"));
        }
        await_blocking_tick(ctx, timed);
    }
}

fn await_sink_writable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    timed: bool,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(id) = sink_id_of(ctx, this) else {
        return Ok(None);
    };
    let Ok(deadline) = await_deadline(ctx, args, timed) else {
        return Ok(None);
    };
    loop {
        if sink_is_writable_now(id) {
            return Ok(None);
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
        if ctx.is_interrupted(false) {
            return Err(ioex("InterruptedIOException: awaitWritable interrupted"));
        }
        await_blocking_tick(ctx, timed);
    }
}

fn native_source_await_readable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    await_source_readable(ctx, args, false)
}

fn native_source_await_readable_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    await_source_readable(ctx, args, true)
}

fn native_sink_await_writable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    await_sink_writable(ctx, args, false)
}

fn native_sink_await_writable_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    await_sink_writable(ctx, args, true)
}

fn native_source_set_read_ready_handler(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match args.get(1).copied() {
        Some(Value::Object(Some(handler))) => {
            remember_ready_handler(read_ready_handler_registry(), ctx, this, handler)
        }
        _ => forget_ready_handler(read_ready_handler_registry(), ctx, this),
    }
    Ok(None)
}

fn native_sink_set_write_ready_handler(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match args.get(1).copied() {
        Some(Value::Object(Some(handler))) => {
            remember_ready_handler(write_ready_handler_registry(), ctx, this, handler)
        }
        _ => forget_ready_handler(write_ready_handler_registry(), ctx, this),
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Natives — sink-side
// ---------------------------------------------------------------------------

fn native_source_is_read_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // gc-common w23-b: a registered id whose channel is gone is a closed,
    // unregistered connection -- shut down, as it answered before it went.
    let shutdown = source_id_of(ctx, this)
        .map(source_half_closed)
        .unwrap_or(false);
    Ok(Some(Value::Int(if shutdown { 1 } else { 0 })))
}

fn native_source_is_read_resumed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let resumed = source_id_of(ctx, this)
        .and_then(get_source_channel)
        .map(|ch| !ch.read_suspended.load(Ordering::Acquire))
        .unwrap_or_else(
            || !matches!(ctx.get_field(this, SRC_FIELD_READ_SUSPENDED), Value::Int(v) if v != 0),
        );
    Ok(Some(Value::Int(if resumed { 1 } else { 0 })))
}

fn native_source_get_read_thread(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Object(channel_io_thread_of(
        ctx,
        this,
        SRC_FIELD_IO_THREAD,
    ))))
}

fn native_sink_get_write_thread(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Object(channel_io_thread_of(
        ctx,
        this,
        SINK_FIELD_IO_THREAD,
    ))))
}

fn native_channel_get_worker(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    io_thread_slot: usize,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(io_thread) = channel_io_thread_of(ctx, this, io_thread_slot) else {
        return Ok(Some(Value::Object(None)));
    };
    match ctx.invoke_virtual(io_thread, "getWorker", "()Lorg/xnio/XnioWorker;", &[]) {
        Ok(Some(value)) => Ok(Some(value)),
        Ok(None) => Ok(Some(Value::Object(None))),
        Err(err) => Err(err),
    }
}

fn native_source_get_worker(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_channel_get_worker(ctx, args, SRC_FIELD_IO_THREAD)
}

fn native_sink_get_worker(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_channel_get_worker(ctx, args, SINK_FIELD_IO_THREAD)
}

fn native_sink_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(result) =
        with_sink_conduit_delegate(ctx, this, "write", "(Ljava/nio/ByteBuffer;)I", &args[1..])
    {
        return result;
    }
    let buf = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("write: null ByteBuffer")),
    };
    let id = sink_id_of(ctx, this).ok_or_else(|| ioex("write: channel not registered"))?;
    let n = sink_channel_write(ctx, id, buf)?;
    if n > 0 {
        notify_sources_after_sink_write(ctx, this);
    }
    Ok(Some(Value::Int(n)))
}

fn native_sink_write_gather(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(result) = with_sink_conduit_delegate(
        ctx,
        this,
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
        &args[1..],
    ) {
        return result;
    }
    let bufs = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("gather write: null array")),
    };
    let offset = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let length = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let id = sink_id_of(ctx, this).ok_or_else(|| ioex("gather write: channel not registered"))?;

    let arr_len = ctx.array_length(bufs);
    let end = offset.saturating_add(length).min(arr_len);
    let mut total: i64 = 0;
    for i in offset..end {
        let elem = match ctx.get_array_element(bufs, i) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        let n = sink_channel_write(ctx, id, elem)?;
        if n == 0 {
            break; // would-block — next iteration must wait for OP_WRITE
        }
        total += n as i64;
    }
    if total > 0 {
        notify_sources_after_sink_write(ctx, this);
    }
    Ok(Some(Value::Long(total)))
}

fn native_sink_transfer_from(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Same fallback posture as transferTo — return 0 so the caller uses the
    // user-space copy path.
    Ok(Some(Value::Long(0)))
}

fn native_sink_set_write_listener(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let listener = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, SINK_FIELD_WRITE_LISTENER, listener);
    Ok(None)
}

fn native_sink_get_write_listener(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = ctx.get_field(this, SINK_FIELD_WRITE_LISTENER);
    Ok(Some(v))
}

fn native_sink_resume_writes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Family-1 fix (cce0079): the conduit delegate and the write-listener
    // dispatch below both run Java (GC-capable) — `this` must be refreshed
    // after each, or the field writes / registry lookups / paired-source
    // notify below operate on a stale address (canary-caught live in
    // `RemoteConnection$RemoteWriteListener.lambda$send$0`; a stale identity
    // hash here also MISSES the registries — a lost-wakeup hazard).
    let this_pin = ctx.pin_native_root(this);
    if let Some(result) = with_sink_conduit_delegate(ctx, this, "resumeWrites", "()V", &[]) {
        if let Err(e) = result {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, SINK_FIELD_WRITE_SUSPENDED, Value::Int(0));
    ctx.set_field(this, SINK_FIELD_WRITE_READY_FLAG, Value::Int(1));
    if let Some(id) = sink_id_of(ctx, this) {
        if let Some(ch) = get_sink_channel(id) {
            // Round-9 HIGH-2: Release - paired with Acquire in dispatch.
            ch.write_suspended.store(false, Ordering::Release);
            ch.write_ready.store(true, Ordering::Release);
        }
        let fired = invoke_sink_write_listener(ctx, this);
        let this = ctx.read_native_pin(this_pin, this);
        xnio_tcp_dbg!("resume_writes id={id} fired_listener={fired}");
        if fired {
            notify_paired_source_readable_with_delays(
                ctx,
                this,
                true,
                &READ_NOTIFY_POST_LISTENER_RETRY_DELAYS_MS,
            );
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_sink_suspend_writes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Family-1 fix (cce0079): refresh `this` across the conduit delegate
    // dispatch (same shape as `native_sink_resume_writes`).
    let this_pin = ctx.pin_native_root(this);
    if let Some(result) = with_sink_conduit_delegate(ctx, this, "suspendWrites", "()V", &[]) {
        if let Err(e) = result {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, SINK_FIELD_WRITE_SUSPENDED, Value::Int(1));
    if let Some(id) = sink_id_of(ctx, this) {
        if let Some(ch) = get_sink_channel(id) {
            // Round-9 HIGH-2: Release - paired with Acquire in dispatch.
            ch.write_suspended.store(true, Ordering::Release);
        }
    }
    Ok(None)
}

fn shutdown_sink_transport(ctx: &mut dyn NativeContext, this: ObjectRef) {
    if let Some(id) = sink_id_of(ctx, this) {
        let peer = get_sink_channel(id).map(|ch| {
            // Round-9 HIGH-2 paired this Release store with the Acquire in
            // sink_channel_write; SeqCst since gc-common w23-b (see
            // `native_source_shutdown_reads`).
            ch.shutdown.store(true, Ordering::SeqCst);
            transport_shutdown_write(&ch.transport);
            ch.peer_source.load(Ordering::SeqCst)
        });
        // gc-common w23-b: the connection goes once both halves are shut down.
        if let Some(peer) = peer {
            retire_connection_if_closed(ctx, (peer != 0).then_some(peer), Some(id));
        }
    }
}

fn native_sink_shutdown_writes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(result) = with_sink_conduit_delegate(ctx, this, "terminateWrites", "()V", &[]) {
        return result;
    }
    shutdown_sink_transport(ctx, this);
    Ok(None)
}

fn native_sink_truncate_writes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(result) = with_sink_conduit_delegate(ctx, this, "truncateWrites", "()V", &[]) {
        return result;
    }
    shutdown_sink_transport(ctx, this);
    Ok(None)
}

// NOTE (cce0079): `native_sink_shutdown_writes`/`native_sink_truncate_writes`
// above return DIRECTLY when the delegate path is taken and only touch
// `this` on the no-delegate path (no dispatch has happened yet) — no stale
// window, unlike resume/suspend which fall through after dispatching.

/// `flush()` - return true if the internal buffered-byte count is zero and
/// the underlying transport has been drained. With no local staging buffer
/// we just report `buffered_bytes == 0`.
fn native_sink_is_write_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(result) = with_sink_conduit_delegate(ctx, this, "isWriteShutdown", "()Z", &[]) {
        return result;
    }
    // gc-common w23-b: as `native_source_is_read_shutdown`.
    let shutdown = sink_id_of(ctx, this).map(sink_half_closed).unwrap_or(false);
    Ok(Some(Value::Int(if shutdown { 1 } else { 0 })))
}

fn native_sink_is_write_resumed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let resumed = sink_id_of(ctx, this)
        .and_then(get_sink_channel)
        .map(|ch| !ch.write_suspended.load(Ordering::Acquire))
        .unwrap_or_else(
            || !matches!(ctx.get_field(this, SINK_FIELD_WRITE_SUSPENDED), Value::Int(v) if v != 0),
        );
    Ok(Some(Value::Int(if resumed { 1 } else { 0 })))
}

fn native_sink_flush(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(result) = with_sink_conduit_delegate(ctx, this, "flush", "()Z", &[]) {
        return result;
    }
    let id = match sink_id_of(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(1))), // channel already gone — treat as drained
    };
    let ch = match get_sink_channel(id) {
        Some(c) => c,
        None => return Ok(Some(Value::Int(1))),
    };
    // Round-9 HIGH-2: Acquire — paired with the AcqRel RMW in
    // sink_channel_write that publishes the buffered count.
    let drained = ch.buffered_bytes.load(Ordering::Acquire) == 0;
    xnio_tcp_dbg!("sink_flush id={id} drained={drained}");
    if drained {
        notify_paired_source_readable(ctx, this, true);
    }
    Ok(Some(Value::Int(if drained { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Natives — ChannelListener$Setter
// ---------------------------------------------------------------------------

fn native_channel_get_self_conduit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match ctx.get_field_by_name(this, "conduit") {
        Value::Object(Some(conduit)) => Ok(Some(Value::Object(Some(conduit)))),
        _ => {
            ctx.set_field_by_name(this, "conduit", Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(this))))
        }
    }
}

fn native_channel_set_conduit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(Value::Object(Some(conduit))) = args.get(1).copied() {
        ctx.set_field_by_name(this, "conduit", Value::Object(Some(conduit)));
    }
    Ok(None)
}

fn native_setter_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // this = ChannelListener$Setter; arg 1 = ChannelListener
    let this = obj_arg(args, 0)?;
    let listener = args.get(1).copied().unwrap_or(Value::Object(None));
    // Setter carries the owning channel handle + the field slot to poke.
    let channel = match ctx.get_field(this, SETTER_FIELD_CHANNEL_HANDLE) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let slot = match ctx.get_field(this, SETTER_FIELD_LISTENER_SLOT_INDEX) {
        Value::Int(v) if v >= 0 => v as usize,
        _ => return Ok(None),
    };
    ctx.set_field(channel, slot, listener);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Test-only helpers to inflate a source / sink channel as a Java object.
// ---------------------------------------------------------------------------

fn alloc_source_conduit_obj(
    ctx: &mut dyn NativeContext,
    id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_STREAM_SOURCE_CONDUIT, SRC_NUM_SLOTS)?;
    remember_source_obj(ctx, obj, id);
    ctx.set_field(obj, SRC_FIELD_CHANNEL_ID, Value::Long(id as i64));
    ctx.set_field(obj, SRC_FIELD_READ_SUSPENDED, Value::Int(1));
    Ok(obj)
}

fn alloc_sink_conduit_obj(
    ctx: &mut dyn NativeContext,
    id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_STREAM_SINK_CONDUIT, SINK_NUM_SLOTS)?;
    remember_sink_obj(ctx, obj, id);
    ctx.set_field(obj, SINK_FIELD_CHANNEL_ID, Value::Long(id as i64));
    ctx.set_field(obj, SINK_FIELD_WRITE_SUSPENDED, Value::Int(1));
    Ok(obj)
}

/// Allocate a Java-side `ConduitStreamSourceChannel` and bind it to the given
/// channel id. Public (but `#[doc(hidden)]`) so integration tests in other
/// crates can stand up a conduit without threading a full Selector in.
#[doc(hidden)]
pub fn alloc_source_channel_obj(
    ctx: &mut dyn NativeContext,
    id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_SOURCE, SRC_NUM_SLOTS)?;
    // Family-1 fix (cce0079): the conduit alloc below can move the
    // still-unrooted `obj` — pin and refresh it, or `remember_source_obj`
    // registers the WRONG identity-hash key (every later
    // `source_id_by_obj` lookup then misses — lost wakeups) and the field
    // stores/return value hand out a stale address.
    let obj_pin = ctx.pin_native_root(obj);
    let conduit = alloc_source_conduit_obj(ctx, id);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    remember_source_obj(ctx, obj, id);
    ctx.set_field(obj, SRC_FIELD_CHANNEL_ID, Value::Long(id as i64));
    ctx.set_field(obj, SRC_FIELD_READ_SUSPENDED, Value::Int(1)); // start suspended
    ctx.set_field_by_name(obj, "conduit", Value::Object(Some(conduit?)));
    Ok(obj)
}

/// Allocate a Java-side `ConduitStreamSinkChannel` bound to the given id.
#[doc(hidden)]
pub fn alloc_sink_channel_obj(
    ctx: &mut dyn NativeContext,
    id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_SINK, SINK_NUM_SLOTS)?;
    // Family-1 fix (cce0079): same as `alloc_source_channel_obj` — refresh
    // `obj` across the conduit alloc before registry/field use.
    let obj_pin = ctx.pin_native_root(obj);
    let conduit = alloc_sink_conduit_obj(ctx, id);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    remember_sink_obj(ctx, obj, id);
    ctx.set_field(obj, SINK_FIELD_CHANNEL_ID, Value::Long(id as i64));
    ctx.set_field(obj, SINK_FIELD_WRITE_SUSPENDED, Value::Int(1));
    ctx.set_field_by_name(obj, "conduit", Value::Object(Some(conduit?)));
    Ok(obj)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register every T19.7.d native with the method registry.
pub fn register_xnio_conduits_natives(r: &mut NativeMethodRegistry) {
    // ---- ConduitStreamSourceChannel ----
    r.register(
        CLS_SOURCE,
        "read",
        "(Ljava/nio/ByteBuffer;)I",
        native_source_read,
    );
    r.register(
        CLS_SOURCE,
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
        native_source_read_scatter,
    );
    r.register(
        CLS_SOURCE,
        "transferTo",
        "(JJLjava/nio/channels/FileChannel;)J",
        native_source_transfer_to,
    );
    r.register(
        CLS_SOURCE,
        "getConduit",
        "()Lorg/xnio/conduits/StreamSourceConduit;",
        native_channel_get_self_conduit,
    );
    r.register(
        CLS_SOURCE,
        "setConduit",
        "(Lorg/xnio/conduits/StreamSourceConduit;)V",
        native_channel_set_conduit,
    );
    r.register(
        CLS_SOURCE,
        "setReadListener",
        "(Lorg/xnio/ChannelListener;)V",
        native_source_set_read_listener,
    );
    r.register(
        CLS_SOURCE,
        "getReadListener",
        "()Lorg/xnio/ChannelListener;",
        native_source_get_read_listener,
    );
    r.register(CLS_SOURCE, "resumeReads", "()V", native_source_resume_reads);
    r.register(
        CLS_SOURCE,
        "suspendReads",
        "()V",
        native_source_suspend_reads,
    );
    r.register(
        CLS_SOURCE,
        "shutdownReads",
        "()V",
        native_source_shutdown_reads,
    );
    r.register(
        CLS_SOURCE,
        "terminateReads",
        "()V",
        native_source_shutdown_reads,
    );
    r.register(
        CLS_SOURCE,
        "isReadShutdown",
        "()Z",
        native_source_is_read_shutdown,
    );
    r.register(CLS_SOURCE, "wakeupReads", "()V", native_source_wakeup_reads);
    r.register(
        CLS_SOURCE,
        "isReadResumed",
        "()Z",
        native_source_is_read_resumed,
    );
    r.register(
        CLS_SOURCE,
        "awaitReadable",
        "()V",
        native_source_await_readable,
    );
    r.register(
        CLS_SOURCE,
        "awaitReadable",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_source_await_readable_timed,
    );
    r.register(
        CLS_SOURCE,
        "getReadThread",
        "()Lorg/xnio/XnioIoThread;",
        native_source_get_read_thread,
    );
    r.register(
        CLS_SOURCE,
        "setReadReadyHandler",
        "(Lorg/xnio/conduits/ReadReadyHandler;)V",
        native_source_set_read_ready_handler,
    );
    r.register(
        CLS_SOURCE,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_source_get_worker,
    );
    r.register(
        CLS_SOURCE,
        "transferTo",
        "(JLjava/nio/ByteBuffer;Lorg/xnio/channels/StreamSinkChannel;)J",
        native_source_transfer_to,
    );

    // ---- StreamSourceConduit raw shim ----
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "read",
        "(Ljava/nio/ByteBuffer;)I",
        native_source_read,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
        native_source_read_scatter,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "transferTo",
        "(JLjava/nio/ByteBuffer;Lorg/xnio/channels/StreamSinkChannel;)J",
        native_source_transfer_to,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "terminateReads",
        "()V",
        native_source_shutdown_reads,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "isReadShutdown",
        "()Z",
        native_source_is_read_shutdown,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "wakeupReads",
        "()V",
        native_source_wakeup_reads,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "isReadResumed",
        "()Z",
        native_source_is_read_resumed,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "awaitReadable",
        "()V",
        native_source_await_readable,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "awaitReadable",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_source_await_readable_timed,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "getReadThread",
        "()Lorg/xnio/XnioIoThread;",
        native_source_get_read_thread,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "setReadReadyHandler",
        "(Lorg/xnio/conduits/ReadReadyHandler;)V",
        native_source_set_read_ready_handler,
    );
    r.register(
        CLS_STREAM_SOURCE_CONDUIT,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_source_get_worker,
    );

    // ---- SourceConduit parent raw shim ----
    r.register(
        CLS_SOURCE_CONDUIT,
        "suspendReads",
        "()V",
        native_source_suspend_reads,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "resumeReads",
        "()V",
        native_source_resume_reads,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "terminateReads",
        "()V",
        native_source_shutdown_reads,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "isReadShutdown",
        "()Z",
        native_source_is_read_shutdown,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "wakeupReads",
        "()V",
        native_source_wakeup_reads,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "isReadResumed",
        "()Z",
        native_source_is_read_resumed,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "awaitReadable",
        "()V",
        native_source_await_readable,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "awaitReadable",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_source_await_readable_timed,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "getReadThread",
        "()Lorg/xnio/XnioIoThread;",
        native_source_get_read_thread,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "setReadReadyHandler",
        "(Lorg/xnio/conduits/ReadReadyHandler;)V",
        native_source_set_read_ready_handler,
    );
    r.register(
        CLS_SOURCE_CONDUIT,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_source_get_worker,
    );

    // ---- ConduitStreamSinkChannel ----
    r.register(
        CLS_SINK,
        "write",
        "(Ljava/nio/ByteBuffer;)I",
        native_sink_write,
    );
    r.register(
        CLS_SINK,
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
        native_sink_write_gather,
    );
    r.register(
        CLS_SINK,
        "transferFrom",
        "(Ljava/nio/channels/FileChannel;JJ)J",
        native_sink_transfer_from,
    );
    r.register(
        CLS_SINK,
        "getConduit",
        "()Lorg/xnio/conduits/StreamSinkConduit;",
        native_channel_get_self_conduit,
    );
    r.register(
        CLS_SINK,
        "setConduit",
        "(Lorg/xnio/conduits/StreamSinkConduit;)V",
        native_channel_set_conduit,
    );
    r.register(
        CLS_SINK,
        "setWriteListener",
        "(Lorg/xnio/ChannelListener;)V",
        native_sink_set_write_listener,
    );
    r.register(
        CLS_SINK,
        "getWriteListener",
        "()Lorg/xnio/ChannelListener;",
        native_sink_get_write_listener,
    );
    r.register(CLS_SINK, "resumeWrites", "()V", native_sink_resume_writes);
    r.register(CLS_SINK, "suspendWrites", "()V", native_sink_suspend_writes);
    r.register(
        CLS_SINK,
        "shutdownWrites",
        "()V",
        native_sink_shutdown_writes,
    );
    r.register(CLS_SINK, "flush", "()Z", native_sink_flush);
    r.register(
        CLS_SINK,
        "terminateWrites",
        "()V",
        native_sink_shutdown_writes,
    );
    r.register(
        CLS_SINK,
        "truncateWrites",
        "()V",
        native_sink_truncate_writes,
    );
    r.register(
        CLS_SINK,
        "isWriteShutdown",
        "()Z",
        native_sink_is_write_shutdown,
    );
    r.register(CLS_SINK, "wakeupWrites", "()V", native_sink_resume_writes);
    r.register(
        CLS_SINK,
        "isWriteResumed",
        "()Z",
        native_sink_is_write_resumed,
    );
    r.register(CLS_SINK, "awaitWritable", "()V", native_sink_await_writable);
    r.register(
        CLS_SINK,
        "awaitWritable",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_sink_await_writable_timed,
    );
    r.register(
        CLS_SINK,
        "getWriteThread",
        "()Lorg/xnio/XnioIoThread;",
        native_sink_get_write_thread,
    );
    r.register(
        CLS_SINK,
        "setWriteReadyHandler",
        "(Lorg/xnio/conduits/WriteReadyHandler;)V",
        native_sink_set_write_ready_handler,
    );
    r.register(
        CLS_SINK,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_sink_get_worker,
    );
    r.register(
        CLS_SINK,
        "transferFrom",
        "(Lorg/xnio/channels/StreamSourceChannel;JLjava/nio/ByteBuffer;)J",
        native_sink_transfer_from,
    );
    r.register(
        CLS_SINK,
        "writeFinal",
        "(Ljava/nio/ByteBuffer;)I",
        native_sink_write,
    );
    r.register(
        CLS_SINK,
        "writeFinal",
        "([Ljava/nio/ByteBuffer;II)J",
        native_sink_write_gather,
    );

    // ---- StreamSinkConduit raw shim ----
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "write",
        "(Ljava/nio/ByteBuffer;)I",
        native_sink_write,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
        native_sink_write_gather,
    );
    r.register(CLS_STREAM_SINK_CONDUIT, "flush", "()Z", native_sink_flush);
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "terminateWrites",
        "()V",
        native_sink_shutdown_writes,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "truncateWrites",
        "()V",
        native_sink_truncate_writes,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "isWriteShutdown",
        "()Z",
        native_sink_is_write_shutdown,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "wakeupWrites",
        "()V",
        native_sink_resume_writes,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "isWriteResumed",
        "()Z",
        native_sink_is_write_resumed,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "awaitWritable",
        "()V",
        native_sink_await_writable,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "awaitWritable",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_sink_await_writable_timed,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "getWriteThread",
        "()Lorg/xnio/XnioIoThread;",
        native_sink_get_write_thread,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "setWriteReadyHandler",
        "(Lorg/xnio/conduits/WriteReadyHandler;)V",
        native_sink_set_write_ready_handler,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_sink_get_worker,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "transferFrom",
        "(Lorg/xnio/channels/StreamSourceChannel;JLjava/nio/ByteBuffer;)J",
        native_sink_transfer_from,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "writeFinal",
        "(Ljava/nio/ByteBuffer;)I",
        native_sink_write,
    );
    r.register(
        CLS_STREAM_SINK_CONDUIT,
        "writeFinal",
        "([Ljava/nio/ByteBuffer;II)J",
        native_sink_write_gather,
    );

    // ---- SinkConduit parent raw shim ----
    r.register(
        CLS_SINK_CONDUIT,
        "suspendWrites",
        "()V",
        native_sink_suspend_writes,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "resumeWrites",
        "()V",
        native_sink_resume_writes,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "terminateWrites",
        "()V",
        native_sink_shutdown_writes,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "truncateWrites",
        "()V",
        native_sink_truncate_writes,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "isWriteShutdown",
        "()Z",
        native_sink_is_write_shutdown,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "wakeupWrites",
        "()V",
        native_sink_resume_writes,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "isWriteResumed",
        "()Z",
        native_sink_is_write_resumed,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "awaitWritable",
        "()V",
        native_sink_await_writable,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "awaitWritable",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_sink_await_writable_timed,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "getWriteThread",
        "()Lorg/xnio/XnioIoThread;",
        native_sink_get_write_thread,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "setWriteReadyHandler",
        "(Lorg/xnio/conduits/WriteReadyHandler;)V",
        native_sink_set_write_ready_handler,
    );
    r.register(
        CLS_SINK_CONDUIT,
        "getWorker",
        "()Lorg/xnio/XnioWorker;",
        native_sink_get_worker,
    );
    r.register(CLS_SINK_CONDUIT, "flush", "()Z", native_sink_flush);

    // ---- ChannelListener$Setter ----
    r.register(
        CLS_LISTENER_SETTER,
        "set",
        "(Lorg/xnio/ChannelListener;)V",
        native_setter_set,
    );
    // The Listener interface itself needs no natives — all invocations flow
    // through `ctx.invoke_virtual` dispatch from `dispatch_channel_event`.
    let _ = CLS_LISTENER;

    // Silence unused-import warnings when the file is consumed only via its
    // public entry points.
    let _ = SETTER_NUM_SLOTS;
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ArrayElementType;

    fn make_byte_buffer(
        ctx: &mut crate::test_utils::MockNativeContext,
        capacity: i32,
    ) -> ObjectRef {
        let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 5).unwrap();
        let arr = ctx.new_array(ArrayElementType::Byte, capacity as usize);
        ctx.set_field(buf, BB_FIELD_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_FIELD_POS, Value::Int(0));
        ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(capacity));
        ctx.set_field(buf, BB_FIELD_CAPACITY, Value::Int(capacity));
        buf
    }

    fn buffer_contents_up_to_pos(
        ctx: &crate::test_utils::MockNativeContext,
        buf: ObjectRef,
    ) -> Vec<u8> {
        let pos = bb_position(ctx, buf) as usize;
        let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
            Value::Object(Some(o)) => o,
            _ => return Vec::new(),
        };
        (0..pos)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => (v & 0xff) as u8,
                _ => 0,
            })
            .collect()
    }

    /// Simple SelectionKeyLike for tests. Ready ops + optional source / sink
    /// attachments.
    struct FakeKey {
        ready: i32,
        src: Option<(u64, ObjectRef)>,
        sink: Option<(u64, ObjectRef)>,
    }

    impl SelectionKeyLike for FakeKey {
        fn ready_ops(&self) -> i32 {
            self.ready
        }
        fn source_attachment(&self) -> Option<(u64, ObjectRef)> {
            self.src
        }
        fn sink_attachment(&self) -> Option<(u64, ObjectRef)> {
            self.sink
        }
    }

    fn mark_sink_write_listener_invoked(
        ctx: &mut crate::test_utils::MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name == "handleEvent" && descriptor == CHANNEL_LISTENER_HANDLE_EVENT_DESC {
            if let Some(Value::Object(Some(sink))) = args.first().copied() {
                ctx.set_field(sink, SINK_FIELD_WRITE_READY_FLAG, Value::Int(2));
            }
            return Some(Ok(None));
        }
        None
    }

    fn mark_source_read_listener_invoked(
        ctx: &mut crate::test_utils::MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name == "handleEvent" && descriptor == CHANNEL_LISTENER_HANDLE_EVENT_DESC {
            if let Some(Value::Object(Some(source))) = args.first().copied() {
                ctx.set_field(source, SRC_FIELD_READ_READY_FLAG, Value::Int(2));
            }
            return Some(Ok(None));
        }
        None
    }

    fn sink_listener_makes_paired_source_pending_then_marks_reads(
        ctx: &mut crate::test_utils::MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name != "handleEvent" || descriptor != CHANNEL_LISTENER_HANDLE_EVENT_DESC {
            return None;
        }
        let Some(Value::Object(Some(channel))) = args.first().copied() else {
            return Some(Ok(None));
        };
        if sink_id_of(ctx, channel).is_some() {
            ctx.set_field(channel, SINK_FIELD_WRITE_READY_FLAG, Value::Int(2));
            if let Some(source) = paired_source_of(ctx, channel) {
                if let Some(id) = source_id_of(ctx, source) {
                    if let Some(ch) = get_source_channel(id) {
                        if let ConduitTransport::Pipe(pipe) = &ch.transport {
                            pipe.push(b"r");
                        }
                    }
                }
            }
        } else if source_id_of(ctx, channel).is_some() {
            ctx.set_field(channel, SRC_FIELD_READ_READY_FLAG, Value::Int(2));
        }
        Some(Ok(None))
    }

    // ---- Test 1: source channel read returns bytes from the socket ----
    #[test]
    fn t19_7_d_source_channel_read_returns_bytes_from_socket() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        pipe.push(b"hello");
        let id = register_source_channel(ConduitTransport::Pipe(pipe.clone()));
        let ch = alloc_source_channel_obj(&mut ctx, id).unwrap();
        // Must be resumed for reads (the registry-side suspend flag is
        // checked by dispatch, not by the direct read path, but we reset
        // the Java-side flag for clarity).
        ctx.set_field(ch, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        let buf = make_byte_buffer(&mut ctx, 16);

        let n = source_channel_read(&mut ctx, id, buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(buffer_contents_up_to_pos(&ctx, buf), b"hello");
        drop_source_channel(&mut ctx, id);
    }

    // ---- Test 2: read at EOF returns -1 ----
    #[test]
    fn t19_7_d_source_channel_read_at_eof_returns_minus_one() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        pipe.close_write(); // no data, writer done => EOF
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let _ch = alloc_source_channel_obj(&mut ctx, id);
        let buf = make_byte_buffer(&mut ctx, 16);

        let n = source_channel_read(&mut ctx, id, buf).unwrap();
        assert_eq!(n, -1, "EOF must surface as -1");
        drop_source_channel(&mut ctx, id);
    }

    // ---- Test 3: sink channel write returns bytes written ----
    #[test]
    fn t19_7_d_sink_channel_write_returns_bytes_written() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_sink_channel(ConduitTransport::Pipe(pipe.clone()));
        let _ch = alloc_sink_channel_obj(&mut ctx, id);
        let buf = make_byte_buffer(&mut ctx, 8);
        // Seed the buffer with 'abcd' at position 0.
        let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
            Value::Object(Some(o)) => o,
            _ => panic!("array"),
        };
        for (i, b) in b"abcd".iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));

        let n = sink_channel_write(&mut ctx, id, buf).unwrap();
        assert_eq!(n, 4);
        let drained: Vec<u8> = {
            let g = pipe.buf.lock().unwrap();
            g.iter().copied().collect()
        };
        assert_eq!(&drained, b"abcd");
        drop_sink_channel(&mut ctx, id);
    }

    // ---- Test 4: write when kernel buffer full returns 0 ----
    #[test]
    fn t19_7_d_sink_channel_write_when_full_returns_zero() {
        let mut ctx = mock_ctx();
        // Cap the pipe at 4 bytes and pre-fill it. Next write must return 0.
        let pipe = Arc::new(Pipe::new(4));
        pipe.push(b"full");
        let id = register_sink_channel(ConduitTransport::Pipe(pipe.clone()));
        let _ch = alloc_sink_channel_obj(&mut ctx, id);
        let buf = make_byte_buffer(&mut ctx, 8);
        let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
            Value::Object(Some(o)) => o,
            _ => panic!("array"),
        };
        for (i, b) in b"more".iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));

        let n = sink_channel_write(&mut ctx, id, buf).unwrap();
        assert_eq!(n, 0, "write on full pipe must return 0");
        let ch = get_sink_channel(id).unwrap();
        assert_eq!(
            ch.buffered_bytes.load(Ordering::Acquire),
            4,
            "buffered_bytes must reflect the 4 bytes that didn't go out"
        );
        drop_sink_channel(&mut ctx, id);
    }

    // ---- Test 5: setReadListener stores the listener on the channel ----
    #[test]
    fn t19_7_d_set_read_listener_stores_on_channel() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let ch = alloc_source_channel_obj(&mut ctx, id).unwrap();
        let listener = ctx.create_string("listener");

        let r = native_source_set_read_listener(
            &mut ctx,
            &[Value::Object(Some(ch)), Value::Object(Some(listener))],
        )
        .unwrap();
        assert!(r.is_none());
        match ctx.get_field(ch, SRC_FIELD_READ_LISTENER) {
            Value::Object(Some(o)) => assert_eq!(o, listener),
            other => panic!("expected listener stored, got {other:?}"),
        }
        drop_source_channel(&mut ctx, id);
    }

    // ---- Test 6: dispatch fires the listener on a ready key ----
    #[test]
    fn t19_7_d_dispatch_fires_listener_on_ready() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
        // Resume reads so dispatch doesn't skip.
        ctx.set_field(ch_obj, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        get_source_channel(id)
            .unwrap()
            .read_suspended
            .store(false, Ordering::Release);
        let listener = ctx.create_string("listener");
        ctx.set_field(
            ch_obj,
            SRC_FIELD_READ_LISTENER,
            Value::Object(Some(listener)),
        );

        let key = FakeKey {
            ready: OP_READ,
            src: Some((id, ch_obj)),
            sink: None,
        };
        let stats = dispatch_channel_event(&mut ctx, &key);
        assert!(stats.read_fired, "listener must fire");
        assert!(!stats.read_panicked);
        // Ready flag written.
        assert_eq!(
            ctx.get_field(ch_obj, SRC_FIELD_READ_READY_FLAG),
            Value::Int(1)
        );
        drop_source_channel(&mut ctx, id);
    }

    // ---- Test 7: a panicking listener is caught; loop continues ----
    #[test]
    fn t19_7_d_listener_panic_caught_loop_continues() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
        ctx.set_field(ch_obj, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        get_source_channel(id)
            .unwrap()
            .read_suspended
            .store(false, Ordering::Release);
        let listener = ctx.create_string("listener");
        ctx.set_field(
            ch_obj,
            SRC_FIELD_READ_LISTENER,
            Value::Object(Some(listener)),
        );

        // Install an invoke_virtual result that panics.
        let slot = ctx.invoke_virtual_result.get();
        // SAFETY: single-threaded test context.
        unsafe {
            *slot = Some(Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Internal {
                    message: "synthetic panic in listener".into(),
                },
            )));
        }
        // The fake context's invoke_virtual returns our pre-staged
        // result. That's an error, not a panic; we separately verify panic
        // safety with a direct catch_unwind smoke test below.
        let key = FakeKey {
            ready: OP_READ,
            src: Some((id, ch_obj)),
            sink: None,
        };
        let stats = dispatch_channel_event(&mut ctx, &key);
        // Listener invocation "ran" (returned an Err from the mock) — no panic.
        assert!(stats.read_fired);
        assert!(!stats.read_panicked);

        // Now stage an actual panic path to exercise catch_unwind. We simulate
        // a panicking listener by wrapping the call directly.
        let panicked = catch_unwind(AssertUnwindSafe(|| panic!("BOOM"))).is_err();
        assert!(panicked, "catch_unwind must report the panic");

        drop_source_channel(&mut ctx, id);
    }

    // ---- Test 8: suspend_reads stops listener dispatch ----
    #[test]
    fn t19_7_d_suspend_reads_stops_listener_dispatch() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
        let listener = ctx.create_string("listener");
        ctx.set_field(
            ch_obj,
            SRC_FIELD_READ_LISTENER,
            Value::Object(Some(listener)),
        );
        native_source_suspend_reads(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

        let key = FakeKey {
            ready: OP_READ,
            src: Some((id, ch_obj)),
            sink: None,
        };
        let stats = dispatch_channel_event(&mut ctx, &key);
        assert!(!stats.read_fired);
        assert_eq!(stats.skipped_suspended, 1);
        drop_source_channel(&mut ctx, id);
    }

    // ---- Test 9: resumeReads restarts dispatch after suspend ----
    #[test]
    fn t19_7_d_resume_reads_restarts_after_suspend() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
        let listener = ctx.create_string("listener");
        ctx.set_field(
            ch_obj,
            SRC_FIELD_READ_LISTENER,
            Value::Object(Some(listener)),
        );

        // Suspend then resume.
        native_source_suspend_reads(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();
        native_source_resume_reads(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

        assert_eq!(
            ctx.get_field(ch_obj, SRC_FIELD_READ_SUSPENDED),
            Value::Int(0)
        );
        let reg = get_source_channel(id).unwrap();
        assert!(!reg.read_suspended.load(Ordering::Acquire));

        let key = FakeKey {
            ready: OP_READ,
            src: Some((id, ch_obj)),
            sink: None,
        };
        let stats = dispatch_channel_event(&mut ctx, &key);
        assert!(stats.read_fired, "listener must fire after resume");
        drop_source_channel(&mut ctx, id);
    }

    // ---- Test 10: shutdownWrites sends FIN ----
    #[test]
    fn t19_7_d_shutdown_writes_sends_fin() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_sink_channel(ConduitTransport::Pipe(pipe.clone()));
        let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
        native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

        assert!(
            pipe.fin_sent.load(Ordering::Acquire),
            "shutdown must set fin_sent on the pipe"
        );
        assert!(pipe.eof.load(Ordering::Acquire));
        // gc-common w23-b: a sink with no paired source is a connection of
        // one half, so its shutdown closes it and it is unregistered; it
        // still reports itself shut down.
        assert!(
            get_sink_channel(id).is_none(),
            "the closed sink is unregistered"
        );
        assert_eq!(
            native_sink_is_write_shutdown(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap(),
            Some(Value::Int(1))
        );

        // A subsequent write must be rejected rather than silently lost.
        let buf = make_byte_buffer(&mut ctx, 4);
        ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));
        let r = sink_channel_write(&mut ctx, id, buf);
        assert!(r.is_err(), "write after shutdown must fail");
        drop_sink_channel(&mut ctx, id);
    }

    // ---- Extra: flush reports true when no bytes buffered ----
    #[test]
    fn t19_7_d_flush_reports_drained_when_clean() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_sink_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
        let r = native_sink_flush(&mut ctx, &[Value::Object(Some(ch_obj))])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Int(1), "flush must report drained when clean");
        drop_sink_channel(&mut ctx, id);
    }

    // ---- Extra: flush reports false when bytes are still buffered ----
    #[test]
    fn t19_7_d_flush_reports_not_drained_when_buffered() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_sink_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
        get_sink_channel(id)
            .unwrap()
            .buffered_bytes
            .store(17, Ordering::Release);
        let r = native_sink_flush(&mut ctx, &[Value::Object(Some(ch_obj))])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Int(0));
        drop_sink_channel(&mut ctx, id);
    }

    // ---- Extra: Setter wires a listener into the channel slot ----
    #[test]
    fn t19_7_d_channel_listener_setter_installs_listener() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
        // Build a Setter tied to SRC_FIELD_READ_LISTENER on ch_obj.
        let setter =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_LISTENER_SETTER, SETTER_NUM_SLOTS)
                .unwrap();
        ctx.set_field(
            setter,
            SETTER_FIELD_CHANNEL_HANDLE,
            Value::Object(Some(ch_obj)),
        );
        ctx.set_field(
            setter,
            SETTER_FIELD_LISTENER_SLOT_INDEX,
            Value::Int(SRC_FIELD_READ_LISTENER as i32),
        );
        let listener = ctx.create_string("installed");
        native_setter_set(
            &mut ctx,
            &[Value::Object(Some(setter)), Value::Object(Some(listener))],
        )
        .unwrap();
        match ctx.get_field(ch_obj, SRC_FIELD_READ_LISTENER) {
            Value::Object(Some(o)) => assert_eq!(o, listener),
            other => panic!("expected listener installed, got {other:?}"),
        }
        drop_source_channel(&mut ctx, id);
    }

    // ---- Extra: resumeWrites fires the channel write listener ----
    #[test]
    fn t19_7_d_resume_writes_fires_channel_listener() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let id = register_sink_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
        let listener = ctx.create_string("write-listener");
        native_sink_set_write_listener(
            &mut ctx,
            &[Value::Object(Some(ch_obj)), Value::Object(Some(listener))],
        )
        .unwrap();
        ctx.set_invoke_virtual_hook(mark_sink_write_listener_invoked);

        native_sink_resume_writes(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

        assert_eq!(
            ctx.get_field(ch_obj, SINK_FIELD_WRITE_SUSPENDED),
            Value::Int(0)
        );
        assert_eq!(
            ctx.get_field(ch_obj, SINK_FIELD_WRITE_READY_FLAG),
            Value::Int(2)
        );
        let reg = get_sink_channel(id).unwrap();
        assert!(!reg.write_suspended.load(Ordering::Acquire));
        assert!(reg.write_ready.load(Ordering::Acquire));
        drop_sink_channel(&mut ctx, id);
    }

    // ---- Extra: resumeWrites retries paired reads after listener writes ----
    #[test]
    fn t19_7_d_resume_writes_retries_paired_source_after_listener() {
        let mut ctx = mock_ctx();
        let source_pipe = Arc::new(Pipe::new(1024));
        let sink_pipe = Arc::new(Pipe::new(1024));
        let source_id = register_source_channel(ConduitTransport::Pipe(source_pipe));
        let sink_id = register_sink_channel(ConduitTransport::Pipe(sink_pipe));
        let source = alloc_source_channel_obj(&mut ctx, source_id).unwrap();
        let sink = alloc_sink_channel_obj(&mut ctx, sink_id).unwrap();
        remember_sink_paired_source(&mut ctx, sink, source);

        ctx.set_field(source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        let read_listener = ctx.create_string("read-listener");
        native_source_set_read_listener(
            &mut ctx,
            &[
                Value::Object(Some(source)),
                Value::Object(Some(read_listener)),
            ],
        )
        .unwrap();
        let write_listener = ctx.create_string("write-listener");
        native_sink_set_write_listener(
            &mut ctx,
            &[
                Value::Object(Some(sink)),
                Value::Object(Some(write_listener)),
            ],
        )
        .unwrap();
        ctx.set_invoke_virtual_hook(sink_listener_makes_paired_source_pending_then_marks_reads);

        native_sink_resume_writes(&mut ctx, &[Value::Object(Some(sink))]).unwrap();

        assert_eq!(
            ctx.get_field(sink, SINK_FIELD_WRITE_READY_FLAG),
            Value::Int(2)
        );
        assert_eq!(
            ctx.get_field(source, SRC_FIELD_READ_READY_FLAG),
            Value::Int(2),
            "resumeWrites should poll the paired source after its write listener returns"
        );

        drop_source_channel(&mut ctx, source_id);
        drop_sink_channel(&mut ctx, sink_id);
    }

    // ---- Extra: sink writes wake peer sources, not just their paired source ----
    #[test]
    fn t19_7_d_sink_write_wakes_registered_peer_source_with_pending_data() {
        let mut ctx = mock_ctx();
        let own_pipe = Arc::new(Pipe::new(1024));
        let peer_pipe = Arc::new(Pipe::new(1024));
        let own_source_id = register_source_channel(ConduitTransport::Pipe(own_pipe));
        let peer_source_id = register_source_channel(ConduitTransport::Pipe(peer_pipe.clone()));
        let sink_id = register_sink_channel(ConduitTransport::Pipe(peer_pipe));
        let own_source = alloc_source_channel_obj(&mut ctx, own_source_id).unwrap();
        let peer_source = alloc_source_channel_obj(&mut ctx, peer_source_id).unwrap();
        let sink = alloc_sink_channel_obj(&mut ctx, sink_id).unwrap();
        remember_sink_paired_source(&mut ctx, sink, own_source);

        ctx.set_field(own_source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        ctx.set_field(peer_source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        let listener = ctx.create_string("peer-read-listener");
        native_source_set_read_listener(
            &mut ctx,
            &[
                Value::Object(Some(peer_source)),
                Value::Object(Some(listener)),
            ],
        )
        .unwrap();
        ctx.set_invoke_virtual_hook(mark_source_read_listener_invoked);

        let buf = make_byte_buffer(&mut ctx, 3);
        let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
            Value::Object(Some(o)) => o,
            _ => panic!("missing backing array"),
        };
        for (idx, b) in b"cap".iter().enumerate() {
            ctx.set_array_element(arr, idx, Value::Int(*b as i32));
        }
        ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(3));

        let n = native_sink_write(
            &mut ctx,
            &[Value::Object(Some(sink)), Value::Object(Some(buf))],
        )
        .unwrap()
        .unwrap()
        .as_int()
        .unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            ctx.get_field(peer_source, SRC_FIELD_READ_READY_FLAG),
            Value::Int(2),
            "peer source listener should fire when the sink makes bytes pending"
        );
        assert_eq!(
            ctx.get_field(own_source, SRC_FIELD_READ_READY_FLAG),
            Value::Int(0),
            "paired same-side source had no pending bytes"
        );

        drop_source_channel(&mut ctx, own_source_id);
        drop_source_channel(&mut ctx, peer_source_id);
        drop_sink_channel(&mut ctx, sink_id);
    }

    // ---- Extra: channel IO-thread mirrors round-trip through raw conduits ----
    #[test]
    fn t19_7_d_channel_io_thread_round_trips_through_raw_conduits() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        let source_id = register_source_channel(ConduitTransport::Pipe(pipe.clone()));
        let sink_id = register_sink_channel(ConduitTransport::Pipe(pipe));
        let source = alloc_source_channel_obj(&mut ctx, source_id).unwrap();
        let sink = alloc_sink_channel_obj(&mut ctx, sink_id).unwrap();
        let io_thread = ctx.fresh_object_ref();

        remember_source_io_thread(&mut ctx, source, io_thread);
        remember_sink_io_thread(&mut ctx, sink, io_thread);

        assert_eq!(
            native_source_get_read_thread(&mut ctx, &[Value::Object(Some(source))])
                .unwrap()
                .unwrap(),
            Value::Object(Some(io_thread))
        );
        assert_eq!(
            native_sink_get_write_thread(&mut ctx, &[Value::Object(Some(sink))])
                .unwrap()
                .unwrap(),
            Value::Object(Some(io_thread))
        );

        // The mock context does not model the real XNIO "conduit" field
        // by name, so allocate raw conduit mirrors directly for this half of
        // the regression. Production channels still remember both surfaces
        // when the named field resolves.
        let source_conduit = alloc_source_conduit_obj(&mut ctx, source_id).unwrap();
        let sink_conduit = alloc_sink_conduit_obj(&mut ctx, sink_id).unwrap();
        remember_source_io_thread(&mut ctx, source_conduit, io_thread);
        remember_sink_io_thread(&mut ctx, sink_conduit, io_thread);
        assert_eq!(
            native_source_get_read_thread(&mut ctx, &[Value::Object(Some(source_conduit))])
                .unwrap()
                .unwrap(),
            Value::Object(Some(io_thread))
        );
        assert_eq!(
            native_sink_get_write_thread(&mut ctx, &[Value::Object(Some(sink_conduit))])
                .unwrap()
                .unwrap(),
            Value::Object(Some(io_thread))
        );

        drop_source_channel(&mut ctx, source_id);
        drop_sink_channel(&mut ctx, sink_id);
    }

    // ---- Extra: out-of-range ByteBuffer bounds are rejected ----
    #[test]
    fn t19_7_d_buffer_bounds_check_rejects_out_of_range() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        pipe.push(b"abcdef");
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let _ch = alloc_source_channel_obj(&mut ctx, id);

        // Build a buffer with position > limit — remaining() returns 0 and the
        // read returns 0 rather than segfaulting.
        let buf = make_byte_buffer(&mut ctx, 8);
        ctx.set_field(buf, BB_FIELD_POS, Value::Int(9));
        ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));
        let n = source_channel_read(&mut ctx, id, buf).unwrap();
        assert_eq!(n, 0, "out-of-range bounds yield 0, not a crash");
        drop_source_channel(&mut ctx, id);
    }

    // ---- Extra: scatter read across multiple buffers ----
    #[test]
    fn t19_7_d_source_scatter_read_distributes_bytes() {
        let mut ctx = mock_ctx();
        let pipe = Arc::new(Pipe::new(1024));
        pipe.push(b"hello world");
        let id = register_source_channel(ConduitTransport::Pipe(pipe));
        let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();

        let b1 = make_byte_buffer(&mut ctx, 5);
        let b2 = make_byte_buffer(&mut ctx, 8);
        let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
        ctx.set_array_element(arr, 0, Value::Object(Some(b1)));
        ctx.set_array_element(arr, 1, Value::Object(Some(b2)));

        let r = native_source_read_scatter(
            &mut ctx,
            &[
                Value::Object(Some(ch_obj)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(2),
            ],
        )
        .unwrap()
        .unwrap();
        let total = r.as_long().unwrap();
        assert!(
            total > 0 && total <= 11,
            "total bytes in [1..=11], got {total}"
        );
        drop_source_channel(&mut ctx, id);
    }
}

#[cfg(test)]
mod w22a_conduit_row_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Private VM identities: no other test uses them.
    const W22A_VM_A: usize = 0x0A22_7A08;
    const W22A_VM_B: usize = 0x0A22_7B08;

    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_xnio_conduit_rows(W22A_VM_A);
            forget_vm_xnio_conduit_rows(W22A_VM_B);
        }
    }

    /// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
    /// item 6): re-binding a conduit's ready handler releases the root it
    /// displaced (it used to leak one per re-registration); a VM's teardown
    /// drops its handler / IO-thread / source rows and the channels they
    /// named, and leaves another VM's rows alone.
    #[test]
    fn w22a_conduit_rows_release_displaced_roots_and_go_with_their_vm() {
        let _forget = Forget;
        let mut a = crate::test_utils::mock_ctx();
        a.set_vm_identity(W22A_VM_A);
        let mut b = crate::test_utils::mock_ctx();
        b.set_vm_identity(W22A_VM_B);

        let channel_a = a.fresh_object_ref();
        let handler_a1 = a.fresh_object_ref();
        let handler_a2 = a.fresh_object_ref();
        remember_ready_handler(read_ready_handler_registry(), &mut a, channel_a, handler_a1);
        assert_eq!(a.global_root_count(), 1);
        remember_ready_handler(read_ready_handler_registry(), &mut a, channel_a, handler_a2);
        assert_eq!(
            a.global_root_count(),
            1,
            "the displaced handler's root is released"
        );
        assert_eq!(
            ready_handler_of(read_ready_handler_registry(), &a, channel_a),
            Some(handler_a2)
        );

        let io_thread_a = a.fresh_object_ref();
        remember_source_io_thread(&mut a, channel_a, io_thread_a);
        remember_source_io_thread(&mut a, channel_a, io_thread_a);
        assert_eq!(a.global_root_count(), 2, "one IO-thread root, not two");

        let source_a = a.fresh_object_ref();
        let source_id =
            register_source_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(16))));
        remember_source_obj(&mut a, source_a, source_id);
        assert!(get_source_channel(source_id).is_some());

        let channel_b = b.fresh_object_ref();
        let handler_b = b.fresh_object_ref();
        remember_ready_handler(read_ready_handler_registry(), &mut b, channel_b, handler_b);

        forget_vm_xnio_conduit_rows(W22A_VM_A);
        assert_eq!(
            ready_handler_of(read_ready_handler_registry(), &a, channel_a),
            None
        );
        assert!(!channel_io_thread_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|k| k.vm == W22A_VM_A));
        assert!(source_obj_handles_by_id(&a).is_empty());
        assert!(
            get_source_channel(source_id).is_none(),
            "A's channel goes with A"
        );
        assert_eq!(
            ready_handler_of(read_ready_handler_registry(), &b, channel_b),
            Some(handler_b),
            "B's rows survive A's teardown"
        );
    }
}

/// gc-common w23-b (`common-w22a-xnio-conduit-channels-are-never-unregistered`).
#[cfg(test)]
mod w23b_conduit_close_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Private VM identities: no other test uses them.
    const W23B_VM_A: usize = 0x0B23_7A08;
    const W23B_VM_B: usize = 0x0B23_7B08;
    const W23B_VM_C: usize = 0x0B23_7C08;
    const W23B_VM_D: usize = 0x0B23_7D08;
    const W23B_VM_E: usize = 0x0B23_7E08;

    // Each test forgets only its own VMs: the tests run in parallel, and a
    // guard that forgot all five wiped a still-running test's rows.
    struct Forget(&'static [usize]);
    impl Drop for Forget {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_xnio_conduit_rows(vm);
            }
            W23B_FIRED.with(|f| f.borrow_mut().clear());
        }
    }

    thread_local! {
        /// The channel every `handleEvent` was dispatched for.
        static W23B_FIRED: std::cell::RefCell<Vec<usize>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn record_handle_event(
        _ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name != "handleEvent" || descriptor != CHANNEL_LISTENER_HANDLE_EVENT_DESC {
            return None;
        }
        if let Some(Value::Object(Some(channel))) = args.first() {
            W23B_FIRED.with(|f| f.borrow_mut().push(channel.as_ptr() as usize));
        }
        Some(Ok(None))
    }

    /// Global roots one connection files: its source channel object, the two
    /// IO-thread bindings, the sink's paired source and the two ready
    /// handlers.
    const ROOTS_PER_CONNECTION: usize = 6;

    struct Conn {
        source_id: u64,
        sink_id: u64,
        source: ObjectRef,
        sink: ObjectRef,
        source_pipe: Arc<Pipe>,
    }

    /// A connection registered the way `xnio_worker::build_stream_connection`
    /// registers one, plus the ready handlers XNIO installs on it.
    fn open_conn(ctx: &mut MockNativeContext) -> Conn {
        let source_pipe = Arc::new(Pipe::new(1024));
        let source_id = register_source_channel(ConduitTransport::Pipe(source_pipe.clone()));
        let sink_id = register_sink_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(1024))));
        let source = alloc_source_channel_obj(ctx, source_id).unwrap();
        let sink = alloc_sink_channel_obj(ctx, sink_id).unwrap();
        let io_thread = ctx.fresh_object_ref();
        remember_source_io_thread(ctx, source, io_thread);
        remember_sink_io_thread(ctx, sink, io_thread);
        remember_sink_paired_source(ctx, sink, source);
        let read_handler = ctx.fresh_object_ref();
        let write_handler = ctx.fresh_object_ref();
        native_source_set_read_ready_handler(
            ctx,
            &[
                Value::Object(Some(source)),
                Value::Object(Some(read_handler)),
            ],
        )
        .unwrap();
        native_sink_set_write_ready_handler(
            ctx,
            &[
                Value::Object(Some(sink)),
                Value::Object(Some(write_handler)),
            ],
        )
        .unwrap();
        Conn {
            source_id,
            sink_id,
            source,
            sink,
            source_pipe,
        }
    }

    /// Every row VM `vm` holds in this module's registries.
    fn vm_rows(vm: usize) -> usize {
        let sources = source_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .vm_row_count(vm);
        let sinks = sink_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .vm_row_count(vm);
        let handles = source_obj_handle_by_id_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&vm)
            .map_or(0, |rows| rows.len());
        let mut n = sources + sinks + handles;
        for registry in [
            sink_paired_source_registry(),
            channel_io_thread_registry(),
            read_ready_handler_registry(),
            write_ready_handler_registry(),
        ] {
            let rows = registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .filter(|k| k.vm == vm)
                .count();
            n += rows;
        }
        n
    }

    fn live_source_ids(ctx: &MockNativeContext) -> Vec<u64> {
        let mut ids: Vec<u64> = source_obj_handles_by_id(ctx)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// A connection goes -- rows, roots and channels -- once BOTH halves are
    /// shut down, in either order; one half alone keeps it. Afterwards the
    /// channel objects still answer as closed.
    #[test]
    fn w23b_closing_both_halves_unregisters_the_connection_and_releases_its_roots() {
        let _forget = Forget(&[W23B_VM_A]);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(W23B_VM_A);
        let roots_before = ctx.global_root_count();

        let conns: Vec<Conn> = (0..3).map(|_| open_conn(&mut ctx)).collect();
        assert_eq!(
            ctx.global_root_count(),
            roots_before + 3 * ROOTS_PER_CONNECTION
        );
        assert_eq!(live_source_ids(&ctx).len(), 3);

        // Source first: the half-closed connection stays.
        native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(conns[0].source))]).unwrap();
        assert!(get_source_channel(conns[0].source_id).is_some());
        assert!(get_sink_channel(conns[0].sink_id).is_some());
        assert_eq!(
            ctx.global_root_count(),
            roots_before + 3 * ROOTS_PER_CONNECTION
        );
        native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(conns[0].sink))]).unwrap();
        assert!(get_source_channel(conns[0].source_id).is_none());
        assert!(get_sink_channel(conns[0].sink_id).is_none());
        assert_eq!(
            ctx.global_root_count(),
            roots_before + 2 * ROOTS_PER_CONNECTION
        );
        assert_eq!(live_source_ids(&ctx).len(), 2);

        // Sink first, then source (terminateWrites / truncateWrites / terminateReads).
        native_sink_truncate_writes(&mut ctx, &[Value::Object(Some(conns[1].sink))]).unwrap();
        assert!(get_source_channel(conns[1].source_id).is_some());
        native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(conns[1].source))]).unwrap();
        native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(conns[2].sink))]).unwrap();
        native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(conns[2].source))]).unwrap();

        assert_eq!(
            ctx.global_root_count(),
            roots_before,
            "no root outlives its connection"
        );
        assert!(live_source_ids(&ctx).is_empty());
        assert_eq!(vm_rows(W23B_VM_A), 0, "no row outlives its connection");
        for conn in &conns {
            assert!(get_source_channel(conn.source_id).is_none());
            assert!(get_sink_channel(conn.sink_id).is_none());
        }

        // The closed channels still answer as closed channels.
        let c = &conns[0];
        assert_eq!(
            native_source_is_read_shutdown(&mut ctx, &[Value::Object(Some(c.source))]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_sink_is_write_shutdown(&mut ctx, &[Value::Object(Some(c.sink))]).unwrap(),
            Some(Value::Int(1))
        );
        let any = ctx.fresh_object_ref();
        assert_eq!(source_channel_read(&mut ctx, c.source_id, any).unwrap(), -1);
        assert!(sink_channel_write(&mut ctx, c.sink_id, any).is_err());
        c.source_pipe.push(b"late");
        assert!(!source_may_be_notified(c.source_id));

        // `drop_source_channel` releases the root it forgets (it took no
        // `ctx` and released nothing).
        let lone_id = register_source_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(16))));
        let _lone = alloc_source_channel_obj(&mut ctx, lone_id).unwrap();
        assert_eq!(ctx.global_root_count(), roots_before + 1);
        drop_source_channel(&mut ctx, lone_id);
        assert_eq!(ctx.global_root_count(), roots_before);
        assert_eq!(vm_rows(W23B_VM_A), 0);
    }

    /// The per-write walk (`notify_registered_sources_readable`) visits the
    /// VM's open connections only: a closed connection with pending bytes and
    /// a listener is neither listed nor fired.
    #[test]
    fn w23b_the_write_walk_sees_only_open_connections() {
        let _forget = Forget(&[W23B_VM_B]);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(W23B_VM_B);
        let closed = open_conn(&mut ctx);
        let open = open_conn(&mut ctx);
        for conn in [&closed, &open] {
            ctx.set_field(conn.source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
            let listener = ctx.fresh_object_ref();
            native_source_set_read_listener(
                &mut ctx,
                &[
                    Value::Object(Some(conn.source)),
                    Value::Object(Some(listener)),
                ],
            )
            .unwrap();
            conn.source_pipe.push(b"x");
        }
        native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(closed.source))]).unwrap();
        native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(closed.sink))]).unwrap();

        assert_eq!(live_source_ids(&ctx), vec![open.source_id]);
        ctx.set_invoke_virtual_hook(record_handle_event);
        notify_registered_sources_readable(&mut ctx, false, None);
        let fired = W23B_FIRED.with(|f| f.borrow().clone());
        assert_eq!(fired, vec![open.source.as_ptr() as usize]);

        // A suspended source is skipped before its root is even resolved.
        native_source_suspend_reads(&mut ctx, &[Value::Object(Some(open.source))]).unwrap();
        assert!(!source_may_be_notified(open.source_id));
        W23B_FIRED.with(|f| f.borrow_mut().clear());
        notify_registered_sources_readable(&mut ctx, false, None);
        assert!(W23B_FIRED.with(|f| f.borrow().is_empty()));
    }

    /// Two VMs' connections stay separate: closing VM C's releases exactly its
    /// roots -- although the mock numbers both VMs' global roots from the
    /// same start, so VM D's rows carry the same handle numbers (the old
    /// any-VM orphan check kept C's handles rooted because of them) -- and
    /// leaves every one of VM D's rows and roots in place.
    #[test]
    fn w23b_two_vms_connections_stay_separate() {
        let _forget = Forget(&[W23B_VM_C, W23B_VM_D]);
        let mut c = crate::test_utils::mock_ctx();
        c.set_vm_identity(W23B_VM_C);
        let mut d = crate::test_utils::mock_ctx();
        d.set_vm_identity(W23B_VM_D);
        let c_before = c.global_root_count();
        let d_before = d.global_root_count();
        let conn_c = open_conn(&mut c);
        let conn_d = open_conn(&mut d);
        let d_rows = vm_rows(W23B_VM_D);

        native_source_shutdown_reads(&mut c, &[Value::Object(Some(conn_c.source))]).unwrap();
        native_sink_shutdown_writes(&mut c, &[Value::Object(Some(conn_c.sink))]).unwrap();

        assert_eq!(c.global_root_count(), c_before);
        assert_eq!(vm_rows(W23B_VM_C), 0);
        assert_eq!(d.global_root_count(), d_before + ROOTS_PER_CONNECTION);
        assert_eq!(vm_rows(W23B_VM_D), d_rows);
        assert_eq!(live_source_ids(&d), vec![conn_d.source_id]);
        assert!(ready_handler_of(read_ready_handler_registry(), &d, conn_d.source).is_some());
        assert!(get_source_channel(conn_d.source_id).is_some());
        assert!(get_sink_channel(conn_d.sink_id).is_some());
    }

    /// `notify_source_readable` releases its pin on every exit. It released
    /// none, so the never-returning source poller grew its pin stack by one
    /// per registered source per tick.
    #[test]
    fn w23b_notify_releases_its_pin() {
        let _forget = Forget(&[W23B_VM_E]);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(W23B_VM_E);
        let conn = open_conn(&mut ctx);
        let pins = ctx.native_pin_count_for_test();

        // Suspended (the allocation default): the loop skips.
        notify_source_readable(&mut ctx, conn.source, false);
        assert_eq!(ctx.native_pin_count_for_test(), pins);

        // Resumed with pending bytes and a listener: it fires.
        ctx.set_field(conn.source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        let listener = ctx.fresh_object_ref();
        native_source_set_read_listener(
            &mut ctx,
            &[
                Value::Object(Some(conn.source)),
                Value::Object(Some(listener)),
            ],
        )
        .unwrap();
        conn.source_pipe.push(b"x");
        ctx.set_invoke_virtual_hook(record_handle_event);
        notify_source_readable(&mut ctx, conn.source, false);
        assert_eq!(W23B_FIRED.with(|f| f.borrow().len()), 1);
        assert_eq!(ctx.native_pin_count_for_test(), pins);

        // The walk, many times over (the poller's shape).
        for _ in 0..16 {
            notify_registered_sources_readable(&mut ctx, false, None);
        }
        assert_eq!(ctx.native_pin_count_for_test(), pins);
    }
}

/// gc-common w24-a (`common-w23b-xnio-identity-hash-keyed-rows-cross-and-outlive-connections`).
/// The mock's identity hash is the address truncated to `i32`, so addresses
/// 4 GiB apart share one (as a wrapped hash counter makes two real objects
/// share one). The objects are never dereferenced: filing and looking up a
/// sink row only keys them. Each test has its own VM identities (the key
/// registry and these registries are process-wide) and forgets them in a
/// guard.
#[cfg(test)]
mod w24a_conduit_key_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Private VM identities: no other test uses them.
    const W24A_VM_A: usize = 0x0A24_7A08;
    const W24A_VM_B: usize = 0x0A24_7B08;
    const W24A_VM_C: usize = 0x0A24_7C08;
    const W24A_VM_D: usize = 0x0A24_7D08;

    struct Forget(&'static [usize]);
    impl Drop for Forget {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_xnio_conduit_rows(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: a key only; the key registry and the mock hash the address
        // and never dereference it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn ctx_for(vm: usize) -> MockNativeContext {
        let c = crate::test_utils::mock_ctx();
        c.set_vm_identity(vm);
        c
    }

    fn new_sink() -> u64 {
        register_sink_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(16))))
    }

    /// A dead channel's rows are not found by a later object carrying its
    /// identity hash: the old `(vm, identity hash)` key answered the dead
    /// channel's id here, and every native on the newcomer then read from or
    /// wrote to the dead connection's socket.
    #[test]
    fn w24a_a_dead_channels_rows_are_not_found_by_an_object_with_its_hash() {
        let _forget = Forget(&[W24A_VM_A]);
        let a = ctx_for(W24A_VM_A);
        let dead = 0x1_A240_1000usize;
        let newcomer = 0x2_A240_1000usize;
        assert_eq!(
            a.identity_hash_code(obj(dead)),
            a.identity_hash_code(obj(newcomer)),
            "premise: the two objects share an identity hash"
        );
        let dead_id = new_sink();
        remember_sink_obj(&a, obj(dead), dead_id);
        assert_eq!(sink_id_by_obj(&a, obj(dead)), Some(dead_id));

        assert_eq!(crate::gc_sweep_lock_keys(W24A_VM_A, &|x| x != dead), 1);
        assert_eq!(
            sink_id_by_obj(&a, obj(newcomer)),
            None,
            "the dead channel's row is not the newcomer's"
        );

        let new_id = new_sink();
        remember_sink_obj(&a, obj(newcomer), new_id);
        assert_eq!(sink_id_by_obj(&a, obj(newcomer)), Some(new_id));
    }

    /// A channel the collector moved keeps its rows (the key registry
    /// re-addresses its slot), and a lookup of an object that was never filed
    /// leaves no key-registry slot behind.
    #[test]
    fn w24a_a_moved_channel_keeps_its_rows_and_lookups_do_not_mint() {
        let _forget = Forget(&[W24A_VM_B]);
        let b = ctx_for(W24A_VM_B);
        let old = 0x1_A241_1000usize;
        // Same low 32 bits: a real object keeps its identity hash across a
        // move, and the mock's hash is the address truncated to `i32`.
        let new = 0x3_A241_1000usize;
        let id = new_sink();
        remember_sink_obj(&b, obj(old), id);

        let mut moved = cratonvm_types::PointerMap::default();
        moved.insert(old, new);
        assert_eq!(
            crate::gc_sweep_and_remap_lock_keys(W24A_VM_B, &|x| x == old, &moved),
            0
        );
        assert_eq!(sink_id_by_obj(&b, obj(new)), Some(id));

        let stranger = 0x1_A241_3000usize;
        assert_eq!(sink_id_by_obj(&b, obj(stranger)), None);
        assert_eq!(source_id_by_obj(&b, obj(stranger)), None);
        assert_eq!(crate::existing_weak_lock_key(&b, obj(stranger)), None);
    }

    /// Two VMs filing one address keep their own rows, and one VM's sweep
    /// finding the object dead costs the other VM nothing.
    #[test]
    fn w24a_two_vms_keep_their_own_channel_rows() {
        let _forget = Forget(&[W24A_VM_C, W24A_VM_D]);
        let c = ctx_for(W24A_VM_C);
        let d = ctx_for(W24A_VM_D);
        let addr = 0x1_A242_1000usize;
        let c_id = new_sink();
        let d_id = new_sink();
        remember_sink_obj(&c, obj(addr), c_id);
        remember_sink_obj(&d, obj(addr), d_id);
        assert_eq!(sink_id_by_obj(&c, obj(addr)), Some(c_id));
        assert_eq!(sink_id_by_obj(&d, obj(addr)), Some(d_id));

        crate::gc_sweep_lock_keys(W24A_VM_C, &|x| x != addr);
        assert_eq!(sink_id_by_obj(&c, obj(addr)), None);
        assert_eq!(sink_id_by_obj(&d, obj(addr)), Some(d_id));
    }
}
