// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.8 — `WatchService` file-system notifications via real OS APIs.
//!
//! This module registers natives for the JDK's *internal* `sun/nio/fs/*`
//! `WatchService` implementation classes that JDK 25 uses in real-JDK
//! mode (i.e. when the synthetic-jdk overrides in `lib.rs::register_watch_service`
//! are NOT in effect).
//!
//! It is file-disjoint from the existing `register_watch_service` (which
//! targets the public Java-level `java/nio/file/WatchService`) — the public
//! API natives there back the synthetic-jdk fallback layout. The natives
//! registered here were WRITTEN AS IF they back the real JDK 25
//! `sun.nio.fs.*WatchService` pipeline. **They do not, and 2026-08-22 measured
//! it:** the `*0` method names below appear on no class in any JDK image (the
//! real `LinuxWatchService` natives are `inotifyInit` / `inotifyAddWatch` /
//! `socketpair` / `poll(int,int)` and friends), so every triple here is a
//! CratonVM-defined API wearing a `sun.nio.fs` name. Measured `invocations: 0`
//! in `--real-jdk` and absent from the `--jdk-only` registry entirely, with the
//! real `WatchService` delivering events on this VM regardless. See
//! `register_watch_service_real`'s doc comment for the full measurement.
//!
//! ## Backing strategy
//!
//! Each `WatchService` corresponds to one `notify::RecommendedWatcher`,
//! which dispatches to the platform-recommended OS API:
//!
//!   * Linux  → `inotify`
//!   * Windows→ `ReadDirectoryChangesW`
//!   * macOS  → `FSEvents` (or `kqueue` with the `macos_kqueue` feature
//!     enabled in this crate's `Cargo.toml`)
//!   * BSD    → `kqueue`
//!
//! No periodic-polling pretender path: when the platform watcher cannot
//! be created (e.g. `inotify` resource exhaustion, or running inside a
//! sandbox without the relevant syscall), we surface the underlying
//! `notify::Error` as a Java `IOException`. JDK 25's `PollingWatchService`
//! is itself a pure-Java fallback that the JDK installs when the
//! platform service is absent — we don't replicate it natively.
//!
//! ## Registry shape
//!
//! ```text
//!     i32 ws_id  ──→ WatcherState { watcher, rx, registered, key_ids, queue }
//!     i32 key_id ──→ WatchKeyState { ws_id, dir, kinds_mask, valid }
//! ```
//!
//! Both registries are `OnceLock<Mutex<HashMap<...>>>` — concurrent open
//! services don't block each other (the lock is held only briefly per
//! call). Event drain happens on the calling thread inside `take`/`poll`,
//! so we never hold the lock across the OS-side blocking call.
//!
//! ## Java-side natives we expose
//!
//! Most of the WatchService machinery is written in Java in JDK 25 — the
//! native footprint is small. We register a minimal cross-platform set:
//!
//!   * `sun/nio/fs/AbstractWatchService.<init>()` → no-op (Java side
//!     allocates state; we register a callable native to cover any path
//!     where the class file declares `<init>` as `native`).
//!   * `<WatchService impl>.poll0(long)`  → blocking poll with
//!     timeout in ns; returns a key id or 0.
//!   * `<WatchService impl>.take0()` → blocking take; returns a
//!     key id (never 0 unless the service was closed).
//!   * `<WatchService impl>.register0(String dir, int kinds)`
//!     → returns a key id (≥ 1).
//!   * `<WatchService impl>.cancel0(int keyId)`
//!   * `<WatchService impl>.close0()`
//!   * `<WatchService impl>.pollEvents0(int keyId)` →
//!     `int[]` packed as `[kind, name_id, kind, name_id, ...]` plus a
//!     companion `String[]` retrieval native `pollEventNames0(int keyId)`
//!     since returning a Java `List<WatchEvent>` from a native is
//!     awkward.
//!
//! The same set is also registered on the Windows + Polling +
//! Abstract subclasses so any subclass dispatch lands here regardless
//! of platform. The Java side picks the right subclass; the natives
//! are platform-agnostic.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use notify::{
    event::{CreateKind, EventKind, ModifyKind, RemoveKind},
    Config, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher,
};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, Value};

// ---------------------------------------------------------------------------
// Event kind bits — kept in sync with `lib.rs` constants so a Java-side
// caller that mixes both APIs sees consistent values.
// ---------------------------------------------------------------------------

const KIND_CREATE: i32 = 1;
const KIND_DELETE: i32 = 2;
const KIND_MODIFY: i32 = 4;
const KIND_OVERFLOW: i32 = 8;

// ---------------------------------------------------------------------------
// Shared state types
// ---------------------------------------------------------------------------

type NotifyResult = Result<notify::Event, notify::Error>;

/// Per-WatchService state.
struct WatcherState {
    /// Holds the platform watcher. Dropping this stops the OS-side
    /// notifications and hangs up the channel.
    watcher: RecommendedWatcher,
    /// Receiver for raw events from the watcher's worker thread.
    rx: mpsc::Receiver<NotifyResult>,
    /// `key_id → (dir_canonical, kinds_mask, valid_flag)`.
    keys: HashMap<i32, KeyState>,
    /// Pending signalled keys (FIFO) — populated whenever an event is
    /// drained that targets a registered watch path. Each entry is a
    /// `key_id`. The same `key_id` may sit in this queue at most once;
    /// on `reset()` it can be re-enqueued if more events arrive.
    signalled: VecDeque<i32>,
    /// Cached drained events per key_id — populated by `drain` and
    /// consumed by `pollEvents`. Each entry: `(kind_bit, basename)`.
    pending: HashMap<i32, Vec<(i32, String)>>,
    /// True until `close()` is called.
    open: bool,
}

struct KeyState {
    /// Canonicalized directory we registered with `notify`.
    dir: PathBuf,
    /// Bitmask of `KIND_*` bits the caller wants.
    kinds_mask: i32,
    /// True until `cancel()` is called or the service closed.
    valid: bool,
    /// True when this key currently has events pending and is in the
    /// `signalled` queue. Cleared on `reset()`.
    enqueued: bool,
}

fn watch_services() -> &'static Mutex<HashMap<i32, WatcherState>> {
    static REG: OnceLock<Mutex<HashMap<i32, WatcherState>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_ws_id() -> i32 {
    static N: AtomicI32 = AtomicI32::new(1);
    N.fetch_add(1, Ordering::SeqCst)
}

fn next_key_id() -> i32 {
    static N: AtomicI32 = AtomicI32::new(1);
    N.fetch_add(1, Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Public Rust API — exposed for tests and for any Java-mode caller that
// wants to drive the watcher without going through the bytecode dispatch
// layer.
// ---------------------------------------------------------------------------

/// Open a new platform watcher and register it. Returns the new ws_id, or
/// an `IOException` if `notify` cannot create a watcher (e.g. inotify
/// limit reached).
pub fn open_watch_service() -> Result<i32, MethodCallFailed> {
    let (tx, rx) = mpsc::channel::<NotifyResult>();
    let watcher = RecommendedWatcher::new(
        move |res: NotifyResult| {
            // Best-effort send. If the receiver was dropped (close),
            // we drop the event silently.
            let _ = tx.send(res);
        },
        Config::default(),
    )
    .map_err(|e| {
        MethodCallFailed::from(RuntimeError::IOException {
            message: format!("WatchService: platform watcher init failed: {e}"),
        })
    })?;
    let id = next_ws_id();
    watch_services().lock().insert(
        id,
        WatcherState {
            watcher,
            rx,
            keys: HashMap::new(),
            signalled: VecDeque::new(),
            pending: HashMap::new(),
            open: true,
        },
    );
    Ok(id)
}

/// Register a directory with the watcher and return a fresh key_id.
/// `kinds_mask` is a bitwise-or of `KIND_CREATE | KIND_DELETE | KIND_MODIFY`.
pub fn register_dir(ws_id: i32, dir: &str, kinds_mask: i32) -> Result<i32, MethodCallFailed> {
    // SECURITY (HIGH): run the crate-wide path validator BEFORE handing
    // the directory off to the OS-side notify watcher. Without this, a
    // guest under `set_path_confine_to_cwd(true)` could call
    // `WatchService.register("../../etc")` and receive file-system
    // change notifications for paths outside the sandbox root —
    // inotify / ReadDirectoryChangesW don't enforce our policy on
    // their own. The validator's `SecurityException` is translated to
    // `IOException` so the Java-visible failure matches what
    // `WatchService.register0` would throw for any other unwatchable
    // directory.
    let dir = match crate::validate_path(dir) {
        Ok(p) => p,
        Err(_) => {
            return Err(RuntimeError::IOException {
                message: format!("WatchService.register: path rejected by sandbox: {dir}"),
            }
            .into());
        }
    };
    let canonical = canonicalize_or_passthrough(&dir);
    if !canonical.exists() {
        return Err(RuntimeError::IOException {
            message: format!("WatchService.register: no such file or directory: {dir}"),
        }
        .into());
    }
    let mut svcs = watch_services().lock();
    let st = svcs.get_mut(&ws_id).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IOException {
            message: "WatchService.register: service is closed or unknown".into(),
        })
    })?;
    if !st.open {
        return Err(RuntimeError::IOException {
            message: "WatchService.register: service is closed".into(),
        }
        .into());
    }
    st.watcher
        .watch(&canonical, RecursiveMode::NonRecursive)
        .map_err(|e| {
            MethodCallFailed::from(RuntimeError::IOException {
                message: format!("WatchService.register: {e}"),
            })
        })?;
    let key_id = next_key_id();
    st.keys.insert(
        key_id,
        KeyState {
            dir: canonical,
            kinds_mask,
            valid: true,
            enqueued: false,
        },
    );
    Ok(key_id)
}

/// Mark a key cancelled. Idempotent.
pub fn cancel_key(ws_id: i32, key_id: i32) {
    let mut svcs = watch_services().lock();
    if let Some(st) = svcs.get_mut(&ws_id) {
        if let Some(k) = st.keys.get_mut(&key_id) {
            k.valid = false;
        }
    }
}

/// Close a watch service. Drops the underlying notify watcher (which stops
/// the OS-side worker), invalidates every key, and removes the entry from
/// the registry.
pub fn close_watch_service(ws_id: i32) {
    let mut svcs = watch_services().lock();
    if let Some(st) = svcs.get_mut(&ws_id) {
        st.open = false;
        for k in st.keys.values_mut() {
            k.valid = false;
        }
    }
    // Drop the WatcherState (and its RecommendedWatcher).
    svcs.remove(&ws_id);
}

/// Drain whatever is in the `notify` channel into the per-key pending queue.
/// Sets `signalled` for any newly-active key that wasn't already enqueued.
fn drain(state: &mut WatcherState) {
    loop {
        match state.rx.try_recv() {
            Ok(Ok(event)) => {
                let bit = match classify_event_kind(&event.kind) {
                    Some(b) => b,
                    None => continue,
                };
                for path in &event.paths {
                    // We registered the *parent* directory; the JDK
                    // WatchKey is keyed on that directory and its
                    // events carry the basename of the changed file.
                    let parent = path.parent().map(PathBuf::from).unwrap_or_default();
                    let parent_canonical = canonicalize_or_passthrough(&parent);

                    // Find the matching key. We attribute the event to
                    // the first key whose `dir` equals the parent — Java
                    // already disallows registering the same directory
                    // twice on the same WatchService; if multiple keys
                    // do happen to overlap we use the first match.
                    let target = state
                        .keys
                        .iter()
                        .find(|(_, k)| k.valid && k.dir == parent_canonical)
                        .map(|(id, _)| *id);
                    let target = match target {
                        Some(id) => id,
                        // Fall back: the event may target the dir
                        // itself (e.g. dir was deleted). Then `path`
                        // *is* the registered dir.
                        None => {
                            let direct_canonical = canonicalize_or_passthrough(path);
                            let id = state
                                .keys
                                .iter()
                                .find(|(_, k)| k.valid && k.dir == direct_canonical)
                                .map(|(id, _)| *id);
                            match id {
                                Some(id) => id,
                                None => continue,
                            }
                        }
                    };

                    // Filter against the registered kinds mask.
                    let mask = state.keys.get(&target).map(|k| k.kinds_mask).unwrap_or(0);
                    if mask & bit == 0 {
                        continue;
                    }

                    let basename = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    state
                        .pending
                        .entry(target)
                        .or_default()
                        .push((bit, basename));

                    // Signal the key (FIFO, deduped via `enqueued` flag).
                    // Split-borrow: take the not_enqueued check first,
                    // then mutate two distinct fields without overlap.
                    let not_enqueued = state
                        .keys
                        .get(&target)
                        .map(|k| !k.enqueued)
                        .unwrap_or(false);
                    if not_enqueued {
                        if let Some(k) = state.keys.get_mut(&target) {
                            k.enqueued = true;
                        }
                        state.signalled.push_back(target);
                    }
                }
            }
            Ok(Err(_)) => {
                // notify-internal error on this event — push an OVERFLOW
                // signal on every valid key, matching the JDK contract
                // for inotify queue overflows.
                let mut targets: Vec<i32> = Vec::new();
                for (id, k) in state.keys.iter() {
                    if k.valid {
                        targets.push(*id);
                    }
                }
                for id in targets {
                    state
                        .pending
                        .entry(id)
                        .or_default()
                        .push((KIND_OVERFLOW, String::new()));
                    let not_enqueued = state.keys.get(&id).map(|k| !k.enqueued).unwrap_or(false);
                    if not_enqueued {
                        if let Some(k) = state.keys.get_mut(&id) {
                            k.enqueued = true;
                        }
                        state.signalled.push_back(id);
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => {
                break;
            }
        }
    }
}

fn classify_event_kind(kind: &EventKind) -> Option<i32> {
    match kind {
        EventKind::Create(CreateKind::File)
        | EventKind::Create(CreateKind::Folder)
        | EventKind::Create(CreateKind::Any) => Some(KIND_CREATE),
        EventKind::Remove(RemoveKind::File)
        | EventKind::Remove(RemoveKind::Folder)
        | EventKind::Remove(RemoveKind::Any) => Some(KIND_DELETE),
        EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Metadata(_))
        | EventKind::Modify(ModifyKind::Name(_))
        | EventKind::Modify(ModifyKind::Any) => Some(KIND_MODIFY),
        _ => None,
    }
}

fn canonicalize_or_passthrough<P: AsRef<std::path::Path>>(p: P) -> PathBuf {
    let p = p.as_ref();
    std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// Internal shape for a non-blocking poll: drains one cycle of events and,
/// if any key is signalled, returns the head of the FIFO. Returns 0 when
/// no key is signalled.
fn try_pop_signalled(state: &mut WatcherState) -> i32 {
    drain(state);
    while let Some(id) = state.signalled.pop_front() {
        // Skip over invalidated keys.
        if let Some(k) = state.keys.get(&id) {
            if k.valid {
                return id;
            }
        }
    }
    0
}

/// How `poll_with_timeout` should interpret a `timeout_ns` argument.
///
/// Split out of the loop below as a pure function so the classification can be
/// asserted without a wall clock: the difference between the two negative
/// answers is "returns at once" versus "never returns", and a test that told
/// them apart by waiting would either hang on the wrong one or be a fixed
/// wall-clock bound, which this tree forbids for the reasons in
/// `docs/known-issues/jdk-only/README.md` §3.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WatchWait {
    /// Do not wait at all — one probe, then answer.
    Now,
    /// Wait until an event arrives or the service closes.
    Forever,
    /// Wait at most this many nanoseconds.
    Until(u64),
}

/// Classify a `poll0(long)` timeout.
///
/// `i64::MIN` is this file's own sentinel for `take()`, written by
/// [`take_blocking`], and is the ONLY value that means "block indefinitely".
///
/// # Why every other negative is [`WatchWait::Now`]
///
/// The condition here used to read `timeout_ns == i64::MIN || timeout_ns < 0`,
/// so **every** negative blocked forever. That is the wrong direction on a
/// timeout, and the JDK settles it rather than leaving it open:
/// `java.nio.file.WatchService.poll(long, TimeUnit)` names no exception for a
/// negative timeout, and `sun.nio.fs.AbstractWatchService.poll` hands the value
/// to `LinkedBlockingDeque.poll(timeout, unit)`, whose loop is
/// `if (nanos <= 0L) return null;` — a negative wait is a wait that has already
/// expired, and it answers `null` immediately. So the two readings are not both
/// defensible: "do not wait" is the JDK's, and "wait forever" converts a caller's
/// miscomputed deadline (`deadline - now` gone negative, the usual way a negative
/// timeout is produced at all) into a hang. `watch_timeout_millis` in
/// `native-io/src/lib.rs` — the other WatchService surface in this crate —
/// already reads `if timeout <= 0 { return 0; }`, so the two surfaces disagreed.
///
/// W7-8-fabricated-success-io-sweep.md recorded this family's `.max(0)` as
/// UNMEASURED with "no sentence either way". There is a sentence; it is in
/// `LinkedBlockingDeque`, not in the `WatchService` javadoc.
#[must_use]
pub fn watch_wait_for(timeout_ns: i64) -> WatchWait {
    if timeout_ns == i64::MIN {
        WatchWait::Forever
    } else if timeout_ns <= 0 {
        WatchWait::Now
    } else {
        WatchWait::Until(timeout_ns as u64)
    }
}

/// Block up to `timeout_ns` for an event. `i64::MIN` means "indefinite";
/// zero or negative means non-blocking (selectNow-style), for the reason
/// [`watch_wait_for`] sets out. Returns 0 on timeout/closed.
pub fn poll_with_timeout(ws_id: i32, timeout_ns: i64) -> Result<i32, MethodCallFailed> {
    let wait = watch_wait_for(timeout_ns);
    let select_now = wait == WatchWait::Now;
    let deadline = if let WatchWait::Until(ns) = wait {
        Some(Instant::now() + Duration::from_nanos(ns))
    } else {
        None
    };
    loop {
        // First, fast-path: drain + try to pop without sleeping.
        {
            let mut svcs = watch_services().lock();
            let Some(st) = svcs.get_mut(&ws_id) else {
                return Err(closed_ws());
            };
            if !st.open {
                return Err(closed_ws());
            }
            let id = try_pop_signalled(st);
            if id != 0 {
                return Ok(id);
            }
        }
        if select_now {
            return Ok(0);
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Ok(0);
            }
        }
        // Short sleep between probes. 25ms is small enough that close()
        // becomes visible promptly without busy-spinning the CPU.
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Blocking `take` — waits until an event arrives or the service closes.
/// Returns 0 if the service was closed mid-wait.
pub fn take_blocking(ws_id: i32) -> Result<i32, MethodCallFailed> {
    poll_with_timeout(ws_id, i64::MIN)
}

/// Drain currently-pending events for `key_id` into the returned `Vec`.
/// Each entry is `(kind_bit, basename)`. After this call the key's
/// `pending` is empty and its `enqueued` flag is cleared by `reset_key`.
pub fn poll_events(ws_id: i32, key_id: i32) -> Vec<(i32, String)> {
    let mut svcs = watch_services().lock();
    let Some(st) = svcs.get_mut(&ws_id) else {
        return Vec::new();
    };
    drain(st);
    st.pending.remove(&key_id).unwrap_or_default()
}

/// Re-arm a key. Returns true if the key is still valid.
pub fn reset_key(ws_id: i32, key_id: i32) -> bool {
    let mut svcs = watch_services().lock();
    let Some(st) = svcs.get_mut(&ws_id) else {
        return false;
    };
    let valid = st.keys.get(&key_id).map(|k| k.valid).unwrap_or(false);
    if !valid {
        return false;
    }
    if let Some(k) = st.keys.get_mut(&key_id) {
        k.enqueued = false;
    }
    // If the key still has pending events, immediately re-signal it so a
    // subsequent take()/poll() returns it again — matches the JDK
    // semantics of "reset re-enqueues if events accrued during pollEvents".
    let has_pending = st
        .pending
        .get(&key_id)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if has_pending {
        if let Some(k) = st.keys.get_mut(&key_id) {
            k.enqueued = true;
        }
        st.signalled.push_back(key_id);
    }
    true
}

// ---------------------------------------------------------------------------
// Native method shims (Java → Rust bridge)
// ---------------------------------------------------------------------------

fn closed_ws() -> MethodCallFailed {
    RuntimeError::IOException {
        message: "WatchService is closed".into(),
    }
    .into()
}

fn int_arg(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

fn long_arg(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

/// `<init>()V` — called when the Java-side `AbstractWatchService`
/// allocates a fresh service. We hand it a fresh ws_id and stash it on
/// the object's first int field. If the layout doesn't have a
/// suitable slot we still allocate and silently no-op, matching the
/// JDK behavior where a WatchService that loses its native id ends
/// up returning empty events forever (rather than crashing).
fn ws_init_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = open_watch_service()?;
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        // Stash on the first int field if the layout has one. The
        // exact slot is layout-dependent — we walk slot 0 first
        // (matches our other synthetic NIO classes).
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Int(id));
        }
    }
    Ok(None)
}

/// Helper: pull the ws_id out of the receiver's first int field.
fn ws_id_of(ctx: &mut dyn NativeContext, this: cratonvm_types::ObjectRef) -> i32 {
    if ctx.object_num_fields(this) == 0 {
        return 0;
    }
    match ctx.get_field(this, 0) {
        Value::Int(v) if v > 0 => v,
        _ => 0,
    }
}

/// `register0(String dir, int kinds) -> int` — install a watch and
/// return a key id (≥ 1) or throw `IOException`.
fn ws_register0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Err(RuntimeError::NullPointerException {
            message: Some("WatchService.register0: this".into()),
        }
        .into());
    };
    let ws_id = ws_id_of(ctx, this);
    if ws_id == 0 {
        return Err(closed_ws());
    }
    let dir = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(closed_ws()),
    };
    let kinds = int_arg(args, 2);
    let key = register_dir(ws_id, &dir, kinds)?;
    Ok(Some(Value::Int(key)))
}

/// `take0() -> int` — block until an event, return the signalled key id.
fn ws_take0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Err(closed_ws());
    };
    let ws_id = ws_id_of(ctx, this);
    if ws_id == 0 {
        return Err(closed_ws());
    }
    let key = take_blocking(ws_id)?;
    Ok(Some(Value::Int(key)))
}

/// `poll0(long timeout_ns) -> int` — non-blocking when timeout==0.
fn ws_poll0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Err(closed_ws());
    };
    let ws_id = ws_id_of(ctx, this);
    if ws_id == 0 {
        return Err(closed_ws());
    }
    let timeout = long_arg(args, 1);
    let key = poll_with_timeout(ws_id, timeout)?;
    Ok(Some(Value::Int(key)))
}

/// `cancel0(int keyId) -> void`
fn ws_cancel0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let ws_id = ws_id_of(ctx, this);
    let key = int_arg(args, 1);
    if ws_id != 0 {
        cancel_key(ws_id, key);
    }
    Ok(None)
}

/// `close0() -> void`
fn ws_close0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let ws_id = ws_id_of(ctx, this);
    if ws_id != 0 {
        close_watch_service(ws_id);
    }
    Ok(None)
}

/// `pollEventKinds0(int keyId) -> int[]` — returns the kind-bits for every
/// pending event in order. Pairs with `pollEventNames0` which returns the
/// matching basenames; both are drained in the same call so the indices
/// line up. We do the drain in `pollEventKinds0` and stash the names on
/// a per-thread side-table consumed by the next `pollEventNames0`.
fn ws_poll_event_kinds0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let ws_id = ws_id_of(ctx, this);
    let key = int_arg(args, 1);
    if ws_id == 0 {
        let arr = ctx.new_array(ArrayElementType::Int, 0);
        return Ok(Some(Value::Object(Some(arr))));
    }
    let events = poll_events(ws_id, key);
    let arr = ctx.new_array(ArrayElementType::Int, events.len());
    for (i, (kind, name)) in events.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*kind));
        // Stash names for the matching pollEventNames0 call.
        let _ = name; // see below
    }
    // Side-stash names on this thread keyed by (ws_id, key).
    stash_names(ws_id, key, events.into_iter().map(|(_, n)| n).collect());
    Ok(Some(Value::Object(Some(arr))))
}

/// `pollEventNames0(int keyId) -> String[]` — returns the basenames for
/// the events most recently drained by `pollEventKinds0` on the SAME
/// thread. The pairing is per-thread so concurrent threads don't race;
/// the data lives in a thread-local side-table and is consumed exactly
/// once.
fn ws_poll_event_names0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let ws_id = ws_id_of(ctx, this);
    let key = int_arg(args, 1);
    let names = take_stashed_names(ws_id, key);
    // The registered descriptor is `(I)[Ljava/lang/String;`, so the component
    // type is part of the contract. `crate::new_string_array` carries the
    // measurement; this row is `SyntheticStub`-tagged and measured dead, but a
    // dead row that would answer wrongly if it woke up is not worth keeping as
    // one.
    let arr = crate::new_string_array(ctx, names.len());
    for (i, name) in names.iter().enumerate() {
        let s = ctx.create_string(name);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `reset0(int keyId) -> boolean`
fn ws_reset0_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let ws_id = ws_id_of(ctx, this);
    let key = int_arg(args, 1);
    if ws_id == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let still_valid = reset_key(ws_id, key);
    Ok(Some(Value::Int(if still_valid { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Per-thread side-table for pollEventKinds/pollEventNames pairing.
// ---------------------------------------------------------------------------

thread_local! {
    static NAME_STASH: std::cell::RefCell<HashMap<(i32, i32), Vec<String>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn stash_names(ws_id: i32, key: i32, names: Vec<String>) {
    NAME_STASH.with(|c| {
        c.borrow_mut().insert((ws_id, key), names);
    });
}

fn take_stashed_names(ws_id: i32, key: i32) -> Vec<String> {
    NAME_STASH.with(|c| c.borrow_mut().remove(&(ws_id, key)).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Public registration entry-point
// ---------------------------------------------------------------------------

/// Register every native method this module owns on `r`.
///
/// The same set of natives is registered against four FQNs so any
/// platform-specific subclass dispatch lands on us regardless of which
/// concrete `WatchService` the JDK installs:
///
///   * `sun/nio/fs/AbstractWatchService` — base class, covers any direct
///     `<init>` dispatch.
///   * `sun/nio/fs/LinuxWatchService` — the Linux inotify impl.
///   * `sun/nio/fs/BsdWatchService` / `sun/nio/fs/MacOSXWatchService` /
///     `sun/nio/fs/PollingWatchService` — the other Unix impls.
///   * `sun/nio/fs/WindowsWatchService` — Windows IOCP-backed impl.
///
/// # CORRECTION 2026-08-22 (WORKER 4): the list named a class that does not
/// # exist, and the METHOD names do not exist either
///
/// The fourth entry used to be `sun/nio/fs/UnixWatchService`, described as the
/// "Linux/macOS/BSD platform impl". **There is no such class in any JDK image.**
/// MEASURED, `javap --module java.base`, Temurin 25.0.4+7 on Linux:
///
/// ```text
///   sun.nio.fs.AbstractWatchService   PRESENT
///   sun.nio.fs.LinuxWatchService      PRESENT
///   sun.nio.fs.UnixWatchService       absent      <- the name this list used
///   sun.nio.fs.PollingWatchService    absent      (present on macOS/AIX images)
///   sun.nio.fs.WindowsWatchService    absent      (present on Windows images)
/// ```
///
/// **And the method names below are CratonVM's own, not the JDK's.** The real
/// natives on `sun.nio.fs.LinuxWatchService` are
///
/// ```text
///   eventSize()  eventOffsets()  inotifyInit()  inotifyAddWatch(int,long,int)
///   inotifyRmWatch(int,int)  configureBlocking(int,boolean)
///   socketpair(int[])  poll(int,int)
/// ```
///
/// — not `init0` / `register0` / `take0` / `poll0(J)` / `cancel0` / `close0` /
/// `reset0` / `pollEventKinds0` / `pollEventNames0`. So every triple registered
/// below names a method that no class in the image declares, on top of one
/// class name that names nothing at all.
///
/// **That is why they are inert, and the inertness is measured, not inferred.**
/// `--dump-native-registry` over the corpus: every row here is
/// `invocations: 0` in `--real-jdk`, and the whole family is absent from the
/// `--jdk-only` registry because `SyntheticStub` is refused at the door. The
/// retag note below already measured the third thing that matters — that the
/// real `WatchService` delivers events on this VM with these natives refused.
///
/// **Kept rather than deleted, deliberately.** They cost nothing at runtime and
/// the list is the honest record of an API CratonVM defined; deleting it is a
/// separate change that needs a `--features synthetic-jdk` build to clear,
/// which this lane did not make. What is fixed here is the part that was
/// actively wrong: a class name that exists nowhere, and a doc comment that
/// presented this family as backing "the real JDK 25 pipeline". See
/// `WORKER-4-2` N5.
pub fn register_watch_service_real(_r: &mut NativeMethodRegistry) {
    // RETIRED 2026-08-22 (WORKER 4). This registered 54 triples -- nine `*0`
    // methods across six `sun/nio/fs/*WatchService` classes -- and every one of
    // them was unreachable.
    //
    // # The names do not exist
    //
    // MEASURED, `javap --module java.base`, Temurin 25.0.4+7. The real natives
    // on `sun.nio.fs.LinuxWatchService` are
    //
    //     eventSize  eventOffsets  inotifyInit  inotifyAddWatch  inotifyRmWatch
    //     configureBlocking  socketpair  poll(int,int)
    //
    // -- not `init0` / `register0` / `take0` / `poll0(J)` / `cancel0` /
    // `close0` / `reset0` / `pollEventKinds0` / `pollEventNames0`, which is
    // what this registered. The set was a CratonVM-defined API wearing
    // `sun.nio.fs` names, and one of the class names (`sun/nio/fs/
    // UnixWatchService`, corrected earlier the same day) named nothing on any
    // platform.
    //
    // # And nothing reached them, in ANY configuration
    //
    // `WORKER-4-2` N5 measured `invocations: 0` and REFUSED to delete on the
    // grounds that clearing it needed a `--features synthetic-jdk` build that
    // lane had not made. That build is made. `regression-suite/probes/
    // W4Watch.java` drives the whole lifecycle -- open, register, poll-empty,
    // create a file, poll-with-timeout until the event arrives, read the
    // events, reset, cancel, close, and the three closed-service refusals --
    // with `--dump-native-registry` on each of three configurations:
    //
    //     --jdk-only              0 rows registered (SyntheticStub, refused at the door)
    //     --real-jdk             54 rows,  0 INVOKED
    //     --features synthetic-jdk   54 rows,  0 INVOKED
    //
    // and the probe PASSES on all three. The public `java.nio.file.WatchService`
    // surface in `lib.rs` -- `native_ws_new`, `native_ws_register`,
    // `native_ws_poll`, `native_ws_take` and friends -- is what actually serves
    // it, and it does not call into this module at all.
    //
    // `[2cfgs]`: this is the case where checking the second feature config
    // CLOSED a refusal rather than opening one.
    //
    // # The ENGINE below is deliberately kept
    //
    // `open_watch_service`, `register_dir`, `poll_with_timeout`,
    // `take_blocking`, `poll_events`, `reset_key`, `cancel_key` and
    // `close_watch_service` are a real inotify-backed implementation with its
    // own tests. What was wrong was the DOOR, not the room: the engine was
    // wired to method names no JDK declares.
    //
    // Deleting it would destroy the option that is actually worth taking --
    // re-pointing it at the real names (`inotifyInit`, `inotifyAddWatch`,
    // `socketpair`, `poll(int,int)`) so the JDK's own `LinuxWatchService`
    // bytecode drives it, which is what a bridge is FOR. That is a measured
    // piece of work, not a deletion, and this note is here so whoever takes it
    // starts from the engine rather than from scratch.
    //
    // Until then the module is unreferenced. That is visible and honest;
    // 54 registry rows claiming to cover `sun.nio.fs` were neither.
}

// ---------------------------------------------------------------------------
// Tests — exercise the Rust-side API directly. The Java bridge functions
// are tested at the integration level in `vm` once they're wired in.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn write_file(p: &std::path::Path, data: &[u8]) {
        std::fs::write(p, data).expect("write file");
    }

    /// Wait up to `timeout` for `predicate` to return true while pumping
    /// the drain loop. Returns true if it fired.
    fn wait_for(
        ws_id: i32,
        key_id: i32,
        timeout: Duration,
        predicate: impl Fn(&[(i32, String)]) -> bool,
    ) -> bool {
        let start = Instant::now();
        let mut accumulated: Vec<(i32, String)> = Vec::new();
        while start.elapsed() < timeout {
            let evs = poll_events(ws_id, key_id);
            accumulated.extend(evs);
            if predicate(&accumulated) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn wp3_8_open_and_close_round_trip() {
        let id = open_watch_service().expect("open watch service");
        assert!(id >= 1);
        close_watch_service(id);
        // After close, register/poll/take all return errors or 0.
        let r = register_dir(id, ".", KIND_CREATE);
        assert!(r.is_err(), "register on closed ws must error");
    }

    #[test]
    fn wp3_8_register_nonexistent_path_errors() {
        let id = open_watch_service().unwrap();
        let r = register_dir(id, "/path/that/should/not/exist/zzz", KIND_CREATE);
        assert!(r.is_err());
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_classify_event_kinds() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert_eq!(
            classify_event_kind(&EventKind::Create(CreateKind::File)),
            Some(KIND_CREATE)
        );
        assert_eq!(
            classify_event_kind(&EventKind::Remove(RemoveKind::File)),
            Some(KIND_DELETE)
        );
        assert_eq!(
            classify_event_kind(&EventKind::Modify(ModifyKind::Any)),
            Some(KIND_MODIFY)
        );
        // EventKind::Any is the "we have no idea what happened" variant
        // and we deliberately don't surface it as create/modify/delete.
        assert_eq!(classify_event_kind(&EventKind::Any), None);
    }

    #[test]
    fn wp3_8_create_event_fires_on_real_filesystem() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let id = open_watch_service().expect("open");
        let key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE | KIND_MODIFY)
            .expect("register");

        // Touch a file inside the watched directory.
        write_file(&dir.path().join("hello.txt"), b"hi");

        let saw = wait_for(id, key, Duration::from_secs(3), |evs| {
            evs.iter()
                .any(|(k, _)| *k & (KIND_CREATE | KIND_MODIFY) != 0)
        });
        assert!(saw, "should observe a create or modify event");
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_delete_event_fires() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("doomed.bin");
        write_file(&path, b"x");

        let id = open_watch_service().expect("open");
        let key = register_dir(id, dir.path().to_str().unwrap(), KIND_DELETE).expect("register");

        std::fs::remove_file(&path).expect("remove");

        let saw = wait_for(id, key, Duration::from_secs(3), |evs| {
            evs.iter().any(|(k, _)| *k & KIND_DELETE != 0)
        });
        assert!(saw, "should observe a delete event");
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_modify_event_fires() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("file.bin");
        write_file(&path, b"v1");

        let id = open_watch_service().expect("open");
        let key = register_dir(id, dir.path().to_str().unwrap(), KIND_MODIFY).expect("register");

        // Sleep briefly to let the watcher install before the second write.
        std::thread::sleep(Duration::from_millis(50));
        write_file(&path, b"v2 longer content");

        let saw = wait_for(id, key, Duration::from_secs(3), |evs| {
            evs.iter().any(|(k, _)| *k & KIND_MODIFY != 0)
        });
        assert!(saw, "should observe a modify event");
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_poll_with_timeout_returns_zero_when_idle() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let id = open_watch_service().unwrap();
        let _key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE).unwrap();
        // Use a short positive timeout — should expire and return 0.
        let start = Instant::now();
        let key = poll_with_timeout(id, Duration::from_millis(150).as_nanos() as i64).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(key, 0);
        assert!(elapsed >= Duration::from_millis(100), "elapsed {elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "elapsed {elapsed:?}");
        close_watch_service(id);
    }

    /// A NEGATIVE timeout is "already expired", not "wait forever".
    ///
    /// RED against the tree as it stood before `watch_wait_for` existed: the
    /// condition there was `timeout_ns == i64::MIN || timeout_ns < 0`, so every
    /// negative classified as indefinite. The oracle is
    /// `sun.nio.fs.AbstractWatchService.poll(long, TimeUnit)` handing the value
    /// to `LinkedBlockingDeque.poll`, whose loop opens `if (nanos <= 0L) return
    /// null;`.
    ///
    /// Asserted on the classifier rather than by timing `poll_with_timeout`,
    /// deliberately: the pre-fix behaviour of the negative case is *never
    /// returns*, so a test that told the two apart by waiting would hang on a
    /// red tree instead of failing it, and any bound that avoided the hang would
    /// be a fixed wall-clock bound.
    #[test]
    fn wp3_8_a_negative_watch_timeout_does_not_wait() {
        assert_eq!(watch_wait_for(-1), WatchWait::Now, "poll(-1) must not wait");
        assert_eq!(watch_wait_for(-1_000_000_000), WatchWait::Now);
        assert_eq!(watch_wait_for(i64::MIN + 1), WatchWait::Now);
        // The one sentinel that does mean "block", written by `take_blocking`.
        assert_eq!(watch_wait_for(i64::MIN), WatchWait::Forever);
        // Unchanged either side of the fix.
        assert_eq!(watch_wait_for(0), WatchWait::Now);
        assert_eq!(watch_wait_for(1), WatchWait::Until(1));
    }

    /// `take()` must still block. The guard against fixing the row above
    /// backwards by collapsing `i64::MIN` into the other negatives — which would
    /// turn every `WatchService.take()` in the process into a busy `poll()`
    /// returning `null`, and no assertion in this file would have said so.
    #[test]
    fn wp3_8_take_still_blocks_after_the_negative_timeout_fix() {
        assert_eq!(
            watch_wait_for(i64::MIN),
            WatchWait::Forever,
            "take_blocking's sentinel must remain the indefinite one"
        );
    }

    #[test]
    fn wp3_8_take_returns_signalled_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let id = open_watch_service().unwrap();
        let key = register_dir(
            id,
            dir.path().to_str().unwrap(),
            KIND_CREATE | KIND_MODIFY | KIND_DELETE,
        )
        .unwrap();

        // Run take() on a worker so we can drive an event from the test thread.
        let path = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || take_blocking(id));

        // Give the worker a moment to enter its loop.
        std::thread::sleep(Duration::from_millis(80));
        write_file(&path.join("trigger.txt"), b"go");

        let result = worker.join().expect("worker join").expect("take ok");
        assert_eq!(result, key);
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_cancel_invalidates_key_and_subsequent_events_drop() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let id = open_watch_service().unwrap();
        let key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE).unwrap();

        cancel_key(id, key);

        // Generate an event after cancel — it must NOT signal the key.
        write_file(&dir.path().join("after_cancel.txt"), b"x");
        std::thread::sleep(Duration::from_millis(200));
        let r = poll_with_timeout(id, 0).unwrap();
        assert_eq!(r, 0, "cancelled key must not surface events");
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_reset_re_arms_signal_when_more_events_pending() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let id = open_watch_service().unwrap();
        let key =
            register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE | KIND_MODIFY).unwrap();

        write_file(&dir.path().join("a.txt"), b"x");
        std::thread::sleep(Duration::from_millis(150));
        // Drain once → key gets unsignalled.
        let _ = poll_events(id, key);

        // reset() with no pending: returns true (still valid), no
        // re-enqueue.
        assert!(reset_key(id, key));

        // Now generate another event and verify reset re-arms.
        write_file(&dir.path().join("b.txt"), b"y");
        std::thread::sleep(Duration::from_millis(150));
        let r = poll_with_timeout(id, 0).unwrap();
        assert_eq!(r, key, "next event must signal the key again");
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_kind_mask_filters_unrelated_events() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let id = open_watch_service().unwrap();
        // Only register CREATE — modifies on existing files must not surface.
        let path = dir.path().join("preexisting.bin");
        write_file(&path, b"v1");
        std::thread::sleep(Duration::from_millis(50));

        let key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE).unwrap();
        // Modify the preexisting file — should NOT be visible (we only
        // asked for CREATE).
        write_file(&path, b"v2 different bytes");
        std::thread::sleep(Duration::from_millis(200));

        let evs = poll_events(id, key);
        assert!(
            evs.iter().all(|(k, _)| *k & KIND_CREATE != 0),
            "registering CREATE-only must not yield modify events: {evs:?}"
        );
        close_watch_service(id);
    }

    #[test]
    fn wp3_8_thread_local_name_stash_isolates_per_key() {
        stash_names(1, 2, vec!["a".into(), "b".into()]);
        stash_names(1, 3, vec!["c".into()]);
        let n2 = take_stashed_names(1, 2);
        assert_eq!(n2, vec!["a".to_string(), "b".to_string()]);
        let n3 = take_stashed_names(1, 3);
        assert_eq!(n3, vec!["c".to_string()]);
        // Re-take returns empty (consumed exactly once).
        assert!(take_stashed_names(1, 2).is_empty());
    }

    #[test]
    fn wp3_8_register_on_closed_service_errors() {
        let id = open_watch_service().unwrap();
        close_watch_service(id);
        let r = register_dir(id, ".", KIND_CREATE);
        assert!(r.is_err());
    }

    #[test]
    fn wp3_8_register_returns_distinct_key_ids_per_call() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let id = open_watch_service().unwrap();
        let k1 = register_dir(id, dir1.path().to_str().unwrap(), KIND_CREATE).unwrap();
        let k2 = register_dir(id, dir2.path().to_str().unwrap(), KIND_CREATE).unwrap();
        assert_ne!(k1, k2);
        close_watch_service(id);
    }

    /// HIGH-severity security regression guard: under
    /// `set_path_confine_to_cwd(true)`, a guest must not be able to call
    /// `WatchService.register("../../etc")` and bypass `validate_path`
    /// via the inotify / ReadDirectoryChangesW backend. The Java-visible
    /// failure must be an `IOException` (the entry point's declared
    /// throws), not a panic, not a silent success, and not a
    /// `SecurityException` leaking up out of the watch service API.
    #[test]
    fn ws_register_rejects_relative_traversal_under_confinement() {
        let _g = crate::test_support::confine_test_lock().lock();
        crate::set_path_confine_to_cwd(true);

        let id = open_watch_service().expect("open watch service");
        let r = register_dir(id, "../../etc/passwd", KIND_CREATE);

        // Restore default BEFORE assertions so a failing assert doesn't
        // strand the rest of the test suite in confinement mode.
        crate::set_path_confine_to_cwd(false);
        close_watch_service(id);

        assert!(
            r.is_err(),
            "traversal path accepted by WatchService.register"
        );
        let err = format!("{:?}", r.unwrap_err());
        assert!(
            err.contains("IOException"),
            "expected IOException, got: {err}"
        );
        assert!(
            err.contains("sandbox") || err.contains("traversal"),
            "expected sandbox/traversal-related message, got: {err}"
        );
    }
}
