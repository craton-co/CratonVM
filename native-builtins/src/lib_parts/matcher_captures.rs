#[allow(unused_imports)]
use super::super::*;

/// Capture-group byte offsets (absolute, into the SAME `decoded` string they
/// were computed against) from a single `find()` call, cached so an
/// immediately-following `group(N)`/`start(N)`/`end(N)` — the overwhelmingly
/// common `while (m.find()) { m.group(N); }` idiom — doesn't have to re-run
/// the regex engine a second time just to recover boundaries `find()` already
/// computed (`native_matcher_group_idx`/`matcher_group_boundary`'s old
/// unconditional `re.captures(&input[start..])` re-search cost was itself
/// the dominant remaining O(n) driver even after the input-redecode fix —
/// confirmed by an isolated `find()`-only benchmark scaling linearly while
/// the `find()+group()` benchmark stayed superlinear).
pub struct MatcherCaptures {
    /// The match this was computed for (the Matcher's `MAT_FIELD_MATCH_START`
    /// at capture time). Only trusted by a reader when it still equals the
    /// Matcher's CURRENT match start — any match-producing path that doesn't
    /// populate this (`find(int)`, `matches()`, `lookingAt()`, `region()`)
    /// simply leaves it stale/mismatched, and callers fall back to a fresh,
    /// always-correct re-search.
    pub match_start: usize,
    pub groups: Vec<Option<(usize, usize)>>,
    pub named: std::collections::HashMap<String, usize>,
}

/// Advance a byte offset into `text` past exactly one UTF-8 scalar value.
///
/// Used by `Matcher.find()` to step past a zero-width match. A bare
/// `offset + 1` could land in the middle of a multi-byte character and panic
/// on the next `&text[offset..]` slice, so when `offset` is inside the string
/// we round up to the next char boundary.
///
/// When `offset` is already at the end of the string (a valid zero-width
/// match position — e.g. `$` or `x*` matching `[len,len]`), we return
/// `text.len() + 1`. That is deliberately one past the end so the *next*
/// `find()` trips its `offset > input.len()` guard and cleanly reports "no
/// match" — mirroring HotSpot's `Matcher.find()`, which advances
/// `nextSearchIndex` past `to` and then returns `false`. Returning `text.len()`
/// here instead would let `find()` re-match the empty string at the end
/// forever.
pub fn advance_one_char(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len() + 1;
    }
    let mut next = offset + 1;
    while next < text.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    next
}

/// Resolved slot indices for every `Matcher`/`Pattern` field this fast path
/// touches, resolved BY NAME exactly once (via `resolve_field_index`, the
/// same name-based resolution `get_field_by_name` does internally) and
/// cached forever after.
///
/// Why this exists: `get_field_by_name`/`set_field_by_name` each take a
/// `class_manager` `RwLock::read()` and walk the class hierarchy searching
/// for the field by name — on EVERY call, not just the first. A single
/// `find()` call touches ~13 Matcher fields, so a naive by-name
/// implementation pays that lock-plus-hierarchy-walk cost 13 times per
/// match; measured, this was the dominant cost of the whole fast path
/// (worse than the regex search itself). `get_field(obj, index)` by
/// contrast resolves through a per-class-id CACHED descriptor lookup with
/// no lock — see `vm/src/vm/vm_exec.rs`'s `get_field`/`get_field_by_name`
/// implementations. Since `java.util.regex.Matcher`/`Pattern` are bootstrap
/// classes (exactly one loaded definition VM-wide, field layout fixed for
/// the process lifetime), resolving each field's slot index once via
/// `resolve_field_index` and reusing it is both safe (still layout-agnostic
/// — never a hardcoded slot number, just a cached RESULT of the same
/// by-name resolution) and avoids paying the resolution cost on every call.
#[derive(Clone, Copy)]
pub struct MatcherFieldIndices {
    pub parent_pattern: usize,
    pub text: usize,
    pub from: usize,
    pub to: usize,
    pub first: usize,
    pub last: usize,
    pub old_last: usize,
    pub groups: usize,
    pub hit_end: usize,
    pub require_end: usize,
    pub transparent_bounds: usize,
    pub anchoring_bounds: usize,
    pub mod_count: usize,
}

#[derive(Clone, Copy)]
pub struct PatternFieldIndices {
    pub pattern: usize,
    pub flags: usize,
    pub capturing_group_count: usize,
}

/// One matcher's decoded input text plus the UTF-8-byte <-> UTF-16-code-unit
/// offset tables needed to bridge Java's UTF-16 `Matcher` indices to the
/// Rust `regex`/`fancy-regex` crates' UTF-8 byte offsets (and back). Built
/// once per distinct `text` object and cached — this is what avoids the
/// exact O(n)-redecode-per-call bug the legacy bridge had (see the module
/// banner and `matcher-native-full-input-redecode-quadratic-FIXED.md`), plus (new
/// here) avoids paying the O(n) offset-table build more than once per input.
/// Everything this fast path needs to run a search, resolved once per
/// distinct `(matcher identity, text identity, pattern identity)` triple and
/// cached — a SINGLE mutex lock covers text decode/offset-tables AND the
/// compiled regex together, instead of two separate cache lookups (one for
/// text, one via `compile_java_regex`'s own lock) on every `find()` call.
/// Measured: consolidating these (plus resolving the `text` object's class
/// only on a cache MISS, not every call — see `matcher_realjdk_cached`)
/// meaningfully reduced per-call overhead versus two independent caches.
pub struct MatcherRealCache {
    /// `identity_hash_code` of the `text` field's current value, so a
    /// `reset(CharSequence)` swap invalidates this entry (same GC-move-safe
    /// keying rationale as the legacy `matcher_read_input_cached`).
    pub text_identity: i32,
    /// `identity_hash_code` of the `parentPattern` field's current value, so
    /// `usePattern(Pattern)` swapping the compiled pattern invalidates this
    /// entry too.
    pub pattern_identity: i32,
    /// The semantic capture count, validated against the compiled Rust regex
    /// on cache construction. The exact Pattern object is part of the cache
    /// key, so steady-state find calls only need to recheck groups[] capacity.
    pub capture_count: usize,
    pub(crate) ascii_run: Option<regex_matcher::AsciiRun>,
    pub utf8: std::sync::Arc<str>,
    /// `byte_to_utf16[byte_offset] == utf16 code-unit offset at that byte`.
    /// Length `utf8.len() + 1` (entries at non-char-boundary byte offsets
    /// are unused filler — the `regex`/`fancy-regex` crates only ever report
    /// match offsets on char boundaries).
    pub byte_to_utf16: std::sync::Arc<[u32]>,
    /// `utf16_to_byte[utf16_offset] == byte offset of that code unit`.
    /// Length `utf16_len + 1`; the second unit of a surrogate pair maps to
    /// the SAME byte offset as the first (both denote one Unicode scalar in
    /// the UTF-8 side) — Java match boundaries essentially never split a
    /// surrogate pair, matching the `regex` crate's own codepoint-atomic
    /// match boundaries.
    pub utf16_to_byte: std::sync::Arc<[u32]>,
    /// Cloning a `JavaRegex` is cheap (the underlying `regex`/`fancy-regex`
    /// engines are internally reference-counted), so storing the compiled
    /// regex here and cloning it out on a cache hit avoids re-entering
    /// `compile_java_regex`'s own separate mutex-guarded cache on every call.
    pub re: JavaRegex,
}

thread_local! {
    /// A Matcher is explicitly not thread-safe, so the most recent cache entry
    /// on this Java thread is the overwhelmingly common case. The global map
    /// remains the cross-thread/cold fallback.
    pub static MATCHER_REAL_LAST_CACHE:
        std::cell::RefCell<Option<(usize, usize, usize, std::sync::Arc<MatcherRealCache>)>> =
        const { std::cell::RefCell::new(None) };

    /// Java's public Matcher state-changing methods increment `modCount`.
    /// Cache the fields needed by the next `find()` behind that guard so a
    /// steady-state loop pays one field read instead of five.
    pub static MATCHER_REAL_LAST_STATE:
        std::cell::Cell<Option<MatcherRealState>> =
        const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy)]
pub struct MatcherRealState {
    pub matcher_raw: usize,
    pub groups_raw: usize,
    pub mod_count: i32,
    pub first: i32,
    pub last: i32,
    pub from: i32,
    pub to: i32,
}

// ===========================================================================
// java.time — LocalDate, LocalTime, Instant, Duration (Phase 17)
// ===========================================================================

// --- LocalDate: 3-field synthetic (year, month, dayOfMonth) ---
pub const LD_FIELD_YEAR: usize = 0;
pub const LD_FIELD_MONTH: usize = 1;
pub const LD_FIELD_DAY: usize = 2;
pub const LD_NUM_FIELDS: usize = 3;

// --- LocalTime: 4-field synthetic (hour, minute, second, nano) ---
pub const LT_FIELD_HOUR: usize = 0;
pub const LT_FIELD_MINUTE: usize = 1;
pub const LT_FIELD_SECOND: usize = 2;
pub const LT_FIELD_NANO: usize = 3;
pub const LT_NUM_FIELDS: usize = 4;

// --- Instant: 2-field synthetic (epochSecond as Long, nano as Int) ---
pub const INST_FIELD_EPOCH_SEC: usize = 0;
pub const INST_FIELD_NANO: usize = 1;
pub const INST_NUM_FIELDS: usize = 2;

// --- Duration: 2-field synthetic (seconds as Long, nanos as Int) ---
pub const DUR_FIELD_SECONDS: usize = 0;
pub const DUR_FIELD_NANOS: usize = 1;
pub const DUR_NUM_FIELDS: usize = 2;

pub fn alloc_local_date(
    ctx: &mut dyn NativeContext,
    year: i32,
    month: i32,
    day: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = alloc_time_synthetic(ctx, "java/time/LocalDate", LD_NUM_FIELDS);
    ctx.set_field(obj, LD_FIELD_YEAR, Value::Int(year));
    ctx.set_field(obj, LD_FIELD_MONTH, Value::Int(month));
    ctx.set_field(obj, LD_FIELD_DAY, Value::Int(day));
    Ok(obj)
}

pub fn alloc_local_time(
    ctx: &mut dyn NativeContext,
    hour: i32,
    minute: i32,
    second: i32,
    nano: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = alloc_time_synthetic(ctx, "java/time/LocalTime", LT_NUM_FIELDS);
    ctx.set_field(obj, LT_FIELD_HOUR, Value::Int(hour));
    ctx.set_field(obj, LT_FIELD_MINUTE, Value::Int(minute));
    ctx.set_field(obj, LT_FIELD_SECOND, Value::Int(second));
    ctx.set_field(obj, LT_FIELD_NANO, Value::Int(nano));
    Ok(obj)
}

pub fn alloc_instant(
    ctx: &mut dyn NativeContext,
    epoch_sec: i64,
    nano: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = alloc_time_synthetic(ctx, "java/time/Instant", INST_NUM_FIELDS);
    ctx.set_field(obj, INST_FIELD_EPOCH_SEC, Value::Long(epoch_sec));
    ctx.set_field(obj, INST_FIELD_NANO, Value::Int(nano));
    Ok(obj)
}

pub fn alloc_duration(
    ctx: &mut dyn NativeContext,
    seconds: i64,
    nanos: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // Normalize: nanos must be in [0, 999_999_999]
    let total_nanos = seconds as i128 * 1_000_000_000 + nanos as i128;
    let norm_sec = (total_nanos.div_euclid(1_000_000_000)) as i64;
    let norm_nanos = (total_nanos.rem_euclid(1_000_000_000)) as i32;
    let obj = alloc_time_synthetic(ctx, "java/time/Duration", DUR_NUM_FIELDS);
    ctx.set_field(obj, DUR_FIELD_SECONDS, Value::Long(norm_sec));
    ctx.set_field(obj, DUR_FIELD_NANOS, Value::Int(norm_nanos));
    Ok(obj)
}

pub fn alloc_time_synthetic(ctx: &mut dyn NativeContext, class: &str, n: usize) -> ObjectRef {
    match ctx.ensure_class_initialized(class) {
        Ok(cid) => ctx.alloc_object(cid, n),
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), n),
    }
}

// The calendar itself lives in [`crate::civil_date`]; these are the names
// this module's natives already use. It kept its own copy until 2026-08-28,
// and that copy was wrong for every day of every leap year.
pub fn is_leap_year(year: i32) -> bool {
    crate::civil_date::is_leap_year(year)
}

pub fn days_in_month(year: i32, month: i32) -> i32 {
    crate::civil_date::days_in_month(year, month)
}

/// Day of year (1-based).
pub fn day_of_year(year: i32, month: i32, day: i32) -> i32 {
    crate::civil_date::day_of_year(year, month, day)
}

/// Days since 1970-01-01 for a `(year, month, day)`.
pub fn to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
    crate::civil_date::to_epoch_day(year, month, day)
}

/// Convert epoch day back to (year, month, day).
pub fn from_epoch_day(epoch_day: i64) -> (i32, i32, i32) {
    crate::civil_date::from_epoch_day(epoch_day)
}

/// Zeller-like day-of-week from epoch day (Monday=1 .. Sunday=7, matching Java's DayOfWeek).
pub fn day_of_week_from_epoch_day(epoch_day: i64) -> i32 {
    // 1970-01-01 was a Thursday (4 in Java's DayOfWeek enum)
    let dow = ((epoch_day + 3) % 7 + 7) % 7 + 1; // 1=Mon .. 7=Sun
    dow as i32
}

// T16.8: register_reference_natives and its helpers moved to
// `src/reference.rs` (Session 88). The block previously lived here (old
// lines ~9037..9382). Registration is now invoked as
// `reference::register_reference_natives(registry)` from
// `register_essential_natives`.

// ==================== Phase 24: java.util.concurrent ====================
//
// Condition = 1-field synthetic (lock=0 reference to owning ReentrantLock)
// CountDownLatch = 1-field synthetic (count=0 Int)
// Semaphore = 2-field synthetic (permits=0 Int, fair=1 Int)
// CyclicBarrier = 3-field synthetic (parties=0 Int, count=1 Int, broken=2 Int)
// CopyOnWriteArrayList = 2-field synthetic (same as ArrayList: data=0, size=1)
//
// ReentrantLock: lock state lives in a SIDE TABLE (`rl_state_table`), not in
// object fields. `java.util.concurrent.locks.ReentrantLock` is loaded as the
// real JDK class, whose sole instance field (slot 0) is the `Sync`
// reference. The lock natives used to scribble `owner`/`holdCount`/`fair`
// into slots 0/1/2 — slot 0 collided with `sync`, so a `long` owner written
// there did not round-trip (a typed reference read decodes the bits as
// `Object`), and slots 1/2 were out of bounds. The side table, keyed by the
// lock's GC-stable monotonic identity-hash, sidesteps the layout entirely.

/// Logical "no owner" for a `ReentrantLock`.
pub const RL_UNOWNED: i64 = -1;

pub const COND_FIELD_LOCK: usize = 0;

/// Per-`ReentrantLock` lock state held in the side table.
#[derive(Clone, Copy, Default)]
pub struct RlState {
    /// Owning thread id, or `-1` ([`RL_UNOWNED`]) when unheld.
    pub owner: i64,
    /// Reentrant hold count (0 when unheld).
    pub hold: i32,
    /// `true` for a fair lock.
    pub fair: bool,
}

/// Stable key for a lock object: `(vm_identity, the lock's weak lock key)`.
///
/// The VM half is not decoration: it is what [`forget_vm_lock_state`] tears a
/// VM's rows down by. It was the whole cross-VM protection while the second
/// half was the identity hash -- hashes are 32-bit and minted per VM from one
/// seed, so without the partition a fresh `ReentrantLock` in VM 2 could be
/// born already held by a thread of VM 1.
///
/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 11): the second half is the lock's WEAK lock key
/// ([`gc_stable_weak_lock_key`]), not its identity hash. Two live
/// `ReentrantLock` / `ReentrantReadWriteLock`s of ONE VM share an identity
/// hash once the heap's counter has wrapped, and they shared owner, hold count
/// and readers -- mutual exclusion broken, or a deadlock. A lock dropped while
/// held left an "owned" row that a new lock with its hash was born into. The
/// weak key is unique per live object, stable across moves, and freed when
/// the lock dies, which drops its rows ([`forget_rl_state_keys`]).
pub type RlKey = (usize, usize);

/// Side table: [`RlKey`] → [`RlState`].
pub fn rl_state_table() -> &'static std::sync::Mutex<std::collections::HashMap<RlKey, RlState>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<RlKey, RlState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Stable key for a lock object — see [`RlKey`]. Mints the weak lock key on
/// first use (a read of a never-locked lock leaves a weak slot, dropped with
/// the lock). Every caller hands in the lock's CURRENT reference (a native's
/// receiver, or a re-read after a GC point), which is what the registry
/// matches by.
pub fn rl_key(ctx: &mut dyn NativeContext, lock: ObjectRef) -> RlKey {
    (ctx.vm_identity(), lib_side_key(ctx, lock))
}

/// Drop the `ReentrantLock` / `ReentrantReadWriteLock` rows filed under
/// `keys`, which the lock-key registry has just freed (the lock died, or its
/// VM is being torn down). Called through [`forget_core_object_state_keys`]
/// with the registry lock released; each table lock is a leaf, taken on its
/// own, and poison-tolerant because this runs in the collection epilogue.
pub fn forget_rl_state_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    {
        let mut t = rl_state_table().lock().unwrap_or_else(|e| e.into_inner());
        if !t.is_empty() {
            let freed: rustc_hash::FxHashSet<usize> = keys.iter().copied().collect();
            t.retain(|&(_, key), _| !freed.contains(&key));
        }
    }
    crate::util_concurrent_ext::forget_rwl_state_keys(keys);
}

/// Drop every lock-state row belonging to `vm_identity`. Called from
/// `release_vm_native_state`.
pub fn forget_vm_lock_state(vm_identity: usize) {
    rl_state_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(vm, _), _| *vm != vm_identity);
    crate::util_concurrent_ext::forget_vm_rwl_state(vm_identity);
}

/// Read the current [`RlState`] for a lock (default = unheld).
pub fn rl_get(key: RlKey) -> RlState {
    rl_state_table()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).copied())
        .unwrap_or(RlState {
            owner: RL_UNOWNED,
            hold: 0,
            fair: false,
        })
}

/// Run `f` with exclusive access to a lock's [`RlState`], creating a
/// default (unheld) entry on first touch. The whole closure runs under the
/// side-table mutex, so a read-modify-write of the lock state is atomic.
pub fn rl_with<R>(key: RlKey, f: impl FnOnce(&mut RlState) -> R) -> R {
    let mut t = rl_state_table().lock().unwrap_or_else(|e| e.into_inner());
    let st = t.entry(key).or_insert(RlState {
        owner: RL_UNOWNED,
        hold: 0,
        fair: false,
    });
    f(st)
}

/// Wait on `obj`'s monitor for up to `timeout_ms`. The caller must already
/// hold the monitor (via a preceding `ctx.monitor_enter(obj)`) before
/// calling this. ALWAYS releases the monitor before returning -- including
/// when `monitor_wait` itself errors (e.g. `InterruptedException`, which
/// `monitor_wait` throws exactly like `Object.wait()` does on either an
/// entry-time or wake-time interrupt).
///
/// A bare `ctx.monitor_wait(obj, ms)?;` immediately followed by
/// `ctx.monitor_exit(obj);` -- the pattern this replaces -- never reaches
/// the `monitor_exit` call when `monitor_wait` errors: the `?` returns
/// early, permanently leaking `obj`'s monitor as held by this thread.
/// Every subsequent `monitor_enter(obj)` on the same object (from ANY
/// thread, including this native's own retry loop and `native_rl_unlock`'s
/// notify step) then blocks forever.
///
/// Confirmed live via `gdb -p <pid> -batch -ex 'thread apply all bt'`
/// during the Tomcat `TestWsRemoteEndpointImplServerDeadlock` investigation
/// (2026-07-09, wsremoteendpoint-close-delay-near-deadlock-FIXED.md):
/// every one of ~10 worker threads was piled up inside `native_rl_unlock`'s
/// `monitor_enter`, all blocked on the same wedged monitor, after a prior
/// `native_rl_lock` caller hit exactly this leak on a `Thread.interrupt()`.
/// Only surfaced once real `ScheduledThreadPoolExecutor`/`ThreadPoolExecutor`
/// contention started reaching these natives (see
/// `enumset-synthetic-surface-drop-realmode-FIXED.md`).
pub fn monitor_wait_release(
    ctx: &mut dyn NativeContext,
    obj: &mut ObjectRef,
    timeout_ms: Option<u64>,
) -> MethodCallResult {
    // `monitor_wait` PARKS this thread, so a collection runs inside this call
    // whenever one is due — the moving collector, or the non-moving young
    // sweep's selective promotion. The exit below and every later iteration of
    // the caller's retry loop must therefore use the POST-WAIT address:
    // `MonitorTable::exit` opens with `header_of(obj)`, so a pre-wait address
    // is an `EXCEPTION_ACCESS_VIOLATION` when the young slot was reclaimed and
    // a permanently LEAKED monitor when it was merely moved — every later
    // waiter on that object then blocks forever.
    //
    // The refreshed reference is written back through `obj` rather than
    // returned, so a caller that keeps looping cannot forget to re-read it:
    // every call site here is a `loop { ... monitor_wait_release(..)? }` retry.
    let pin = ctx.pin_native_root(*obj);
    let result = ctx.monitor_wait(*obj, timeout_ms);
    *obj = ctx.read_native_pin(pin, *obj);
    ctx.monitor_exit(*obj);
    ctx.unpin_native_roots(pin);
    result
}

/// `true` iff `e` is the VM's `InterruptedException` (the error
/// `monitor_wait` raises on an entry-time or wake-time interrupt). Used by
/// `native_rl_lock` to distinguish "must absorb and retry" (this) from any
/// other error (must propagate).
pub fn is_interrupted_exception(e: &cratonvm_types::error::MethodCallFailed) -> bool {
    matches!(
        e,
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::InterruptedException
            )
        )
    )
}

pub const CDL_FIELD_COUNT: usize = 0;

pub const SEM_FIELD_PERMITS: usize = 0;
pub const SEM_FIELD_FAIR: usize = 1;

pub const CB_FIELD_PARTIES: usize = 0;
pub const CB_FIELD_COUNT: usize = 1;
pub const CB_FIELD_BROKEN: usize = 2;

/// Convert a time value + TimeUnit ordinal to milliseconds.
/// Ordinals: 0=NANOSECONDS 1=MICROSECONDS 2=MILLISECONDS 3=SECONDS 4=MINUTES 5=HOURS 6=DAYS
///
/// The scaling-UP cases (SECONDS/MINUTES/HOURS/DAYS) must SATURATE on overflow,
/// exactly like `java.util.concurrent.TimeUnit.x(d, m, over)` — a plain `value *
/// m` wraps. e.g. `TimeUnit.SECONDS.toMillis(Long.MAX_VALUE)` is `Long.MAX_VALUE`,
/// not `-1000`. Kafka's `Sensor` derives `inactiveSensorExpirationTimeMs =
/// SECONDS.toMillis(Long.MAX_VALUE)`; a wrapped negative value made every fresh
/// sensor report `hasExpired() == true`, so `Sensor.add(...)` silently dropped
/// every metric and `metrics.metric(name)` returned null.
/// Read a `java.util.concurrent.TimeUnit`'s ordinal robustly.
///
/// The real `TimeUnit` is a JDK enum: its ordinal lives in the `Enum` base field
/// `ordinal`, while object slot 0 holds the enum `name` (a `String`). Several
/// timed natives historically read slot 0 for the ordinal — which works in their
/// unit tests (they build a synthetic 1-field TimeUnit with the ordinal at slot
/// 0) but is WRONG for the real enum the JDK passes: `await(N, SECONDS)` then
/// read the `name` String, fell back to MILLISECONDS, and waited `N` ms instead
/// of `N` seconds — so `CountDownLatch.await(10, SECONDS)` returned `false`
/// almost immediately (ES `RestClient*IntegTests` async assertions). Prefer the
/// named `ordinal` field; fall back to slot 0 for the synthetic test object.
/// The `ordinal` of a `TimeUnit` constant, with the field index memoized per
/// receiver class.
///
/// `get_field_by_name` takes the class-manager read lock and walks the
/// hierarchy comparing field-name strings on EVERY call — the same cost the
/// `setExclusiveOwnerThread` native memoized away for the same reason, and
/// this native is on the same path: the exact census
/// (`--nojit CRATONVM_DISABLE_INTRINSICS=1`) puts `toNanos` + `toMillis` at
/// **2.00 calls per expired task** on `HashedWheelTimerTest`, because
/// `HashedWheelTimer.newTimeout` converts the delay and every fired task
/// converts the elapsed nanos back. Measured as a pair from compiled code at
/// 258 ns against HotSpot's 2.0 (`probes/FunnelRungRate.java`).
///
/// The index is a per-class constant, so the memo is keyed on the receiver's
/// class id. `java/util/concurrent/TimeUnit` is one class with seven
/// constants and no constant bodies in JDK 25, so two slots is already
/// generous; the second exists for an image that gives the constants their own
/// subclasses. A miss falls back to the resolving path, so an unexpected
/// layout is slow rather than wrong, and the pre-existing slot-0 fallback is
/// untouched. Class redefinition needs no invalidation: it allocates a NEW
/// `ClassId`, so a stale entry can never be consulted for the redefined class.
///
/// Keyed on (VM identity, class id): `ClassId`s are minted per VM and one
/// thread can run natives for several VMs (gc-common w18-g).
pub fn time_unit_ordinal(ctx: &dyn NativeContext, u: ObjectRef) -> i32 {
    const MEMO_SLOTS: usize = 2;
    thread_local! {
        static ORDINAL_FIELD_INDEX: std::cell::RefCell<[(usize, u32, u32); MEMO_SLOTS]> =
            const { std::cell::RefCell::new([(0, u32::MAX, 0); MEMO_SLOTS]) };
    }
    let class_id = ctx.class_id_of_object(u);
    let raw_cid = class_id.as_u32();
    let vm = ctx.vm_identity();
    let cached = ORDINAL_FIELD_INDEX.with(|memo| {
        memo.borrow()
            .iter()
            .find(|(memo_vm, cid, _)| *cid == raw_cid && *memo_vm == vm)
            .map(|(_, _, index)| *index as usize)
    });
    if let Some(index) = cached {
        if let Some(o) = ctx.get_field(u, index).as_int() {
            return o;
        }
    }
    if let Some(index) = ctx.resolve_field_index_by_class_id(class_id, "ordinal") {
        if let Some(o) = ctx.get_field(u, index).as_int() {
            ORDINAL_FIELD_INDEX.with(|memo| {
                let mut memo = memo.borrow_mut();
                // Take a free slot, else evict slot 0. The policy does not need
                // to be clever at this size, but the table must not be able to
                // grow without bound.
                let victim = memo
                    .iter()
                    .position(|(_, cid, _)| *cid == u32::MAX)
                    .unwrap_or(0);
                // Cast: field counts are far below `u32::MAX`.
                memo[victim] = (vm, raw_cid, index as u32);
            });
            return o;
        }
    }
    ctx.get_field(u, 0).as_int().unwrap_or(2)
}

pub fn time_unit_nanos_per(ordinal: i32) -> i128 {
    match ordinal {
        0 => 1,
        1 => 1_000,
        2 => 1_000_000,
        3 => 1_000_000_000,
        4 => 60_000_000_000,
        5 => 3_600_000_000_000,
        6 => 86_400_000_000_000,
        _ => 1_000_000,
    }
}

pub fn convert_time_unit_between(value: i64, source_ordinal: i32, target_ordinal: i32) -> i64 {
    let source = time_unit_nanos_per(source_ordinal);
    let target = time_unit_nanos_per(target_ordinal).max(1);
    let converted = (value as i128) * source / target;
    converted.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

pub fn native_timeunit_to_ordinal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    target_ordinal: i32,
) -> MethodCallResult {
    let recv = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let dur = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let source_ordinal = time_unit_ordinal(ctx, recv);
    Ok(Some(Value::Long(convert_time_unit_between(
        dur,
        source_ordinal,
        target_ordinal,
    ))))
}

pub fn native_timeunit_sleep(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let recv = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let dur = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Double(v)) => v.to_bits() as i64,
        Some(Value::Float(v)) => v.to_bits() as i64,
        _ => 0,
    };
    if dur <= 0 {
        return Ok(None);
    }
    let ordinal = time_unit_ordinal(ctx, recv);
    let millis = convert_time_unit_between(dur, ordinal, 2).max(0);
    // JDK 25 `TimeUnit.sleep` is `Thread.sleep(toMillis(t), excessNanos(t,
    // ms))`: a sub-millisecond part is slept, not dropped. This native used
    // to truncate to whole milliseconds, so `NANOSECONDS.sleep(1)` and
    // `MICROSECONDS.sleep(999)` did not sleep at all (interpreter round i1
    // wave 25, lane L7). Whole-millisecond durations keep the millisecond
    // native (and its scheduled-pump slices) unchanged.
    let excess_nanos: i32 = match ordinal {
        0 => (dur % 1_000_000) as i32,
        1 => ((dur % 1_000) * 1_000) as i32,
        _ => 0,
    };
    if excess_nanos == 0 {
        native_thread_sleep(ctx, &[Value::Long(millis)])
    } else {
        crate::lang_system::native_thread_sleep_millis_nanos(
            ctx,
            &[Value::Long(millis), Value::Int(excess_nanos)],
        )
    }
}

pub fn native_timeunit_to_nanos(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_timeunit_to_ordinal(ctx, args, 0)
}

pub fn native_timeunit_to_micros(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_timeunit_to_ordinal(ctx, args, 1)
}

pub fn native_timeunit_to_seconds(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_timeunit_to_ordinal(ctx, args, 3)
}

pub fn native_timeunit_to_minutes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_timeunit_to_ordinal(ctx, args, 4)
}

pub fn native_timeunit_to_hours(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_timeunit_to_ordinal(ctx, args, 5)
}

pub fn native_timeunit_to_days(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_timeunit_to_ordinal(ctx, args, 6)
}

pub fn convert_time_unit_to_millis(value: i64, ordinal: i32) -> i64 {
    // Saturating multiply mirroring TimeUnit.x: clamp to i64::MAX / i64::MIN
    // when the magnitude would overflow.
    fn sat_mul(value: i64, m: i64) -> i64 {
        let over = i64::MAX / m;
        if value > over {
            i64::MAX
        } else if value < -over {
            i64::MIN
        } else {
            value * m
        }
    }
    match ordinal {
        0 => value / 1_000_000,
        1 => value / 1_000,
        2 => value,
        3 => sat_mul(value, 1_000),
        4 => sat_mul(value, 60_000),
        5 => sat_mul(value, 3_600_000),
        6 => sat_mul(value, 86_400_000),
        _ => value,
    }
}

/// Build a `java.util.HashSet` holding `elems`, through the class's own
/// `<init>` and `add` bytecode -- the ONE shape that is correct in both
/// compatibility modes.
///
/// # Why this exists rather than a raw-slot factory
///
/// The shape it replaces allocated two or three slots and wrote an element
/// array at absolute 0, a count at 1 and a capacity at 2. That is the MAP
/// layout, on a class whose one real instance field is
/// `map` (`Ljava/util/HashMap;`):
///
/// * on a real `java.util.HashSet` every `Set` method dereferences `map`, so
///   the object came back with an `Object[]` where a `HashMap` belongs and
///   `size()`/`iterator()`/`contains()` answered for an EMPTY set whatever the
///   array held (the TC0622 defect);
/// * the elements went into the array in insertion order rather than into hash
///   buckets, so the set was findable only by an implementation that agreed to
///   look there;
/// * and it pinned `java/util/HashSet`'s synthetic slot floor at 2-3 against
///   one real field, which pads the class -- and `LinkedHashSet`, which
///   declares none of its own -- out of the compact layout entirely
///   (`ClassStore::build_compact_layout` refuses any padded class, so the
///   padding costs every slot, not just itself).
///
/// Going through `<init>` costs nothing in synthetic-JDK mode: `native_hs_init`
/// allocates the backing map and `hs_set_backing_map` places it at the slot
/// `hs_map_slot` names, which is absolute 0 in both modes.
///
/// GC-safe: the elements are parked in one heap array and the set is pinned
/// across the `<init>` and every `add`, each of which allocates.
pub fn build_real_hash_set(
    ctx: &mut dyn NativeContext,
    elems: &[ObjectRef],
) -> Result<ObjectRef, MethodCallFailed> {
    // Pin the elements BEFORE the first allocation, then park them in one heap
    // array we can re-read across each `add`. `new_ref_array` is itself a
    // collection point, so a bare `elems` slice read after it is the
    // native-stale-local shape; `pin_native_root` allocates nothing, so taking
    // the pins first is free.
    let elem_pins: Vec<usize> = elems.iter().map(|&e| ctx.pin_native_root(e)).collect();
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), elems.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, e) in elems.iter().enumerate() {
        let e = ctx.read_native_pin(elem_pins[i], *e);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(e)));
    }
    // The pin stack unwinds to a BASE, so releasing the earliest handle
    // releases every one taken after it.
    let pin_base = elem_pins.first().copied().unwrap_or(arr_pin);
    // ONE slot requested, which is what the real class declares. The
    // allocator takes `max(n, real)`, so a fabricated 3-slot stub still gets
    // its three.
    let set = match try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 1) {
        Ok(set) => set,
        Err(e) => {
            // Release the region before propagating: a native that returns
            // through `?` with pins still on the stack leaves them there for
            // the rest of the thread's life.
            ctx.unpin_native_roots(pin_base);
            return Err(e);
        }
    };
    let set_pin = ctx.pin_native_root(set);
    let _ = ctx.invoke(
        "java/util/HashSet",
        "<init>",
        "()V",
        &[Value::Object(Some(set))],
    );
    for i in 0..elems.len() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let elem = ctx.get_array_element(arr, i);
        let set = ctx.read_native_pin(set_pin, set);
        let _ = ctx.invoke_virtual(set, "add", "(Ljava/lang/Object;)Z", &[elem]);
    }
    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(pin_base);
    Ok(set)
}

/// Build a `java.util.HashSet` with a real-layout `java.util.HashMap` inside,
/// populated with the supplied String keys (each mapped to the canonical
/// `HashSet.PRESENT` singleton — represented here as the same `Boolean.TRUE`
/// substitute / null sentinel: JDK code only ever does `containsKey`, never
/// reads the value).
///
/// Falls back to [`build_real_hash_set`] -- the class's own `<init>` and `add`
/// -- when the real `HashMap` / `HashMap$Node` field layout cannot be resolved
/// (i.e. classes not yet loaded, or a fabricated stub). It used to fall back to
/// a legacy synthetic 2-field `(data_array, size)` shape instead, which was the
/// last unconditional raw-slot write on `java/util/HashSet` in the tree and the
/// last thing pinning its synthetic slot floor. This matches the pattern S111r7
/// introduced in
/// `lang_system::native_system_getenv_all` so JDK bytecode for
/// `HashSet.spliterator()` (which constructs a `HashMap.KeySpliterator` from
/// the wrapped HashMap and later does `getfield m.table`) reads valid slots
/// instead of seeing the synthetic `(data_array, size)` placement.
///
/// This is a generic collection-layout helper, NOT vendor-specific.
pub fn jdk_string_hash_from_obj(ctx: &mut dyn NativeContext, k: ObjectRef) -> i32 {
    let s = ctx.read_string(k).unwrap_or_default();
    let mut h: i32 = 0;
    for unit in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(i32::from(unit));
    }
    h ^ ((h as u32 >> 16) as i32)
}

pub fn build_real_layout_hashmap_with_pins(
    ctx: &mut dyn NativeContext,
    hashes: &[i32],
    key_pins: &[(usize, ObjectRef)],
    val_pins: Option<&[(usize, ObjectRef)]>,
    default_val: Option<Value>,
) -> Option<ObjectRef> {
    use cratonvm_types::ClassId;

    let hashmap_cid = ctx
        .ensure_class_initialized("java/util/HashMap")
        .unwrap_or(ClassId::new(0));
    let node_cid = ctx
        .ensure_class_initialized("java/util/HashMap$Node")
        .unwrap_or(ClassId::new(0));

    let (
        Some(f_table),
        Some(f_size),
        Some(f_threshold),
        Some(f_loadfactor),
        Some(f_entryset),
        Some(n_hash),
        Some(n_key),
        Some(n_value),
        Some(n_next),
    ) = (
        ctx.resolve_field_index("java/util/HashMap", "table"),
        ctx.resolve_field_index("java/util/HashMap", "size"),
        ctx.resolve_field_index("java/util/HashMap", "threshold"),
        ctx.resolve_field_index("java/util/HashMap", "loadFactor"),
        ctx.resolve_field_index("java/util/HashMap", "entrySet"),
        ctx.resolve_field_index("java/util/HashMap$Node", "hash"),
        ctx.resolve_field_index("java/util/HashMap$Node", "key"),
        ctx.resolve_field_index("java/util/HashMap$Node", "value"),
        ctx.resolve_field_index("java/util/HashMap$Node", "next"),
    )
    else {
        return None;
    };

    let mut cap: usize = 16;
    while cap < (key_pins.len() * 4 / 3 + 1).max(16) {
        cap <<= 1;
    }
    let buckets = ctx.new_ref_array(ClassId::new(0), cap);
    let buckets_pin = ctx.pin_native_root(buckets);

    let map_n_fields = [f_table, f_size, f_threshold, f_loadfactor, f_entryset]
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        + 1;
    let node_n_fields = [n_hash, n_key, n_value, n_next]
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        + 1;
    let map = ctx.alloc_object(hashmap_cid, map_n_fields);
    let map_pin = ctx.pin_native_root(map);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);
    ctx.set_field(map, f_table, Value::Object(Some(buckets)));
    ctx.set_field(map, f_size, Value::Int(key_pins.len() as i32));
    ctx.set_field(map, f_threshold, Value::Int((cap as i32 * 3) / 4));
    ctx.set_field(map, f_loadfactor, Value::Float(0.75));
    ctx.set_field(map, f_entryset, Value::Object(None));

    for (i, &(kpin, kraw)) in key_pins.iter().enumerate() {
        let hash = hashes[i];
        let idx = ((cap as u32 - 1) & hash as u32) as usize;
        let node = ctx.alloc_object(node_cid, node_n_fields);
        ctx.set_field(node, n_hash, Value::Int(hash));
        let k = ctx.read_native_pin(kpin, kraw);
        ctx.set_field(node, n_key, Value::Object(Some(k)));
        let v = if let Some(vps) = val_pins {
            let (vpin, vraw) = vps[i];
            Value::Object(Some(ctx.read_native_pin(vpin, vraw)))
        } else {
            default_val.unwrap_or_else(|| {
                let k_now = ctx.read_native_pin(kpin, kraw);
                Value::Object(Some(k_now))
            })
        };
        ctx.set_field(node, n_value, v);
        let buckets = ctx.read_native_pin(buckets_pin, buckets);
        let existing = ctx.get_array_element(buckets, idx);
        ctx.set_field(node, n_next, existing);
        ctx.set_array_element(buckets, idx, Value::Object(Some(node)));
    }

    let map = ctx.read_native_pin(map_pin, map);
    Some(map)
}

pub fn build_real_layout_hashset_inner(
    ctx: &mut dyn NativeContext,
    keys: &[ObjectRef],
    hashes: Vec<i32>,
) -> Result<ObjectRef, MethodCallFailed> {
    use cratonvm_types::ClassId;

    let key_pins: Vec<(usize, ObjectRef)> = keys
        .iter()
        .map(|&k| (ctx.pin_native_root(k), k))
        .collect();
    let pin_base = key_pins.first().map(|p| p.0);

    let hashset_cid = ctx
        .ensure_class_initialized("java/util/HashSet")
        .unwrap_or(ClassId::new(0));
    let s_map = ctx.resolve_field_index("java/util/HashSet", "map");
    let present = ctx
        .static_field_index_by_name(hashset_cid, "PRESENT")
        .map(|idx| ctx.get_static_field(hashset_cid, idx))
        .filter(|v| matches!(v, Value::Object(Some(_))));

    if let Some(s_map) = s_map {
        if let Some(map) =
            build_real_layout_hashmap_with_pins(ctx, &hashes, &key_pins, None, present)
        {
            let map_pin = ctx.pin_native_root(map);
            let set_n_fields = (s_map + 1).max(ctx.class_num_total_fields(hashset_cid));
            let set = ctx.alloc_object(hashset_cid, set_n_fields);
            let map = ctx.read_native_pin(map_pin, map);
            ctx.set_field(set, s_map, Value::Object(Some(map)));
            ctx.unpin_native_roots(pin_base.unwrap_or(map_pin));
            return Ok(set);
        }
    }

    let current: Vec<ObjectRef> = key_pins
        .iter()
        .map(|&(pin, key)| ctx.read_native_pin(pin, key))
        .collect();
    if let Some(base) = pin_base {
        ctx.unpin_native_roots(base);
    }
    build_real_hash_set(ctx, &current)
}

pub fn build_real_layout_string_hashset(
    ctx: &mut dyn NativeContext,
    keys: &[ObjectRef],
) -> Result<ObjectRef, MethodCallFailed> {
    let hashes: Vec<i32> = keys
        .iter()
        .map(|&k| jdk_string_hash_from_obj(ctx, k))
        .collect();
    build_real_layout_hashset_inner(ctx, keys, hashes)
}

pub fn build_real_layout_identity_hashset(
    ctx: &mut dyn NativeContext,
    keys: &[ObjectRef],
) -> Result<ObjectRef, MethodCallFailed> {
    let hashes: Vec<i32> = keys
        .iter()
        .map(|&k| {
            let h = ctx.identity_hash_code(k);
            h ^ ((h as u32 >> 16) as i32)
        })
        .collect();
    build_real_layout_hashset_inner(ctx, keys, hashes)
}

pub fn build_real_layout_string_hashmap(
    ctx: &mut dyn NativeContext,
    entries: &[(ObjectRef, ObjectRef)],
) -> Result<ObjectRef, MethodCallFailed> {
    let hashes: Vec<i32> = entries
        .iter()
        .map(|&(k, _)| jdk_string_hash_from_obj(ctx, k))
        .collect();
    let mut key_pins = Vec::with_capacity(entries.len());
    let mut val_pins = Vec::with_capacity(entries.len());
    for &(k, v) in entries {
        key_pins.push((ctx.pin_native_root(k), k));
        val_pins.push((ctx.pin_native_root(v), v));
    }
    let pin_base = key_pins.first().map(|p| p.0);

    if let Some(map) =
        build_real_layout_hashmap_with_pins(ctx, &hashes, &key_pins, Some(&val_pins), None)
    {
        if let Some(base) = pin_base {
            ctx.unpin_native_roots(base);
        }
        return Ok(map);
    }

    let current: Vec<(ObjectRef, ObjectRef)> = key_pins
        .iter()
        .zip(val_pins.iter())
        .map(|(&(kp, kr), &(vp, vr))| (ctx.read_native_pin(kp, kr), ctx.read_native_pin(vp, vr)))
        .collect();
    if let Some(base) = pin_base {
        ctx.unpin_native_roots(base);
    }
    let hashmap_cid = ctx.ensure_class_initialized("java/util/HashMap")?;
    let map = ctx.alloc_object(hashmap_cid, ctx.class_num_total_fields(hashmap_cid));
    let map_pin = ctx.pin_native_root(map);
    let _ = ctx.invoke_virtual(map, "<init>", "()V", &[]);
    for (k, v) in current {
        let k_pin = ctx.pin_native_root(k);
        let v_pin = ctx.pin_native_root(v);
        let m = ctx.read_native_pin(map_pin, map);
        let kc = ctx.read_native_pin(k_pin, k);
        let vc = ctx.read_native_pin(v_pin, v);
        let _ = ctx.invoke_virtual(
            m,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(kc)), Value::Object(Some(vc))],
        );
        ctx.unpin_native_roots(k_pin);
    }
    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(map_pin);
    Ok(map)
}


// ---------------------------------------------------------------------------
// T19.H2: SharedSecrets JavaLangAccess shim
// ---------------------------------------------------------------------------
//
// KC16 / WildFly / JBoss Modules surface the NPE
//     "Cannot invoke currentCarrierThread on null"
// at VM boot because `ForkJoinPool` (and friends) fetch the `JavaLangAccess`
// singleton via `SharedSecrets.getJavaLangAccess()` and immediately call
// `jla.currentCarrierThread()` through an `invokeinterface`. In real HotSpot
// the singleton is populated by `System.initPhase1()`, which runs
// `System.setJavaLangAccess()` to register a `java.lang.System$1` that
// implements every `JavaLangAccess` interface method.
//
// Rustjvm does not run `System.initPhase1()` end-to-end, so the static
// `SharedSecrets.javaLangAccess` field is null. We paper over that by:
//   1. Registering a native for `SharedSecrets.getJavaLangAccess()` that
//      allocates and caches a synthetic `java.lang.System$1` object.
//   2. Registering natives for the subset of `JavaLangAccess` methods that
//      `ForkJoinPool` / `Virtual­Thread` actually call during boot —
//      `currentCarrierThread()`, `currentThread0()`, `setCurrentThread()`,
//      `isTerminated`, and the `SharedSecrets.getJavaLangInvokeAccess`
//      companion used by MethodHandles lookup.
//
// NOT cached (gc-followups-20260706 audit): an earlier revision cached the
// shim in a process-wide `OnceLock<ObjectRef>`; the current implementation
// deliberately allocates a fresh `System$1` per call (see the per-thread-heap
// rationale in `get_or_build_jla_shim`), so there is no raw static ref to go
// stale across a moving GC. Object identity is irrelevant for these natives.

/// Build or return the cached System$1 JLA shim object.
///
/// The object has a single instance field (slot 0 reserved for any future
/// back-reference); native methods registered on `java/lang/System$1`
/// read nothing from it.
pub fn get_or_build_jla_shim(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // Per-thread cache: because cratonvm's per-thread heap means a cached
    // ObjectRef from another thread could be stale, we allocate a fresh
    // `System$1` each call. The backing class is shared, so every
    // returned object routes to the same `JavaLangAccess.*` natives —
    // the object identity is irrelevant for these method calls.
    Ok(try_alloc_concurrent_synthetic(
        ctx,
        "java/lang/System$1",
        1,
    )?)
}

/// `jdk/internal/access/SharedSecrets.getJavaLangAccess()` shim.
pub fn native_shared_secrets_get_jla(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let shim = get_or_build_jla_shim(ctx);
    Ok(Some(Value::Object(Some(shim?))))
}

/// `jdk/internal/misc/SharedSecrets.getJavaLangAccess()` (legacy package).
pub fn native_shared_secrets_misc_get_jla(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let shim = get_or_build_jla_shim(ctx);
    Ok(Some(Value::Object(Some(shim?))))
}

/// `java/lang/System$1.currentCarrierThread()` — forwards to the VM's
/// current thread object. Matches real-JDK behavior in
/// platform-thread-only mode (no virtual-thread scheduler yet).
pub fn native_jla_current_carrier_thread(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    // The platform thread underneath: a mounted virtual thread's carrier
    // (the VM's `jdk_continuation` module), else the current thread.
    let thread_obj = ctx.current_carrier_thread_object();
    Ok(Some(Value::Object(Some(thread_obj))))
}

pub fn register_t19_h2_shared_secrets_shim(registry: &mut NativeMethodRegistry) {
    // census-tag: jdk.internal.access.SharedSecrets / JavaLangAccess are
    // VM-internal access bridges → Bridge.
            let ss_new = "jdk/internal/access/SharedSecrets";
    registry.register(
        ss_new,
        "getJavaLangAccess",
        "()Ljdk/internal/access/JavaLangAccess;",
        native_shared_secrets_get_jla, cratonvm_native_api::NativeKind::Bridge,
    );
    let ss_old = "jdk/internal/misc/SharedSecrets";

    // Shim: java.lang.System$1.currentCarrierThread() → Thread.currentThread().
    // `JavaLangAccess` is an interface, and our invokeinterface path dispatches
    // via the concrete receiver class, so this registration on `System$1` is
    // what actually gets hit when `jla.currentCarrierThread()` is invoked on
    // our shim instance.
    let sys1 = "java/lang/System$1";
    registry.register(
        sys1,
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
        native_jla_current_carrier_thread, cratonvm_native_api::NativeKind::Bridge,
    );
    // Same dispatch for `currentThread0` (used internally by
    // VirtualThread.pinnedCheck / blockedOn).
    registry.register(
        sys1,
        "currentThread0",
        "()Ljava/lang/Thread;",
        native_jla_current_carrier_thread, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Thread.currentCarrierThread()` is already registered elsewhere; we
    // re-register here with the same implementation so any duplicate-entry
    // drift keeps both pointing at the same forwarder.
    registry.register(
        "java/lang/Thread",
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
        native_jla_current_carrier_thread,
        NativeKind::Bridge,
    );

    // ---- JavaLangAccess interface receiver-class registrations ----
    //
    // Interface method dispatch resolves through the receiver's concrete
    // class — `System$1` above — but belt-and-suspenders: register on the
    // interface as well so any direct invocation path hits the forwarder.
    // `layers` on the concrete receiver class, for the same reason
    // `currentCarrierThread` is registered there: `invokeinterface` dispatches
    // through `System$1`.
    registry.register(
        sys1,
        "layers",
        "(Ljava/lang/ClassLoader;)Ljava/util/stream/Stream;",
        native_jla_layers, cratonvm_native_api::NativeKind::Bridge,
    );
    let jla = "jdk/internal/access/JavaLangAccess";
    registry.register(
        jla,
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
        native_jla_current_carrier_thread, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        jla,
        "layers",
        "(Ljava/lang/ClassLoader;)Ljava/util/stream/Stream;",
        native_jla_layers, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// `JavaLangAccess.layers(ClassLoader)` — the module layers with at least one
/// module defined to `loader`.
///
/// `java.util.ServiceLoader$ModuleServicesLookupIterator.iteratorFor` calls
/// this for any loader that is neither null nor the platform loader, then walks
/// the result collecting module-declared service providers. Without it
/// `ManagementFactory.getPlatformMBeanServer()` dies with `NoSuchMethodError`
/// the moment strict mode runs its real bytecode.
pub fn native_jla_layers(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // EMPTY, and measured rather than assumed. The alternative,
    // `Stream.of(ModuleLayer.boot())`, was built and run: it reaches the next
    // unimplemented `JavaLangAccess` method
    // (`getServicesCatalog(Ljava/lang/ModuleLayer;)`) and dies there instead.
    // Empty is also the honest answer for this VM -- it defines no NAMED
    // modules to the application class loader, which is the only loader for
    // which `iteratorFor` consults this method at all.
    ctx.invoke(
        "java/util/stream/Stream",
        "empty",
        "()Ljava/util/stream/Stream;",
        &[],
    )
}

pub fn build_synthetic_module_descriptor(
    ctx: &mut dyn NativeContext,
    module_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Was: `requires`/`exports`/`opens`/`provides`/`packages` fabricated as
    // EMPTY sets unconditionally — for every module, in every mode — even
    // though the VM had already parsed the real `module-info.class` into
    // `ClassManager::module_registry` (`classloading::module::parse_module_info`
    // + `resolve_module_path`). `getDescriptor().exports()` therefore answered
    // `[]` for a module that declares two exports
    // (`regression-suite/src/RJdkModule.java:69` vs HotSpot 25).
    //
    // The single implementation now lives beside the rest of the
    // `java.lang.Module` / `ModuleLayer` surface and answers from that
    // registry. A module the registry does NOT know still gets the all-empty
    // shape this used to hand everyone, so synthetic-jdk and any embedder with
    // an unpopulated registry are unchanged.
    crate::jboss_jdkspecific::build_module_descriptor(ctx, module_name)
}

// ---------------------------------------------------------------------------
// T19.H2: MethodHandles.Lookup.<clinit> dependency natives
// ---------------------------------------------------------------------------
//
// Real HotSpot's `MethodHandles$Lookup.<clinit>`:
//   0. call `Class.desiredAssertionStatus()` → putstatic `$assertionsDisabled`
//   1. call `Set.of("lookupClass", "allowedModes")` → pass to Reflection
//   2. call `jdk.internal.reflect.Reflection.registerFieldsToFilter(Class, Set)`
//   3. call `ClassFileDumper.getInstance("jdk.invoke.MethodHandle.dumpClassFiles",
//                                        "DUMP_CLASS_FILES")` → putstatic
//   4. call `MethodHandles.IMPL_NAMES.getClass()` (no-op pop — just checking)
//   5. `new Lookup(Object.class, null, -1)` → putstatic IMPL_LOOKUP
//   6. `new Lookup(Object.class, null, 32)` → putstatic PUBLIC_LOOKUP
//   7. `new ConcurrentHashMap()` → putstatic LOOKASIDE_TABLE
//
// Steps 2 and 3 were unregistered in cratonvm, which triggered the NPE
// on step 1's Set.of(Obj,Obj) returning null (because the 2-arg Set.of
// overload was missing from `phases_early.rs`).
//
// This function registered `Set.of` overloads for 2..=8 arguments (since
// RETIRED 2026-08-12 — real bytecode now runs) and still registers
// `ClassFileDumper.getInstance(String, String)`, returning a disabled dumper
// object (so downstream `dumper.isEnabled()` returns false).
// `Reflection.registerFieldsToFilter(Class, Set)` (step 2) is deliberately
// UNREGISTERED — see the comment where it used to be shadowed, below.

// `build_hashset_from_args` was removed with the ten `Set.of` intrinsics it
// served (see the retirement note below). Its one remaining caller would have
// been that block; `cratonvm_native_collections::make_hashset_with_elements`
// is still the helper for anything that genuinely wants a mutable HashSet.

pub fn register_t19_h2_lookup_clinit_deps(registry: &mut NativeMethodRegistry) {
    // census-tag: faithful Set.of fixed-arity factories — spec-exact immutable
    // collections replicating real JDK bytecode → Intrinsic.
            // --- Set.of(Object, Object) through Set.of(Object x 10) ---
    //
    // JDK 9+ `Set.of` has fixed-arity overloads for 0..=10 arguments plus
    // a varargs fallback. Our existing `phases_early::register_core_stdlib_extras`
    // only covers the 0-arg and varargs forms; we fill in 2..=10 here
    // because `MethodHandles$Lookup.<clinit>` needs 2 (lookupClass,
    // allowedModes) and `ClassFileDumper.<clinit>` needs 8 (BAD_CHARS).
    // RETIRED 2026-08-12 — the ten fixed-arity `java/util/Set.of` overloads
    // that stood here, tagged `Intrinsic`.
    //
    // The census tag above them read "faithful Set.of fixed-arity factories —
    // spec-exact immutable collections replicating real JDK bytecode →
    // Intrinsic". Every body was
    // `build_hashset_from_args` → `make_hashset_with_elements`, i.e. a plain
    // MUTABLE `java.util.HashSet`. Not spec-exact, not immutable, and not what
    // `Set.of` returns; a synthetic stub wearing the one tag strict mode is
    // contractually required to keep (jdk-only-mode §1.4: concrete bytecode
    // wins over any native "except for a reviewed `NativeKind::Intrinsic`").
    //
    // It never dispatched in Compatible mode. `register_factory_natives`
    // (`native-collections`) registers the same ten triples later and
    // `register()` is last-write-wins, so the census records these ten as
    // `owns_slot: false, overwrote: intrinsic` — superseded, unreachable.
    // Removing them is therefore byte-for-byte neutral there, which is what
    // contract §5/§10 requires.
    //
    // Under `--real-jdk` they were live and wrong. The later stub is dropped at
    // registration, so the slot fell back to THESE — the refusal laundered into
    // a worse answer than the one it refused (the W7-20 shape, at the kind
    // level). Measured on JDK 25.0.4/linux with
    // `probes/ImmutableCollectionsDifferentialProbe`:
    //
    //                      HotSpot                          --real-jdk (before)
    //   Set.of("x")        java.util.ImmutableCollections$Set12   java.util.HashSet
    //   .add(...)          UnsupportedOperationException          NO THROW, size 2
    //
    // A `Set.of` result that accepts `add` is not a nearly-right immutable set,
    // it is a mutable one. With nothing registered, real `java.util.Set.of`
    // bytecode runs and answers exactly as HotSpot does.
    //
    // The comment that justified them said `MethodHandles$Lookup.<clinit>`
    // needs the 2-arg form and `ClassFileDumper.<clinit>` the 8-arg one. That
    // was a real boot dependency when it was written and is discharged by
    // running the real bytecode: verified by booting `--real-jdk` and
    // `--fast-boot` and by the regression corpus in both modes.
    //
    // Sibling population, same shape, NOT taken here because it is unmeasured:
    // six `java/util/logging/Handler` triples where a `phases_early.rs`
    // `Intrinsic` is superseded by a `reflect_annotations.rs` `SyntheticStub`.
    // Those sixteen triples are the whole intrinsic-superseded-by-stub set in
    // the schema-4 census.

    // `Reflection.registerFieldsToFilter`/`registerMethodsToFilter` are
    // deliberately NOT shadowed here any more. Neither is a native method in
    // the real JDK, and shadowing them stops the real `<clinit>` call sites
    // that populate `Reflection.fieldFilterMap`/`methodFilterMap` (their own
    // hardcoded base PLUS every class that calls the public API on itself,
    // e.g. `MethodHandles$Lookup` for `lookupClass`/`allowedModes`) from ever
    // reaching those fields. Letting the real bytecode run and reading the
    // fields back at query time (`reflect_filter.rs`, consulted by
    // `native_class_get_declared_field(s)`/`native_class_get_declared_method(s)`
    // in `lang_class.rs`) reproduces HotSpot's set with no hand-written list
    // — see core-reflection-has-no-member-filter-20260911.md.
    let refl = "jdk/internal/reflect/Reflection";

    // --- Reflection.areNestMates(Class, Class) ---
    //
    // JEP 181 nestmate access check, exposed to library code (e.g. JDK
    // serialization's `ObjectStreamClass` privileged-lookup path, which
    // Spring's `beanProviderSerialization` test exercises via
    // `ObjectInputStream`). Real semantics
    // (`Reflection.areNestMates` -> `Class.isNestmateOf`): identical
    // classes are always nestmates; otherwise two classes are nestmates
    // iff they resolve to the same nest host. Delegate to
    // `native_class_get_nest_host` (already used by `Class.getNestHost()`)
    // for host resolution so both entry points agree on a class's host,
    // including the lambda-proxy special case it already handles.
    registry.register(
        refl,
        "areNestMates",
        "(Ljava/lang/Class;Ljava/lang/Class;)Z",
        |ctx, args| {
            let a = obj_arg(args, 0)?;
            let b = obj_arg(args, 1)?;
            if a == b {
                return Ok(Some(Value::Int(1)));
            }
            // Each host resolution may run `loadClass` (a GC point): `b` is
            // pinned across the first, and each host is reduced to its class
            // id at once rather than kept as a reference across the other
            // (JIT round 20 wave 16, lane nest).
            let b_pin = ctx.pin_native_root(b);
            let host_a =
                crate::lang_class::native_class_get_nest_host(ctx, &[Value::Object(Some(a))]);
            let id_a = match host_a {
                Ok(Some(Value::Object(Some(h)))) => crate::lang_class::mirror_class_id(ctx, h),
                Ok(_) => None,
                Err(e) => {
                    ctx.unpin_native_roots(b_pin);
                    return Err(e);
                }
            };
            let b = ctx.read_native_pin(b_pin, b);
            ctx.unpin_native_roots(b_pin);
            let id_b = match crate::lang_class::native_class_get_nest_host(
                ctx,
                &[Value::Object(Some(b))],
            )? {
                Some(Value::Object(Some(h))) => crate::lang_class::mirror_class_id(ctx, h),
                _ => None,
            };
            let same = match (id_a, id_b) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            };
            Ok(Some(Value::Int(if same { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- ClassFileDumper.getInstance(String, String) ---
    //
    // Real HotSpot uses this to dump generated classfiles to disk when
    // `-Djdk.invoke.MethodHandle.dumpClassFiles` is set. We don't
    // generate classfiles (IBG is bypassed — see lang_invoke C33), so
    // we return a disabled dumper whose `isEnabled()` and `dumpClass`
    // natives are no-ops. The dumper's field slots are laid out to
    // match real JDK's ClassFileDumper:
    //     slot 0: key (String)
    //     slot 1: dumpDir (String)
    //     slot 2: enabled (Int boolean)
    //     slot 3: counter (AtomicInteger / null)
    let dumper = "jdk/internal/util/ClassFileDumper";
    registry.register(
        dumper,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljdk/internal/util/ClassFileDumper;",
        |ctx, args| {
            // PIN THE ARGUMENTS ACROSS THE ALLOCATION. `args` is a snapshot of
            // the operand stack taken before this native was entered, and
            // `try_alloc_concurrent_synthetic` can run a moving young
            // collection -- after which the two `String`s in it name the
            // addresses the collector moved them away from. Storing those into
            // the dumper's fields puts a dead address inside a LIVE object,
            // where no frame remap reaches it and the allocator re-serves it
            // to something else. Same shape as `URL.openConnection`; see
            // `docs/feature-designs/default-moving-young-gen.md`.
            let key = args.first().copied().unwrap_or(Value::Object(None));
            let dir = args.get(1).copied().unwrap_or(Value::Object(None));
            let p_key = match key {
                Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
                _ => None,
            };
            let p_dir = match dir {
                Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
                _ => None,
            };
            // Both pins are released before returning, the error arm too:
            // they were left for the funnel to truncate, the `[pin-balance]
            // LEAK callee=<cb@..> left 2 pin(s)` every debug run printed at
            // start-up and at its first `findStatic` (interpreter round i2
            // wave 61, lane L4). The lower pin's truncation releases both.
            let pin_floor = p_key.or(p_dir).map(|(h, _)| h);
            let d =
                match try_alloc_concurrent_synthetic(ctx, "jdk/internal/util/ClassFileDumper", 4) {
                    Ok(d) => d,
                    Err(e) => {
                        if let Some(floor) = pin_floor {
                            ctx.unpin_native_roots(floor);
                        }
                        return Err(e);
                    }
                };
            let key = match p_key {
                Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
                None => key,
            };
            let dir = match p_dir {
                Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
                None => dir,
            };
            if let Some(floor) = pin_floor {
                ctx.unpin_native_roots(floor);
            }
            ctx.set_field(d, 0, key);
            ctx.set_field(d, 1, dir);
            ctx.set_field(d, 2, Value::Int(0)); // disabled
            ctx.set_field(d, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(d))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // KEEP: correct constant, and the two dump entry points below are only
    // reachable when it is true. Class-file dumping is a HotSpot diagnostic
    // (-Djdk.invoke.LambdaMetafactory.dumpProxyClassFiles et al.) that
    // CratonVM does not implement, so "disabled" is the honest answer;
    // `getInstance` above stamps the same 0 into the instance's slot 2.
    registry.register(dumper, "isEnabled", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        dumper,
        "dumpClass",
        "(Ljava/lang/String;Ljava/lang/Class;[B)V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        dumper,
        "dumpFailedClass",
        "(Ljava/lang/String;[B)V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(dumper, "key", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(this, 0)))    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

// LinkedBlockingQueue: add element at tail, grow array if needed.
//
// gc-common w21-a: answers the queue's CURRENT address. The grow path
// allocates, and every caller (`util_concurrent_ext.rs`'s LBQ / LTQ
// `put` / `offer` / `add` / `transfer` / `tryTransfer`) went on to
// `monitor_notify_all` / `monitor_exit` / `get_field` through the address it
// passed in -- the pre-allocation one (lesson (ii) of the round: a helper that
// allocates and takes the object by value hides its GC point from the caller).
#[cfg(feature = "synthetic-jdk")]
#[must_use = "the queue may have moved: use the returned address"]
pub fn m18_lbq_add_internal(
    ctx: &mut dyn NativeContext,
    mut this: ObjectRef,
    elem: Value,
) -> ObjectRef {
    let size = match ctx.get_field(this, 1) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return this,
    };
    let cap = ctx.array_length(arr);
    if size >= cap {
        let new_cap = (cap * 2).max(16);
        // GC: the grow path ALLOCATES, and everything it then touches is a Rust
        // local holding a pre-allocation address — the old array it copies from,
        // the element it stores, and the receiver it publishes into. Under a
        // moving collector those go stale; under the Generational non-moving young
        // sweep an object nothing else roots is ZEROED in place. Root them for the
        // duration of the grow and re-read each one at its use. See
        // `internal/fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let arr_h = scope.root(arr);
        let elem_h = match elem {
            Value::Object(Some(o)) => Some(scope.root(o)),
            _ => None,
        };
        let new_arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        let new_h = scope.root(new_arr);
        for i in 0..size {
            let src = scope.get(&arr_h);
            let v = scope.get_array_element(src, i);
            let dst = scope.get(&new_h);
            scope.set_array_element(dst, i, v);
        }
        let elem_now = match &elem_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => elem,
        };
        let dst = scope.get(&new_h);
        scope.set_array_element(dst, size, elem_now);
        let (recv, dst) = (scope.get(&this_h), scope.get(&new_h));
        scope.set_field(recv, 0, Value::Object(Some(dst)));
        // Carry the refreshed receiver out: the store below runs after the
        // scope closes, and `this` named a pre-allocation address until now.
        this = scope.get(&this_h);
    } else {
        ctx.set_array_element(arr, size, elem);
    }
    ctx.set_field(this, 1, Value::Int((size + 1) as i32));
    this
}

// LinkedBlockingQueue: remove and return head element, shift remaining
#[cfg(feature = "synthetic-jdk")]
pub fn m18_lbq_remove_head(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let size = match ctx.get_field(this, 1) {
        Value::Int(n) => n as usize,
        _ => return Value::Object(None),
    };
    if size == 0 {
        return Value::Object(None);
    }
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    let head = ctx.get_array_element(arr, 0);
    for i in 1..size {
        ctx.set_array_element(arr, i - 1, ctx.get_array_element(arr, i));
    }
    ctx.set_array_element(arr, size - 1, Value::Object(None));
    ctx.set_field(this, 1, Value::Int((size - 1) as i32));
    head
}

// ArrayBlockingQueue: remove and return head element from circular buffer
#[cfg(feature = "synthetic-jdk")]
pub fn m18_abq_remove_head(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let size = match ctx.get_field(this, 1) {
        Value::Int(n) => n,
        _ => return Value::Object(None),
    };
    if size == 0 {
        return Value::Object(None);
    }
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    let cap = ctx.array_length(arr) as i32;
    let head = match ctx.get_field(this, 2) {
        Value::Int(n) => n,
        _ => 0,
    };
    let result = ctx.get_array_element(arr, head as usize);
    ctx.set_array_element(arr, head as usize, Value::Object(None));
    ctx.set_field(this, 1, Value::Int(size - 1));
    ctx.set_field(this, 2, Value::Int((head + 1) % cap));
    result
}

/// Helper: create a ConcurrentSkipListMap containing entries in [from, to) range
pub fn cslm_subrange(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    from: Option<Value>,
    to: Option<Value>,
) -> MethodCallResult {
    let size = match ctx.get_field(source, 2) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let keys_arr = match ctx.get_field(source, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let vals_arr = match ctx.get_field(source, 1) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };

    // gc-common w20-c: the three allocations and every `compareTo` (Java) are
    // GC points, and the source arrays, the new map, its arrays, the bound
    // keys and each key were all used raw after them. Each lives in a scope
    // handle and is read back at its use.
    let mut scope = NativeHandleScope::new(ctx);
    let keys_h = scope.root(keys_arr);
    let vals_h = scope.root(vals_arr);
    let from_h = match from {
        Some(Value::Object(Some(o))) => Some(scope.root(o)),
        _ => None,
    };
    let to_h = match to {
        Some(Value::Object(Some(o))) => Some(scope.root(o)),
        _ => None,
    };
    /// A range bound, re-read from its handle when it is an object.
    fn bound(
        scope: &NativeHandleScope<'_>,
        v: Option<Value>,
        h: &Option<cratonvm_native_api::NativeHandle>,
    ) -> Option<Value> {
        match (v, h) {
            (Some(Value::Object(Some(_))), Some(h)) => Some(Value::Object(Some(scope.get(h)))),
            (v, _) => v,
        }
    }

    // Build a new map with the subrange
    let result = try_alloc_concurrent_synthetic(
        &mut *scope,
        "java/util/concurrent/ConcurrentSkipListMap",
        3,
    )?;
    let result_h = scope.root(result);
    let cap = size.max(4);
    let new_keys = scope.new_array(cratonvm_types::ArrayElementType::Reference, cap);
    let new_keys_h = scope.root(new_keys);
    let new_vals = scope.new_array(cratonvm_types::ArrayElementType::Reference, cap);
    let new_vals_h = scope.root(new_vals);
    let (result, new_keys) = (scope.get(&result_h), scope.get(&new_keys_h));
    scope.set_field(result, 0, Value::Object(Some(new_keys)));
    scope.set_field(result, 1, Value::Object(Some(new_vals)));

    let mut count = 0usize;
    for i in 0..size {
        let keys_now = scope.get(&keys_h);
        let k = scope.get_array_element(keys_now, i);
        let mut k_h = None;
        if let Value::Object(Some(ek)) = k {
            let ek_h = scope.root(ek);
            // Check lower bound
            if let Some(from_key) = bound(&scope, from, &from_h) {
                let ek = scope.get(&ek_h);
                if let Ok(Some(Value::Int(cmp))) =
                    scope.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[from_key])
                {
                    if cmp < 0 {
                        continue;
                    }
                }
            }
            // Check upper bound
            if let Some(to_key) = bound(&scope, to, &to_h) {
                let ek = scope.get(&ek_h);
                if let Ok(Some(Value::Int(cmp))) =
                    scope.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[to_key])
                {
                    if cmp >= 0 {
                        break;
                    }
                }
            }
            k_h = Some(ek_h);
        }
        let k = match &k_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => k,
        };
        let new_keys = scope.get(&new_keys_h);
        scope.set_array_element(new_keys, count, k);
        let vals_now = scope.get(&vals_h);
        let v = scope.get_array_element(vals_now, i);
        let new_vals = scope.get(&new_vals_h);
        scope.set_array_element(new_vals, count, v);
        count += 1;
    }
    let result = scope.get(&result_h);
    scope.set_field(result, 2, Value::Int(count as i32));
    Ok(Some(Value::Object(Some(result))))
}

/// Release a `ReentrantLock` for a thread that is about to `await()` on one
/// of its conditions. Returns the saved hold count to restore on re-acquire,
/// or `None` if the caller does not own the lock.
pub fn rl_release_for_await(key: RlKey, tid: i64) -> Option<i32> {
    rl_with(key, |st| {
        if st.owner != tid {
            return None;
        }
        let saved = if st.hold <= 0 { 1 } else { st.hold };
        st.owner = RL_UNOWNED;
        st.hold = 0;
        Some(saved)
    })
}

/// `monitor_wait` while keeping `obj` valid across the wait, for a native that
/// entered `obj`'s monitor itself (no Java `synchronized` block around it):
/// the monitor is exited on BOTH arms, at the post-wait address, and only then
/// is the wait's error returned.
///
/// `monitor_wait` parks this thread — a GC safepoint — so a relocating
/// collection (the moving collector, or the non-moving young sweep's selective
/// promotion) can move `obj` while we are blocked, leaving a raw `ObjectRef`
/// stale and the following `monitor_exit`/`get_field` doing `header_of` a dead
/// address → EXCEPTION_ACCESS_VIOLATION. Pin `obj` as a native root across the
/// wait (the collector remaps `native_pin_roots`, see vm/src/memory/gc.rs
/// `update_all_roots`) and use its post-GC address; unpin on both arms. (This
/// was `monitor_wait_keepalive`, which left the exit to its callers.)
///
/// Interpreter round i1 wave 25, lane L7 (page
/// `interpreter-L7-synthetic-latch-natives-keep-the-monitor-after-an-interrupted-wait`):
/// `monitor_wait` answers an interrupt with `Err` while the caller still owns
/// the monitor (the entry check throws before the wait releases anything, and
/// an interrupted wait re-acquires before it throws, as `Object.wait` must).
/// The synthetic `CountDownLatch.await` / `LinkedBlockingQueue.take` natives
/// wrote `this = monitor_wait_keepalive(..)?; ctx.monitor_exit(this);`, so an
/// interrupted waiter kept the latch's monitor and the next `countDown`
/// blocked forever; the `Semaphore` natives exited on both arms but at the
/// PRE-wait address, stale if the wait spanned a relocating collection.
///
/// Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`): the
/// wait says what JDK code it stands for (`NativeContext::monitor_wait_as`;
/// it was a plain `monitor_wait`). Every caller is a served stand-in for an
/// AQS acquire (`CountDownLatch.await`, a blocking queue's `take` / timed
/// `poll`, `Semaphore.acquire` / timed `tryAcquire`): `BlockKind::Parked`, or
/// `BlockKind::TimedParked` when the Java call has a deadline, so the waiter
/// reads WAITING / TIMED_WAITING and JVMTI `PARKED`, as on HotSpot, and not
/// the WAITING / `IN_OBJECT_WAIT` of the monitor wait it polls with.
pub fn monitor_wait_keepalive_and_exit_as(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    timeout_ms: Option<u64>,
    kind: cratonvm_native_api::BlockKind,
) -> Result<ObjectRef, MethodCallFailed> {
    let pin = ctx.pin_native_root(obj);
    let wr = ctx.monitor_wait_as(obj, timeout_ms, kind);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    ctx.monitor_exit(obj);
    wr?;
    Ok(obj)
}

/// STW-TAKEOVER-FIX (2026-07-13): use `ctx.monitor_enter_gc_safe(obj)`
/// (see `NativeContext::monitor_enter_gc_safe`'s doc) instead of a raw
/// `ctx.monitor_enter(obj)` in any NEW native whose contended wait must be
/// excused from an in-flight STW barrier pause AND that keeps referencing
/// `obj` afterward — it pins, enters, and refreshes internally, returning
/// the current (possibly GC-relocated) reference. Applied narrowly to the
/// one call site with live-gdb-confirmed evidence of the WildFly
/// `parallel-extension-add` deadlock:
/// `native_cdl_await`/`native_cdl_await_timeout`/`native_cdl_count_down`
/// below. Left as a doc pointer (not a helper function) since the trait
/// method already does the whole job — no wrapper needed.
pub fn bounded_monitor_wait_ms(remaining: std::time::Duration, cap_ms: u64) -> u64 {
    remaining.as_millis().clamp(1, cap_ms as u128) as u64
}

/// Shared core for timed `Condition` waits (`await(time,unit)` and
/// `awaitUntil(Date)`): release the owning lock, monitor-wait up to
/// `timeout_ms`, then re-acquire. Returns Z — 1 if signaled before the
/// deadline, 0 if the wait timed out.
pub fn cond_await_millis(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    timeout_ms: u64,
) -> MethodCallResult {
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    // FELIX FIX (lost-wakeup race): hold the condition monitor across the
    // lock release — see `native_cond_await` for the full rationale.
    ctx.monitor_enter(this);
    let saved_hold = match rl_release_for_await(lock_key, tid) {
        Some(h) => h,
        None => {
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "current thread is not owner".to_string(),
            }
            .into());
        }
    };
    ctx.monitor_enter(lock_ref);
    ctx.monitor_notify(lock_ref)?;
    ctx.monitor_exit(lock_ref);
    let start = std::time::Instant::now();
    // GC-SAFEPOINT FIX (see native_cond_await): pin `this`/`lock_ref` across
    // the blocking wait so a relocating GC can't leave them stale.
    let this_pin = ctx.pin_native_root(this);
    let lock_pin = ctx.pin_native_root(lock_ref);
    let wr = ctx.monitor_wait(this, Some(timeout_ms));
    let this = ctx.read_native_pin(this_pin, this);
    let lock_ref = ctx.read_native_pin(lock_pin, lock_ref);
    ctx.unpin_native_roots(this_pin);
    ctx.monitor_exit(this);
    let timed_out = start.elapsed().as_millis() as u64 >= timeout_ms;
    // Condition contract: re-acquire the lock before returning, even on
    // interrupt (see `native_cond_await`).
    reacquire_lock_after_await(ctx, lock_ref, lock_key, tid, saved_hold)?;
    wr?;
    Ok(Some(Value::Int(if timed_out { 0 } else { 1 })))
}

/// Re-acquire a `ReentrantLock` after a condition `await()` returns,
/// restoring the saved reentrant hold count. Spins (with a short monitor
/// wait) until the lock is free.
pub fn reacquire_lock_after_await(
    ctx: &mut dyn NativeContext,
    mut lock_ref: ObjectRef,
    lock_key: RlKey,
    tid: i64,
    saved_hold: i32,
) -> MethodCallResult {
    loop {
        let claimed = rl_with(lock_key, |st| {
            if st.owner == RL_UNOWNED || st.owner == tid {
                st.owner = tid;
                st.hold = saved_hold;
                true
            } else {
                false
            }
        });
        if claimed {
            return Ok(None);
        }
        // GC-SAFEPOINT FIX (see native_cond_await): the 5 ms re-check wait
        // parks this thread, so a relocating GC could leave `lock_ref` stale.
        // Pin it across the wait and read back the post-GC address; unpin even
        // on the error path.
        let pin = ctx.pin_native_root(lock_ref);
        ctx.monitor_enter(lock_ref);
        let wr = ctx.monitor_wait(lock_ref, Some(5));
        lock_ref = ctx.read_native_pin(pin, lock_ref);
        ctx.monitor_exit(lock_ref);
        ctx.unpin_native_roots(pin);
        // UNINTERRUPTIBLE re-acquire (Condition contract): a pending interrupt
        // must NOT abandon the lock re-acquire — the caller observes the
        // interrupt via the registry flag after `await` returns. On interrupt
        // the monitor wait returns immediately, so throttle the retry to avoid
        // a hot spin while another thread still holds the lock.
        if wr.is_err() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

// --- CountDownLatch ---

// The real `java.util.concurrent.CountDownLatch` has a single field,
// `sync: Ljava/util/concurrent/CountDownLatch$Sync;` — a REFERENCE slot. The
// synthetic natives below own every public entry point (init/countDown/await/
// getCount/toString), so the slot is exclusively theirs — but a bare
// `set_field(this, 0, Int(count))` is silently coerced to `Object(None)` by
// the descriptor-aware write path (`coerce_field_value_by_descriptor` maps a
// primitive written to an `L` slot to null; same trap as the StringWriter
// count, see native-io). Every later read then sees `Object(None)` → count 0 →
// `await()` returns immediately and `countDown()` no-ops. Visible symptom:
// WildFly's subsystem-test `waitForSetup()` fell through before the boot
// thread assigned `bootSuccess`, so `isSuccessfulBoot()` read false and the
// test failed with "Subsystem boot failed!" while boot later succeeded.
// Store the count in an int[] holder instead — an object reference
// matches the declared slot type, survives the coercion, and is traced and
// relocated by the GC.
pub fn cdl_count(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    match ctx.get_field(this, CDL_FIELD_COUNT) {
        Value::Object(Some(holder)) => match ctx.get_array_element(holder, 0) {
            Value::Int(v) => v,
            _ => 0,
        },
        // Legacy synthetic objects (alloc_concurrent_synthetic, no real class
        // layout → no descriptor → the raw Int write survives).
        Value::Int(v) => v,
        _ => 0,
    }
}

pub static NEXT_CDL_WAIT_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_cdl_wait_key() -> i32 {
    loop {
        let candidate =
            NEXT_CDL_WAIT_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & i32::MAX as u64;
        if candidate != 0 {
            return candidate as i32;
        }
    }
}

/// Stable VM-local condition key stored alongside the latch count. It moves
/// with the holder array, unlike a raw ObjectRef address, and therefore stays
/// valid while an unmounted continuation is visible to a moving collector.
pub fn cdl_wait_key(ctx: &mut dyn NativeContext, this: ObjectRef) -> u64 {
    if let Value::Object(Some(holder)) = ctx.get_field(this, CDL_FIELD_COUNT) {
        if let Value::Int(key) = ctx.get_array_element(holder, 1) {
            if key > 0 {
                return key as u64;
            }
        }
        let key = next_cdl_wait_key();
        ctx.set_array_element(holder, 1, Value::Int(key));
        if let Value::Int(stored) = ctx.get_array_element(holder, 1) {
            if stored > 0 {
                return stored as u64;
            }
        }
    }
    // Compatibility for a legacy one-slot holder (or a raw-Int legacy
    // synthetic with no holder). New latch instances always take the unique
    // holder-key path above.
    //
    // gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
    // rank 37): this was the identity hash, which is move-stable but not
    // unique -- two live latches share one after the heap's counter wraps, and
    // a positive hash also lies in the holder keys' range (1..=i32::MAX), so a
    // legacy latch could share its wait key with a holder latch too. Either
    // way one latch's `countDown` woke the other's parked virtual threads (a
    // spurious wakeup; they re-check the count). The latch's weak lock key is
    // unique per live object and move-stable, and bit 63 keeps it out of the
    // holder range (the key's low 32 bits are a registry-wide generation,
    // which already tells live keys apart); the latch is alive while any
    // thread waits on it, so its key is not freed under a waiter.
    const LEGACY_WAIT_KEY_TAG: u64 = 1 << 63;
    LEGACY_WAIT_KEY_TAG | lib_side_key(ctx, this) as u64
}

pub fn cdl_set_count(ctx: &mut dyn NativeContext, this: ObjectRef, count: i32) {
    if let Value::Object(Some(holder)) = ctx.get_field(this, CDL_FIELD_COUNT) {
        ctx.set_array_element(holder, 0, Value::Int(count));
        return;
    }
    // No holder yet (or a legacy Int slot): install one. The raw Int fallback
    // path still works for synthetic allocations where set_field is uncoerced,
    // but route everything through the holder for uniformity. Pin `this`
    // across the allocation — a moving GC during `new_array` would relocate
    // the receiver and leave the Rust-local copy stale.
    let this_pin = ctx.pin_native_root(this);
    let holder = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_array_element(holder, 0, Value::Int(count));
    ctx.set_array_element(holder, 1, Value::Int(next_cdl_wait_key()));
    ctx.set_field(this, CDL_FIELD_COUNT, Value::Object(Some(holder)));
}

// --- Semaphore ---

// The real `java.util.concurrent.Semaphore` has a single field
// (`sync: Ljava/util/concurrent/Semaphore$Sync;` — a REFERENCE slot). Writing
// the permit count as a bare Int into slot 0 is silently coerced to null by
// the descriptor-aware `set_field` path (`coerce_field_value_by_descriptor`
// maps a primitive written to an `L` slot to null), and the FAIR flag write
// to slot 1 falls past the real 1-field layout entirely (dropped by the OOB
// guard). Every later read then saw count 0: `availablePermits()` lied,
// `acquire()` blocked forever, `release()` no-op'd. Same bug shape as the
// CountDownLatch count above — keep both ints in an int[2] holder
// ([0]=permits, [1]=fair) stored in slot 0: an object reference matches the
// declared slot type, survives the coercion, and is traced/relocated by the
// GC. The raw-Int fallback covers legacy synthetic allocations
// (alloc_concurrent_synthetic — no real layout, so the Int writes survive).
// A holder-less synthetic's state lives in `util_concurrent_ext`'s
// `sem_states_by_obj`, keyed through the lock-key registry (gc-common w16-b).
#[derive(Clone, Copy)]
pub struct SemState {
    pub permits: i32,
    pub fair: i32,
}

// --- CyclicBarrier ---

// Same descriptor-coercion trap as Semaphore/CountDownLatch above: the real
// `java.util.concurrent.CyclicBarrier` layout is lock(0,L), trip(1,L),
// parties(2,I), barrierCommand(3,L), generation(4,L), count(5,I) — so a
// synthetic Int write to slot 0/1 is coerced to null and a "broken" write to
// slot 2 lands in the REAL `parties` int. All the state therefore lives in a
// holder object parked in slot 0, which is an `L` slot in the real layout (an
// object survives there) and an untyped slot in the synthetic stub.
//
// HOLDER LAYOUT — receiver slot 0 holds a REFERENCE array of length 2:
//
//     [0] = long[4] { parties, count, generation, broken_gen }
//     [1] = the barrier action `Runnable`, or null
//
// The nesting exists so the action can be stored **without** punning a second
// slot. It used to be a bare `int[3]` and the two-arg constructor threw the
// `Runnable` away with a "Simplified: ignore the barrier action" comment, so
// `new CyclicBarrier(2, action)` ran the action zero times where HotSpot runs
// it once per trip. Parking a reference in another declared-`int` slot to fix
// that is exactly the shape the GC's W7-84 guard reports and auto-boxes; one
// reference array holding one primitive array and one `Runnable` is type-clean
// on both sides and costs one extra indirection per access.
//
// `generation`/`broken_gen` are `long` and not `int` so that the
// "is this generation broken" comparison below can never alias by wraparound.
// Raw-Int fallback covers legacy synthetic allocations whose slots were
// written before any native ran.

/// Holder-array indices (NOT receiver object slots).
pub const CB_HOLDER_STATE: usize = 0;
pub const CB_HOLDER_ACTION: usize = 1;
pub const CB_HOLDER_LEN: usize = 2;

/// `long[4]` state indices (NOT receiver object slots).
pub const CB_H_PARTIES: usize = 0;
pub const CB_H_COUNT: usize = 1;
/// Trip counter. A waiting party is released iff this has moved past the value
/// it read on arrival — which is what makes the barrier CYCLIC. The previous
/// "released iff `count` is back to 0" test was only valid while nobody
/// re-entered the barrier: a released waiter preempted before it re-read `count`
/// would find a faster party had already bumped it to 1.., conclude it had NOT
/// been released, and wait again with its wake-up already spent. That left the
/// barrier permanently one party short and deadlocked every later trip (100%
/// reproducible with 4 parties over 20 rounds, JIT on and `--nojit`).
pub const CB_H_GENERATION: usize = 2;
/// The generation that was broken, or [`CB_NO_BREAK`]. Per-generation, not a
/// flag: `reset()` has to break the generation its parked parties are waiting
/// in — they must wake with `BrokenBarrierException` — while leaving the FRESH
/// generation unbroken, so `isBroken()` reads false immediately afterwards.
pub const CB_H_BROKEN_GEN: usize = 3;
pub const CB_STATE_LEN: usize = 4;

/// `broken_gen` value meaning "no generation has been broken". Generations
/// start at 0 and only increase, so this can never collide with a real one.
pub const CB_NO_BREAK: i64 = -1;

/// Resolve (or lazily install) the receiver's holder array. Returns the
/// possibly-relocated receiver alongside it — installing the holder allocates.
pub fn cb_holder(ctx: &mut dyn NativeContext, this: ObjectRef) -> (ObjectRef, ObjectRef) {
    if let Value::Object(Some(h)) = ctx.get_field(this, CB_FIELD_PARTIES) {
        return (this, h);
    }
    let legacy_parties = match ctx.get_field(this, CB_FIELD_PARTIES) {
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let legacy_count = match ctx.get_field(this, CB_FIELD_COUNT) {
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let legacy_broken = match ctx.get_field(this, CB_FIELD_BROKEN) {
        Value::Int(v) => v,
        _ => 0,
    };
    // Two allocations, so the receiver AND the first array must be rooted
    // across the second (moving-GC relocation hazard).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let state = scope.new_array(cratonvm_types::ArrayElementType::Long, CB_STATE_LEN);
    let state_h = scope.root(state);
    let holder = scope.new_array(cratonvm_types::ArrayElementType::Reference, CB_HOLDER_LEN);
    let state = scope.get(&state_h);
    let this = scope.get(&this_h);
    scope.set_array_element(state, CB_H_PARTIES, Value::Long(legacy_parties));
    scope.set_array_element(state, CB_H_COUNT, Value::Long(legacy_count));
    scope.set_array_element(state, CB_H_GENERATION, Value::Long(0));
    scope.set_array_element(
        state,
        CB_H_BROKEN_GEN,
        Value::Long(if legacy_broken != 0 { 0 } else { CB_NO_BREAK }),
    );
    scope.set_array_element(holder, CB_HOLDER_STATE, Value::Object(Some(state)));
    scope.set_array_element(holder, CB_HOLDER_ACTION, Value::Object(None));
    scope.set_field(this, CB_FIELD_PARTIES, Value::Object(Some(holder)));
    (this, holder)
}

/// The `long[4]` state array inside a holder.
pub fn cb_state(ctx: &dyn NativeContext, holder: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_array_element(holder, CB_HOLDER_STATE) {
        Value::Object(Some(s)) => Some(s),
        _ => None,
    }
}

pub fn cb_get(ctx: &dyn NativeContext, state: ObjectRef, idx: usize) -> i64 {
    match ctx.get_array_element(state, idx) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    }
}

pub fn cb_set(ctx: &dyn NativeContext, state: ObjectRef, idx: usize, v: i64) {
    ctx.set_array_element(state, idx, Value::Long(v));
}

/// Receiver + holder + state in one step, for every native below.
pub fn cb_parts(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<(ObjectRef, ObjectRef, ObjectRef)> {
    let (this, holder) = cb_holder(ctx, this);
    let state = cb_state(ctx, holder)?;
    Some((this, holder, state))
}

/// Shared constructor body. `action` is the `Runnable` from the two-arg form.
pub fn cb_init_common(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    parties: i32,
    action: Option<ObjectRef>,
) -> MethodCallResult {
    if parties <= 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "parties must be > 0".to_string(),
        }
        .into());
    }
    // `cb_holder` allocates, so the action has to be rooted across it too —
    // it arrives as a raw `ObjectRef` from the caller's operand stack.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let action_h = action.map(|a| scope.root(a));
    let this = scope.get(&this_h);
    let Some((_, holder, state)) = cb_parts(&mut *scope, this) else {
        return Ok(None);
    };
    scope.set_array_element(state, CB_H_PARTIES, Value::Long(parties as i64));
    scope.set_array_element(state, CB_H_COUNT, Value::Long(0)); // number currently waiting
    scope.set_array_element(state, CB_H_GENERATION, Value::Long(0));
    scope.set_array_element(state, CB_H_BROKEN_GEN, Value::Long(CB_NO_BREAK));
    let action_now = action_h.as_ref().map(|h| scope.get(h));
    scope.set_array_element(holder, CB_HOLDER_ACTION, Value::Object(action_now));
    Ok(None)
}

pub fn native_cb_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let parties = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    cb_init_common(ctx, this, parties, None)}

/// `CyclicBarrier(int parties, Runnable barrierAction)`.
///
/// The action is stored in the holder's reference slot and run by
/// [`cb_await_inner`] on the last arriving thread. It used to be discarded
/// outright.
pub fn native_cb_init_action(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let parties = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let action = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    cb_init_common(ctx, this, parties, action)}

/// Throw the REAL `java.util.concurrent.BrokenBarrierException` /
/// `TimeoutException` for a barrier failure, rather than an
/// `IllegalStateException` whose *message* merely names them.
///
/// `CyclicBarrier.await` declares both as checked exceptions and callers
/// discriminate on the TYPE: `catch (TimeoutException)` to retry vs
/// `catch (BrokenBarrierException)` to abandon the generation. An
/// `IllegalStateException` matches neither, so a correct caller propagates a
/// fatal error where HotSpot would have recovered.
///
/// Falls back to the historic `IllegalStateException` when the class cannot be
/// constructed (synthetic-JDK mode without these classes registered), so no
/// configuration loses the failure entirely.
pub fn cb_throw(ctx: &mut dyn NativeContext, class_name: &str) -> MethodCallFailed {
    match ctx.new_object_initialized(class_name, "()V", &[]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalStateException {
            message: format!(
                "{}: CyclicBarrier await",
                class_name.rsplit('/').next().unwrap_or(class_name)
            ),
        }
        .into(),
    }
}

pub const CB_BROKEN_BARRIER: &str = "java/util/concurrent/BrokenBarrierException";
pub const CB_TIMEOUT: &str = "java/util/concurrent/TimeoutException";

/// Shared barrier-await. `deadline: None` blocks indefinitely (the plain
/// `await()`, which previously returned WITHOUT waiting for the other
/// parties — wrong for any real barrier user once the storage works).
pub fn cb_await_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    deadline: Option<std::time::Instant>,
) -> MethodCallResult {
    // Install the holder up-front so no allocation happens inside the
    // monitor section (re-binds `this` across the possible allocation).
    let Some((this, holder, state)) = cb_parts(ctx, this) else {
        return Ok(Some(Value::Int(0)));
    };
    // Rebound after the barrier action runs, which can move all three.
    let (mut this, mut holder, mut state) = (this, holder, state);

    ctx.monitor_enter(this);
    if cb_get(ctx, state, CB_H_BROKEN_GEN) == cb_get(ctx, state, CB_H_GENERATION) {
        ctx.monitor_exit(this);
        return Err(cb_throw(ctx, CB_BROKEN_BARRIER));
    }
    let parties = cb_get(ctx, state, CB_H_PARTIES).max(1);
    let my_gen = cb_get(ctx, state, CB_H_GENERATION);
    let count = cb_get(ctx, state, CB_H_COUNT);
    let new_count = count + 1;

    if new_count >= parties {
        // Last party in. The barrier action runs HERE — still holding the
        // monitor, with no party released yet — which is where
        // `CyclicBarrier.nextGeneration` runs it under its own ReentrantLock.
        // A caller can therefore rely on the action having completed before
        // any `await()` returns.
        let action = match ctx.get_array_element(holder, CB_HOLDER_ACTION) {
            Value::Object(Some(a)) => Some(a),
            _ => None,
        };
        if let Some(action) = action {
            let (run_result, this_now, holder_now, state_now) = {
                let mut scope = NativeHandleScope::new(ctx);
                let this_h = scope.root(this);
                let holder_h = scope.root(holder);
                let state_h = scope.root(state);
                let action_h = scope.root(action);
                let receiver = scope.get(&action_h);
                let r = scope.invoke_virtual(receiver, "run", "()V", &[]);
                (
                    r,
                    scope.get(&this_h),
                    scope.get(&holder_h),
                    scope.get(&state_h),
                )
            };
            this = this_now;
            holder = holder_now;
            state = state_now;
            let _ = holder;
            if let Err(e) = run_result {
                // HotSpot breaks the barrier and propagates: every other party
                // must fail rather than silently proceed past an action that
                // did not complete.
                cb_set(ctx, state, CB_H_BROKEN_GEN, my_gen);
                let _ = ctx.monitor_notify_all(this);
                ctx.monitor_exit(this);
                return Err(e);
            }
        }
        // Trip: open the next generation and wake everyone parked in this one.
        cb_set(ctx, state, CB_H_COUNT, 0);
        cb_set(ctx, state, CB_H_GENERATION, my_gen.wrapping_add(1));
        let notify_result = ctx.monitor_notify_all(this);
        ctx.monitor_exit(this);
        notify_result?;
        return Ok(Some(Value::Int(0)));
    }

    // Not all parties yet — record the arrival and wait for this generation to
    // trip, break, or time out.
    cb_set(ctx, state, CB_H_COUNT, new_count);
    let arrival_index = (parties - new_count) as i32;
    let this_pin = ctx.pin_native_root(this);
    // `state` is the int[] holder every `cb_get`/`cb_set` below dereferences,
    // and it crosses the same `monitor_wait` park as the receiver. Pinning
    // only `this` kept it ALIVE (it hangs off `this`) but not CURRENT: after a
    // relocating collection inside the wait, every state read in the next
    // iteration went to the pre-wait array.
    let state_pin = ctx.pin_native_root(state);
    loop {
        let this = ctx.read_native_pin(this_pin, this);
        let state = ctx.read_native_pin(state_pin, state);
        if cb_get(ctx, state, CB_H_BROKEN_GEN) == my_gen {
            ctx.monitor_exit(this);
            return Err(cb_throw(ctx, CB_BROKEN_BARRIER));
        }
        if cb_get(ctx, state, CB_H_GENERATION) != my_gen {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(arrival_index)));
        }
        let wait_ms = match deadline {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    // Timeout — break THIS generation so the other parties
                    // waiting in it fail too, and leave it broken until
                    // `reset()`, which is what `isBroken()` reports.
                    cb_set(ctx, state, CB_H_BROKEN_GEN, my_gen);
                    let notify_result = ctx.monitor_notify_all(this);
                    ctx.monitor_exit(this);
                    notify_result?;
                    return Err(cb_throw(ctx, CB_TIMEOUT));
                }
                bounded_monitor_wait_ms(remaining, 10)
            }
            None => 10,
        };
        // monitor_wait releases the monitor while parked and reacquires it
        // before returning, so the loop re-reads state under the lock. On
        // error (interrupt) the monitor is still held — release it before
        // propagating so the unwind doesn't leak ownership.
        if let Err(e) = ctx.monitor_wait(this, Some(wait_ms)) {
            // The wait PARKED before it failed, so a collection may have moved
            // the receiver: exit on the post-wait address, not the pre-wait one
            // (`MonitorTable::exit` dereferences it).
            let this = ctx.read_native_pin(this_pin, this);
            ctx.monitor_exit(this);
            return Err(e);
        }
    }
}

pub fn native_cb_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    cb_await_inner(ctx, this, None)}

pub fn native_cb_await_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // args: this, timeout(long), TimeUnit
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
    cb_await_inner(ctx, this, Some(deadline))
}

pub fn native_cb_get_parties(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some((_, _, state)) = cb_parts(ctx, this) else {
        return Ok(Some(Value::Int(0)));
    };
    Ok(Some(Value::Int(cb_get(ctx, state, CB_H_PARTIES) as i32)))}

pub fn native_cb_get_number_waiting(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some((_, _, state)) = cb_parts(ctx, this) else {
        return Ok(Some(Value::Int(0)));
    };
    Ok(Some(Value::Int(cb_get(ctx, state, CB_H_COUNT) as i32)))}

/// `isBroken()` asks about the CURRENT generation, not about whether the
/// barrier was ever broken — which is why `reset()` below can leave a broken
/// generation behind and still report false.
pub fn native_cb_is_broken(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some((_, _, state)) = cb_parts(ctx, this) else {
        return Ok(Some(Value::Int(0)));
    };
    let broken = cb_get(ctx, state, CB_H_BROKEN_GEN) == cb_get(ctx, state, CB_H_GENERATION);
    Ok(Some(Value::Int(broken as i32)))
}

/// `reset()` — break the generation the parked parties are waiting in, then
/// open a fresh, unbroken one.
///
/// Both halves matter: parties already at the barrier must wake with
/// `BrokenBarrierException` (they were promised a trip that will not happen),
/// while `isBroken()` must read false immediately afterwards. Clearing a single
/// "broken" flag cannot express that — the waiters have not run yet when
/// `reset()` returns, so by the time they look, the flag they needed to see is
/// already gone.
pub fn native_cb_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((this, _, state)) = cb_parts(ctx, this) else {
        return Ok(None);
    };
    ctx.monitor_enter(this);
    let gen = cb_get(ctx, state, CB_H_GENERATION);
    cb_set(ctx, state, CB_H_BROKEN_GEN, gen);
    cb_set(ctx, state, CB_H_COUNT, 0);
    cb_set(ctx, state, CB_H_GENERATION, gen.wrapping_add(1));
    let notify_result = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    notify_result?;
    Ok(None)
}


// ===========================================================================
// Base64 — Encoder/Decoder for basic, URL-safe, and MIME variants
//
// BOTH synthetics use the REAL JDK instance-field layout, in declaration
// order, so that the public methods this file does NOT register (7 of the 18
// — see `register_base64_natives`) can run real JDK bytecode against a
// receiver this file fabricated. Read off `javap -p` on 25.0.3+9-LTS:
//
//   java.util.Base64$Encoder   0 newline:[B  1 linemax:I  2 isURL:Z  3 doPadding:Z
//   java.util.Base64$Decoder   0 isURL:Z     1 isMIME:Z
//
// The Decoder was a ONE-field synthetic holding an Int variant tag (0/1/2) in
// slot 0 until 2026-08-13, which slot 0 of the real layout calls `isURL`. So
// `getMimeDecoder()` wrote 2 into `isURL` and left `isMIME` false, and every
// real-bytecode entry point read our MIME decoder as a URL, non-MIME one.
// Measured on HotSpot by building `Decoder(isURL=true, isMIME=false)` through
// the private constructor and decoding what `getMimeDecoder()` must accept
// (`scratchpad/e14/B64Unreg.java`):
//
//   input                real getMimeDecoder()  our fabricated shape
//   76-col wrapped text   len=60                 IllegalArgumentException:
//                                                  Illegal base64 character d
//   "-_-_"                len=0                  len=3
//   "QQ\n=="              len=1                  IllegalArgumentException:
//                                                  Illegal base64 character a
//
// Three of four rows wrong, one of them silently. That is the two-readers-of-
// one-slot shape: the NATIVE route read slot 0 as a variant tag and the
// BYTECODE route read it as `isURL`.
//
// The variant tag survives as this file's INTERNAL vocabulary (it is what
// `b64_decode`/`b64_encode` take), derived from the two booleans on read.
// Tags: 0 = basic, 1 = url-safe, 2 = MIME.
// ===========================================================================

pub const B64_ENCODER_CLASS: &str = "java/util/Base64$Encoder";
pub const B64_DECODER_CLASS: &str = "java/util/Base64$Decoder";
pub const B64_ENCODER_FIELD_NEWLINE: usize = 0;
pub const B64_ENCODER_FIELD_LINEMAX: usize = 1;
pub const B64_ENCODER_FIELD_IS_URL: usize = 2;
pub const B64_ENCODER_FIELD_DO_PADDING: usize = 3;
pub const B64_ENCODER_NUM_FIELDS: usize = 4;
pub const B64_DECODER_FIELD_IS_URL: usize = 0;
pub const B64_DECODER_FIELD_IS_MIME: usize = 1;
pub const B64_DECODER_NUM_FIELDS: usize = 2;
pub const B64_VARIANT_BASIC: i32 = 0;
pub const B64_VARIANT_URL: i32 = 1;
pub const B64_VARIANT_MIME: i32 = 2;

/// `Base64.Encoder.MIMELINEMAX`.
pub const B64_MIME_LINEMAX: i32 = 76;

/// What the real JDK writes into `linemax` for every encoder that does NOT
/// wrap lines — **-1, not 0**, and the difference is load-bearing.
///
/// `outLength` and `encode0` both test `linemax > 0`, which reads 0 and -1
/// alike. `Base64$EncOutputStream` does not: it tests `linepos == linemax`,
/// and `linepos` starts at 0. Measured on HotSpot by constructing each shape
/// through the private `Encoder(boolean,byte[],int,boolean)` constructor
/// (`scratchpad/e14/B64Zero.java`), encoding 60 bytes:
///
/// ```text
/// linemax=0  newline=null  encodeToString=80  wrap(OutputStream)=NPE / "b" is null
/// linemax=0  newline=CRLF  encodeToString=80  wrap(OutputStream)=82   <- WRONG, a
///                                                                       basic stream
///                                                                       with a CRLF in it
/// linemax=-1 newline=null  encodeToString=80  wrap(OutputStream)=80   <- real getEncoder()
/// ```
///
/// So this file writing 0 meant `Base64.getEncoder().wrap(os)` — one of the
/// unregistered methods, hence real bytecode — threw NullPointerException on
/// the first write. Writing -1 is what makes `wrap` correct, and it is ALSO
/// why "just give the fabricated encoder a CRLF newline" is not the fix on its
/// own: with linemax still 0 that turns the NPE into a silently wrong 82.
pub const B64_NO_LINEMAX: i32 = -1;

/// `Base64.Encoder.CRLF` — the line separator of `getMimeEncoder()`.
pub const B64_MIME_CRLF: [u8; 2] = *b"\r\n";

pub const B64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
pub const B64_URL_CHARS: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Variant-tag entry point, kept for the callers outside this module
/// (`http_url_connection.rs`, `net_phase_e.rs`, `regex_matcher.rs`'s test) that
/// only ever want one of the three canonical encoders. MIME here means the
/// canonical `getMimeEncoder()` shape, 76 columns and CRLF.
///
/// Everything driven by an actual `Base64$Encoder` receiver goes through
/// [`b64_encode_wrapped`] instead, because a receiver carries its OWN
/// `linemax`/`newline` and this tag cannot represent them.
pub fn b64_encode(input: &[u8], variant: i32, no_padding: bool) -> Vec<u8> {
    let (linemax, newline): (i32, &[u8]) = if variant == B64_VARIANT_MIME {
        (B64_MIME_LINEMAX, &B64_MIME_CRLF)
    } else {
        (B64_NO_LINEMAX, &[])
    };
    b64_encode_wrapped(
        input,
        variant == B64_VARIANT_URL,
        no_padding,
        linemax,
        newline,
    )
}

/// Line-for-line port of real `java.util.Base64.Encoder.encode0` (JDK 25
/// `java.base/java/util/Base64.java`), with `linemax` and `newline` taken from
/// the caller instead of hardcoded at 76/CRLF.
///
/// Hardcoding them made `Base64.getMimeEncoder(20, new byte[]{'\n'})` — the one
/// factory this file does not register, so it runs real bytecode and hands back
/// a real `Encoder` with `linemax=20, newline=[10]` — encode with 76-column
/// CRLF wrapping anyway. On the 60-byte fixture (80 base64 chars) that is
/// `80 + 1 separator x 2 bytes = 82` against HotSpot's
/// `80 + 3 separators x 1 byte = 83`. **Off by one, not off by a mile**, which
/// is the dangerous kind: a caller asserting "about the right length", or
/// `decode`-ing the result (MIME decoding ignores every non-alphabet byte, so
/// both round-trip), cannot tell. It is wrong in the LINE STRUCTURE, not the
/// length — one 76-char line where the caller asked for four 20-char lines.
///
/// Three details of `encode0` that a plausible rewrite gets wrong, each
/// measured on HotSpot in `scratchpad/e14/B64Line.java`:
///
/// 1. **The separator test is `dlen == linemax`, an EQUALITY.** `slen` is
///    rounded down to a whole number of triples, so a `linemax` that is not a
///    multiple of 4 can never be hit and the encoder emits NO separators at
///    all: `linemax=20 -> 83` but `linemax=22 -> 80`, and `linemax=6 -> 80`.
///    A `>=` here would wrap all three.
/// 2. **A separator is written only when input remains** (`&& sp < end`), so a
///    payload ending exactly on a line boundary gets no trailing separator:
///    57 bytes at `linemax=76` is 76 chars, not 78; 15 bytes at `linemax=20`
///    is 20, not 21.
/// 3. **`linemax > 0` is not the same as "wraps"**, and `linemax` past the
///    payload is fine: `linemax=1000` on 60 bytes is 80 chars.
///
/// The one deliberate divergence is `linemax` in `1..=3`, where
/// `linemax / 4 * 3` is 0, real JDK's block size becomes 0 and its
/// `while (sp < sl)` loop never advances. Measured: HotSpot **hangs** (all
/// three values still running after 1.2 s in `scratchpad/e14/B64Fab.java`).
/// No public factory can produce it — `getMimeEncoder(int, byte[])` rounds
/// `lineLength >> 2 << 2` and returns the unwrapped `RFC4648` singleton for
/// anything below 4 — so it is reachable only through reflection. We treat it
/// as "no wrapping" rather than reproducing a hang.
pub fn b64_encode_wrapped(
    input: &[u8],
    is_url: bool,
    no_padding: bool,
    linemax: i32,
    newline: &[u8],
) -> Vec<u8> {
    let table = if is_url { B64_URL_CHARS } else { B64_CHARS };
    let end = input.len();
    // `int slen = (end - off) / 3 * 3;` — the whole-triple prefix. `sl` is
    // where the 3-byte loop stops and the 1-or-2-byte tail begins.
    let sl = end / 3 * 3;
    // `if (linemax > 0 && slen > linemax / 4 * 3) slen = linemax / 4 * 3;` —
    // real JDK REUSES `slen` as the per-line block size once the payload is
    // longer than one line. When it is not, the whole payload is one block and
    // `dlen` may still equal `linemax` exactly (15 bytes at linemax 20), which
    // rule 2 above is what keeps from emitting a trailing separator.
    let line_bytes = if linemax > 0 {
        (linemax / 4 * 3) as usize
    } else {
        0
    };
    let block = if linemax > 0 && sl > line_bytes && line_bytes > 0 {
        line_bytes
    } else {
        sl
    };
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4 + newline.len() * 4);
    let mut sp = 0usize;
    while sp < sl {
        let sl0 = (sp + block).min(sl);
        let mut sp0 = sp;
        while sp0 < sl0 {
            let triple = ((input[sp0] as u32) << 16)
                | ((input[sp0 + 1] as u32) << 8)
                | input[sp0 + 2] as u32;
            out.push(table[((triple >> 18) & 0x3F) as usize]);
            out.push(table[((triple >> 12) & 0x3F) as usize]);
            out.push(table[((triple >> 6) & 0x3F) as usize]);
            out.push(table[(triple & 0x3F) as usize]);
            sp0 += 3;
        }
        let dlen = (sl0 - sp) / 3 * 4;
        sp = sl0;
        // `if (dlen == linemax && sp < end)` — see rules 1 and 2 above.
        if linemax > 0 && dlen == linemax as usize && sp < end {
            out.extend_from_slice(newline);
        }
    }
    let remaining = end - sp;
    if remaining == 1 {
        let b0 = input[sp] as u32;
        out.push(table[((b0 >> 2) & 0x3F) as usize]);
        out.push(table[((b0 << 4) & 0x3F) as usize]);
        if !no_padding {
            out.push(b'=');
            out.push(b'=');
        }
    } else if remaining == 2 {
        let b0 = input[sp] as u32;
        let b1 = input[sp + 1] as u32;
        out.push(table[((b0 >> 2) & 0x3F) as usize]);
        out.push(table[(((b0 << 4) | (b1 >> 4)) & 0x3F) as usize]);
        out.push(table[((b1 << 2) & 0x3F) as usize]);
        if !no_padding {
            out.push(b'=');
        }
    }
    out
}

pub fn b64_decode_char(c: u8, variant: i32) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' if variant != B64_VARIANT_URL => Some(62),
        b'/' if variant != B64_VARIANT_URL => Some(63),
        b'-' if variant == B64_VARIANT_URL => Some(62),
        b'_' if variant == B64_VARIANT_URL => Some(63),
        _ => None,
    }
}

/// If `bytes` is a PEM-armored block (`-----BEGIN … -----` … `-----END … -----`),
/// base64-decode its body to DER; otherwise return `bytes` unchanged.
///
/// `CertificateFactory.generateCertificate` accepts BOTH DER and PEM streams on
/// real JDK (`sun.security.provider.X509Factory` sniffs the `-----BEGIN` armor).
/// CratonVM's native read the raw stream bytes as if always-DER, so a PEM stream
/// (e.g. Netty's `SelfSignedCertificate` cert file, handed to
/// `generateCertificate` unparsed) built a cert whose `getEncoded()` was empty →
/// rustls `invalid peer certificate: BadEncoding` on the server handshake. See
/// http-server-sslengine-identity-singleton-clobber-FIXED.md.
pub fn pem_block_to_der(bytes: &[u8]) -> Vec<u8> {
    const BEGIN: &[u8] = b"-----BEGIN";
    // Locate the first `-----BEGIN` line; bail (return input) if absent.
    let Some(begin) = bytes.windows(BEGIN.len()).position(|w| w == BEGIN) else {
        return bytes.to_vec();
    };
    // Body starts after the end of the BEGIN line.
    let after_begin = &bytes[begin..];
    let Some(nl) = after_begin.iter().position(|&c| c == b'\n') else {
        return bytes.to_vec();
    };
    let body_start = begin + nl + 1;
    // Body ends at the `-----END` line.
    const END: &[u8] = b"-----END";
    let end_rel = bytes[body_start..]
        .windows(END.len())
        .position(|w| w == END);
    let body = match end_rel {
        Some(e) => &bytes[body_start..body_start + e],
        None => &bytes[body_start..],
    };
    // MIME, not BASIC: a PEM body is line-wrapped by definition (RFC 7468
    // caps it at 64 chars per line), and even a one-line body carries the
    // newline that precedes `-----END`. The BASIC decoder rejects EVERY
    // non-alphabet byte including whitespace (see `b64_decode`), so with it
    // this function could never decode any real PEM — it always fell through
    // to the "hand the original bytes back" arm and returned the armored text
    // as if it were DER. MIME is exactly the whitespace-skipping variant.
    match b64_decode(body, B64_VARIANT_MIME) {
        Ok(der) if !der.is_empty() => der,
        // Not valid base64 (or empty) — hand the original bytes back so the
        // caller's existing DER path / mirror fallback runs exactly as before.
        _ => bytes.to_vec(),
    }
}

/// Matches real `java.util.Base64.Decoder`'s exact wording for an
/// out-of-alphabet byte (`"Illegal base64 character " +
/// Integer.toString(src[sp - 1], 16)`) — Spring Boot's
/// `Base64ProtocolResolverTests`/`JksSslStoreBundleTests` assert on this
/// text, not just the exception type.
///
/// `src` is a `byte[]`, so the value handed to `Integer.toString(int, 16)` is
/// the SIGNED byte: `0xff` renders as `"-1"` and `0xc3` as `"-3d"`, not as
/// `"ff"`/`"c3"`. Verified against HotSpot 25.
pub fn b64_illegal_char_msg(b: u8) -> String {
    let signed = b as i8 as i32;
    if signed < 0 {
        format!("Illegal base64 character -{:x}", signed.unsigned_abs())
    } else {
        format!("Illegal base64 character {signed:x}")
    }
}

/// Line-for-line port of real `java.util.Base64.Decoder` (JDK 25
/// `java.base/java/util/Base64.java`): the two length guards of
/// `decodedOutLength`, then `decode0`'s shift-register loop.
///
/// The previous hand-rolled quad-at-a-time loop diverged from the JDK on
/// every malformed input — different exception text for each of the four
/// error shapes, no "trailing bytes after the padding" check at all, and it
/// treated MIME as merely "strip whitespace" when RFC 2045 decoding must
/// ignore EVERY non-alphabet byte. Callers assert on that text (Spring Boot's
/// `Base64ProtocolResolverTests` / `JksSslStoreBundleTests` both check for
/// `"Illegal base64"`), so the wording is part of the contract, not cosmetic.
/// Each case below is pinned by `Base64Probe` diffed against HotSpot 25.
pub fn b64_decode(input: &[u8], variant: i32) -> Result<Vec<u8>, String> {
    let is_mime = variant == B64_VARIANT_MIME;
    let sl = input.len();

    // `decodedOutLength` runs first on real JDK (`decode` sizes `dst` with it
    // before calling `decode0`), so its guard wins over anything the decode
    // loop would report. A 1-byte input is fatal for BASIC/URL but not for
    // MIME — MIME falls through to `decode0`, which then reports the dangling
    // unit itself ("Last unit does not have enough valid bits").
    if sl == 1 && !is_mime {
        return Err("Input byte[] should at least have 2 bytes for base64 bytes".to_string());
    }

    let mut out: Vec<u8> = Vec::with_capacity(sl / 4 * 3 + 3);
    let mut sp = 0usize;
    let mut bits: u32 = 0;
    let mut shiftto: i32 = 18; // bit position of the first byte of a 4-char atom

    while sp < sl {
        let raw = input[sp];
        sp += 1;
        let Some(b) = b64_decode_char(raw, variant) else {
            if raw == b'=' {
                // `=`    shiftto==18 — padding with nothing in front of it
                // `x=`   shiftto==12 — dangling single char, handled after the loop
                // `xx=`  shiftto==6 && sp==sl — missing the second `=`
                // `xx=y` shiftto==6 — last char is not `=`
                let wrong_tail = shiftto == 6 && {
                    if sp == sl {
                        true
                    } else {
                        let next = input[sp];
                        sp += 1;
                        next != b'='
                    }
                };
                if wrong_tail || shiftto == 18 {
                    return Err("Input byte array has wrong 4-byte ending unit".to_string());
                }
                break;
            }
            if is_mime {
                // RFC 2045: ignore every non-alphabet byte, not just whitespace.
                continue;
            }
            return Err(b64_illegal_char_msg(raw));
        };
        bits |= b << (shiftto as u32);
        shiftto -= 6;
        if shiftto < 0 {
            out.push((bits >> 16) as u8);
            out.push((bits >> 8) as u8);
            out.push(bits as u8);
            shiftto = 18;
            bits = 0;
        }
    }
    // Reached the end of the input, or broke out on padding.
    if shiftto == 6 {
        out.push((bits >> 16) as u8);    } else if shiftto == 0 {
        out.push((bits >> 16) as u8);
        out.push((bits >> 8) as u8);    } else if shiftto == 12 {
        return Err("Last unit does not have enough valid bits".to_string());    }
    // Anything past the padding is invalid — except MIME filler, which is
    // skipped. Note real JDK's `if (isMIME && base64[src[sp++]] < 0)` only
    // evaluates (and so only advances) `sp` when `isMIME` is true, which is
    // why the reported index differs by one between the two modes.
    while sp < sl {
        if is_mime {
            let trailing = input[sp];
            sp += 1;
            if b64_decode_char(trailing, variant).is_none() {
                continue;
            }
        }
        return Err(format!(
            "Input byte array has incorrect ending byte at {sp}"
        ));
    }
    Ok(out)
}

/// 11 of `java.util.Base64`'s 18 public methods are registered here. The other
/// SEVEN are **deliberately left to real JDK bytecode**, which is the whole
/// point of `--real-jdk` mode: a native that shadows correct bytecode is a
/// liability, and each of these seven would be one.
///
/// | left unregistered | why NOT a native | HotSpot, 60-byte fixture |
/// |---|---|---|
/// | `Base64.getMimeEncoder(I[B)LBase64$Encoder;` | a native would have to re-implement three separately measured behaviours: `lineLength >> 2 << 2` rounding (`19 -> 16`, `22 -> 20`), the `IllegalArgumentException / Illegal base64 line separator character 0x41` alphabet check on the separator (`'-'` passes, `'='` does not), and the `lineLength <= 0 -> Encoder.RFC4648` aliasing. The bytecode has all three. | 83 |
/// | `Base64$Encoder.encode([B[B)I` | real `encode0` + the `Output byte array is too small for encoding all input bytes` contract | 82 mime / 80 basic |
/// | `Base64$Encoder.encode(LByteBuffer;)LByteBuffer;` | plus ByteBuffer position/limit semantics (measured: source position advances to 60) | 82 |
/// | `Base64$Encoder.wrap(LOutputStream;)LOutputStream;` | returns a stateful `Base64$EncOutputStream` | 82 |
/// | `Base64$Decoder.decode([B[B)I` | real `decode0` | 60 |
/// | `Base64$Decoder.decode(LByteBuffer;)LByteBuffer;` | ditto | 60 |
/// | `Base64$Decoder.wrap(LInputStream;)LInputStream;` | returns a stateful `Base64$DecInputStream` | 60 |
///
/// All seven read their answer off the RECEIVER's fields, so "leave it to
/// bytecode" is only a real decision once the receiver is honest — which is why
/// this lane's work is in `b64_alloc_encoder` (writing `newline`, and `-1` not
/// `0` into `linemax`) and `b64_alloc_decoder` (the two-boolean layout) rather
/// than in seven new `registry.register` calls. Before those two fixes, four of
/// the seven threw `NullPointerException` and three silently decoded as the
/// wrong variant.
///
/// **Synthetic-JDK mode is the exception, and it stays broken on purpose.**
/// There is no bytecode there, so all seven are `NoSuchMethodError`. Registering
/// them to fix that would ALSO shadow the real bytecode in real-JDK mode,
/// because a `registry.register` call cannot see the runtime JDK mode — the same
/// trap in reverse as gating a native on a feature flag and thereby dropping it
/// from a mode whose stub has no method bodies. Nothing in the corpus is known
/// to call these seven; leaving them unregistered keeps the real-JDK answer
/// correct and costs synthetic mode only what it already lacked.
pub fn register_base64_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: Base64 encode/decode is a spec-exact algorithm replicating
    // real JDK bytecode → Intrinsic.
            let b64 = "java/util/Base64";
    let enc = "java/util/Base64$Encoder";
    let dec = "java/util/Base64$Decoder";

    // --- Base64 static factory methods ---
    registry.register(
        b64,
        "getEncoder",
        "()Ljava/util/Base64$Encoder;",
        native_b64_get_encoder, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        b64,
        "getUrlEncoder",
        "()Ljava/util/Base64$Encoder;",
        native_b64_get_url_encoder, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        b64,
        "getMimeEncoder",
        "()Ljava/util/Base64$Encoder;",
        native_b64_get_mime_encoder, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        b64,
        "getDecoder",
        "()Ljava/util/Base64$Decoder;",
        native_b64_get_decoder, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        b64,
        "getUrlDecoder",
        "()Ljava/util/Base64$Decoder;",
        native_b64_get_url_decoder, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        b64,
        "getMimeDecoder",
        "()Ljava/util/Base64$Decoder;",
        native_b64_get_mime_decoder, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Encoder methods ---
    registry.register(enc, "encode", "([B)[B", native_b64_encode, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        enc,
        "encodeToString",
        "([B)Ljava/lang/String;",
        native_b64_encode_to_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        enc,
        "withoutPadding",
        "()Ljava/util/Base64$Encoder;",
        native_b64_without_padding, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Decoder methods ---
    registry.register(dec, "decode", "([B)[B", native_b64_decode_bytes, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        dec,
        "decode",
        "(Ljava/lang/String;)[B",
        native_b64_decode_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

/// Allocate a fabricated `Base64$Encoder` with all FOUR JDK fields written.
///
/// **The GC hazard this function's shape exists to avoid.**
/// `try_alloc_concurrent_synthetic` hands back a bare `ObjectRef` — a raw
/// pointer into the GC heap (`types/src/value.rs`), not a root. The MIME arm
/// then has to allocate a second object, the `byte[]{13,10}` line separator,
/// *between* that allocation and the `set_field` calls that finish the encoder.
/// `new_array` can trigger a young collection, and this VM's young collector
/// MOVES: it evacuates survivors and rewrites only the root sets it knows —
/// thread stacks, statics, `native_pin_roots`, and the per-thread handle-slot
/// table (`vm/src/memory/roots.rs`). A raw `ObjectRef` in a Rust local is in
/// none of them, and the encoder is at that moment young, half-built, and
/// referenced from nowhere in Java — so it is not merely relocatable, it is
/// *collectable*. The four `set_field`s would then write `newline`, `linemax`,
/// `isURL`, `doPadding` into whatever now occupies the vacated from-space slot.
/// That is the bug family `types/src/handle.rs` opens by calling this codebase's
/// #1 recurring defect, with 37+ independently-found sites.
///
/// The old body was safe only because it never allocated between the two — it
/// simply left `newline` null. Fixing that introduces the hazard, which is why
/// this is not the one-liner it looks like. `NativeHandleScope` closes it: the
/// encoder is read back through a collector-updated slot AFTER the array
/// allocation, never from the pre-allocation local, and the scope pops on every
/// Rust exit path including the `?` above.
pub fn b64_alloc_encoder(
    ctx: &mut dyn NativeContext,
    variant: i32,
    no_padding: bool,
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let mut scope = NativeHandleScope::new(ctx);
    let encoder =
        try_alloc_concurrent_synthetic(&mut *scope, B64_ENCODER_CLASS, B64_ENCODER_NUM_FIELDS)?;
    let encoder_h = scope.root(encoder);
    // ---- allocating region: `encoder` must not be read as a raw local past here
    let newline_h = if variant == B64_VARIANT_MIME {
        let arr = scope.new_array(ArrayElementType::Byte, B64_MIME_CRLF.len());
        for (i, &b) in B64_MIME_CRLF.iter().enumerate() {
            scope.set_array_element(arr, i, Value::Int(b as i32));
        }
        Some(scope.root(arr))
    } else {
        None
    };
    // ---- end allocating region: re-read both through their slots
    let encoder = scope.get(&encoder_h);
    let newline = newline_h.as_ref().map(|h| scope.get(h));
    // Real `getEncoder()`/`getUrlEncoder()` have `newline == null`; only the
    // MIME encoder carries CRLF. Writing null EXPLICITLY (rather than leaving
    // the slot untouched) is what makes the fabricated object's shape match the
    // real one even when `try_alloc_concurrent_synthetic` hands back a recycled
    // span.
    scope.set_field(encoder, B64_ENCODER_FIELD_NEWLINE, Value::Object(newline));
    scope.set_field(
        encoder,
        B64_ENCODER_FIELD_LINEMAX,
        // -1, not 0, for the non-wrapping encoders — see `B64_NO_LINEMAX`.
        Value::Int(if variant == B64_VARIANT_MIME {
            B64_MIME_LINEMAX
        } else {
            B64_NO_LINEMAX
        }),
    );
    scope.set_field(
        encoder,
        B64_ENCODER_FIELD_IS_URL,
        Value::Int(i32::from(variant == B64_VARIANT_URL)),
    );
    scope.set_field(
        encoder,
        B64_ENCODER_FIELD_DO_PADDING,
        Value::Int(i32::from(!no_padding)),
    );
    Ok(Some(Value::Object(Some(encoder))))
}

/// Allocate a fabricated `Base64$Decoder` in the real JDK's TWO-boolean layout
/// (`isURL`, `isMIME`) — see this section's header for the measured damage the
/// previous one-Int-slot layout did to every real-bytecode entry point.
///
/// No allocation happens between the object and its field writes, so unlike
/// `b64_alloc_encoder` this one needs no handle scope.
pub fn b64_alloc_decoder(ctx: &mut dyn NativeContext, variant: i32) -> MethodCallResult {
    let decoder = try_alloc_concurrent_synthetic(ctx, B64_DECODER_CLASS, B64_DECODER_NUM_FIELDS)?;
    ctx.set_field(
        decoder,
        B64_DECODER_FIELD_IS_URL,
        Value::Int(i32::from(variant == B64_VARIANT_URL)),
    );
    ctx.set_field(
        decoder,
        B64_DECODER_FIELD_IS_MIME,
        Value::Int(i32::from(variant == B64_VARIANT_MIME)),
    );
    Ok(Some(Value::Object(Some(decoder))))
}

/// The real JDK's own singleton for one of the six no-arg factories, or `None`
/// when there is no real class library to take it from.
///
/// **Why this, and not a native-side cache.** HotSpot returns the SAME object
/// from repeated calls — identity, not equality (`scratchpad/e14/B64Ident.java`):
///
/// ```text
/// Base64.getEncoder()    == Base64.getEncoder()    -> true
/// Base64.getUrlEncoder() == Base64.getUrlEncoder() -> true
/// Base64.getMimeEncoder()== Base64.getMimeEncoder()-> true
/// Base64.getDecoder()    == Base64.getDecoder()    -> true
/// Base64.getUrlDecoder() == Base64.getUrlDecoder() -> true
/// Base64.getMimeDecoder()== Base64.getMimeDecoder()-> true
/// ```
///
/// and they are exactly the `static final` fields `Encoder.RFC4648` /
/// `RFC4648_URLSAFE` / `RFC2045` and the matching three on `Decoder`. Caching a
/// FABRICATED object in a native `static` would reproduce the `==` rows above,
/// but it cannot reproduce this one, also measured:
///
/// ```text
/// Base64.getMimeEncoder(0, sep) == Base64.getEncoder() -> true
/// ```
///
/// `getMimeEncoder(int, byte[])` is deliberately unregistered (see
/// `register_base64_natives`), so it runs real bytecode and returns the real
/// `Encoder.RFC4648` for any `lineLength <= 0`. No amount of native-side caching
/// makes that equal to an object this file fabricated; only *being* the JDK's
/// singleton does. Taking the field is also free of the two things a cache would
/// need and could get wrong: it is already a GC root (a static field), so no
/// `register_var_handle_root` / `read_var_handle_root` re-read discipline, and
/// it is already VM-scoped, so no process-global `static` to scope by
/// `vm_identity()`.
///
/// Caching is otherwise SAFE — every instance field of both classes is `final`
/// (measured), so a shared instance has no mutable state — but unnecessary.
///
/// Returns `None`, and the caller fabricates exactly as before, whenever the
/// real class library is absent (synthetic-JDK mode), the class will not
/// initialize, the field is not declared, or it holds anything other than a
/// non-null reference. `Base64$Encoder.<clinit>` and `Base64$Decoder.<clinit>`
/// build these three statics each and call nothing on `java.util.Base64`
/// (checked with `javap -c`), so this cannot recurse back into the factory.
pub fn b64_jdk_singleton(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
) -> Option<ObjectRef> {
    let cid = ctx.ensure_class_initialized(class_name).ok()?;
    let index = ctx.static_field_index_by_name(cid, field_name)?;
    match ctx.get_static_field(cid, index) {
        Value::Object(Some(singleton)) => Some(singleton),
        _ => None,
    }
}

/// One of the three `Encoder` factories: the real JDK's singleton when there is
/// one, otherwise a fabricated stand-in built to the same field layout.
pub fn b64_encoder_factory(
    ctx: &mut dyn NativeContext,
    field_name: &str,
    variant: i32,
) -> MethodCallResult {
    match b64_jdk_singleton(ctx, B64_ENCODER_CLASS, field_name) {
        Some(singleton) => Ok(Some(Value::Object(Some(singleton)))),
        None => b64_alloc_encoder(ctx, variant, false),
    }
}

/// One of the three `Decoder` factories; see [`b64_encoder_factory`].
pub fn b64_decoder_factory(
    ctx: &mut dyn NativeContext,
    field_name: &str,
    variant: i32,
) -> MethodCallResult {
    match b64_jdk_singleton(ctx, B64_DECODER_CLASS, field_name) {
        Some(singleton) => Ok(Some(Value::Object(Some(singleton)))),
        None => b64_alloc_decoder(ctx, variant),
    }
}

pub fn native_b64_get_encoder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    b64_encoder_factory(ctx, "RFC4648", B64_VARIANT_BASIC)
}
pub fn native_b64_get_url_encoder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    b64_encoder_factory(ctx, "RFC4648_URLSAFE", B64_VARIANT_URL)
}
pub fn native_b64_get_mime_encoder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    b64_encoder_factory(ctx, "RFC2045", B64_VARIANT_MIME)
}
pub fn native_b64_get_decoder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    b64_decoder_factory(ctx, "RFC4648", B64_VARIANT_BASIC)
}
pub fn native_b64_get_url_decoder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    b64_decoder_factory(ctx, "RFC4648_URLSAFE", B64_VARIANT_URL)
}
pub fn native_b64_get_mime_decoder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    b64_decoder_factory(ctx, "RFC2045", B64_VARIANT_MIME)
}

/// Helper: read byte[] arg into a Vec<u8>
pub fn b64_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            bytes.push(b as u8);
        }
    }
    bytes
}

/// Helper: write Vec<u8> into a new byte[] on the heap
pub fn b64_write_byte_array(ctx: &mut dyn NativeContext, data: &[u8]) -> ObjectRef {
    use cratonvm_types::ArrayElementType;
    let result = ctx.new_array(ArrayElementType::Byte, data.len());
    for (i, &b) in data.iter().enumerate() {
        ctx.set_array_element(result, i, Value::Int(b as i32));
    }
    result
}

/// Everything real `Base64$Encoder` carries, read off the receiver's four
/// JDK-aligned fields rather than collapsed into a three-valued variant tag.
///
/// A tag cannot express this family. `isURL` picks the ALPHABET and `linemax` /
/// `newline` control LINE WRAPPING, and in the real JDK the two are
/// independent — collapsing them meant the wrapping of every encoder was
/// inferred from a tag instead of read, so a custom `linemax` could not
/// survive the trip. Measured shapes (`scratchpad/e14/B64Line.java`, all four
/// fields via reflection):
///
/// ```text
/// getEncoder()                  linemax=-1  isURL=false  newline=null       60B -> 80
/// getUrlEncoder()               linemax=-1  isURL=true   newline=null       60B -> 80
/// getMimeEncoder()              linemax=76  isURL=false  newline=[13,10]    60B -> 82
/// getMimeEncoder(20,{'\n'})     linemax=20  isURL=false  newline=[10]       60B -> 83
/// getMimeEncoder(20,{'\n'})
///     .withoutPadding()         linemax=20  isURL=false  newline=[10]       60B -> 83
/// ```
pub struct B64EncoderShape {
    pub is_url: bool,
    pub linemax: i32,
    pub newline: Vec<u8>,
    pub no_padding: bool,
}

impl B64EncoderShape {
    pub fn encode(&self, input: &[u8]) -> Vec<u8> {
        b64_encode_wrapped(
            input,
            self.is_url,
            self.no_padding,
            self.linemax,
            &self.newline,
        )
    }
}

/// Read the encoder shape off `this`, or throw the `NullPointerException`
/// HotSpot throws.
///
/// A `linemax > 0` encoder with a null `newline` is not merely unusual, it is
/// FATAL on the real JDK — and fatal on the very first call, not only on inputs
/// long enough to wrap, because `Encoder.encodedOutLength` sizes the
/// destination with
///
/// ```text
/// if (linemax > 0) len += (len - 1) / linemax * newline.length;
/// ```
///
/// before `encode0` ever runs. Measured by constructing that exact shape
/// through the private constructor (`scratchpad/e14/B64Fab.java`) — this is
/// what CratonVM's fabricated MIME encoder looked like to real bytecode until
/// `b64_alloc_encoder` started writing field 0:
///
/// ```text
/// Encoder(false, null, 76, true).encodeToString(30 bytes / 40 chars, no wrap needed)
///   -> java.lang.NullPointerException / Cannot read the array length because "this.newline" is null
/// ...identically for 57B, 60B, encode([B), encode([B[B), encode(ByteBuffer), wrap(OutputStream).
/// ```
///
/// Reproducing the throw rather than defaulting to CRLF matters because the two
/// are indistinguishable to a caller that only ever wraps: defaulting would give
/// a plausible answer for an object the JDK considers malformed, which is the
/// same "quiet wrong answer" shape as the null-argument defect this family was
/// last fixed for.
pub fn b64_encoder_shape(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<B64EncoderShape, MethodCallFailed> {
    let linemax = match ctx.get_field(this, B64_ENCODER_FIELD_LINEMAX) {
        Value::Int(v) => v,
        _ => B64_NO_LINEMAX,
    };
    let newline = match ctx.get_field(this, B64_ENCODER_FIELD_NEWLINE) {
        Value::Object(Some(arr)) => b64_read_byte_array(ctx, arr),
        _ if linemax > 0 => return Err(RuntimeError::NullPointerException { message: None }.into()),
        _ => Vec::new(),
    };
    Ok(B64EncoderShape {
        is_url: matches!(ctx.get_field(this, B64_ENCODER_FIELD_IS_URL), Value::Int(v) if v != 0),
        linemax,
        newline,
        no_padding: b64_no_padding(ctx, this),
    })
}

/// Decoder variant from the real JDK's two booleans.
///
/// `isMIME` is tested first because `Decoder.RFC2045` is `(isURL=false,
/// isMIME=true)` and the ordering only matters for a `(true, true)` decoder,
/// which no public factory can produce — `getUrlDecoder()` is `(true, false)`,
/// `getMimeDecoder()` is `(false, true)`, and the constructor is private. Such
/// a decoder would use the URL alphabet AND ignore junk; this file's
/// three-valued tag cannot say that, and `b64_decode` — verified 36/36 against
/// HotSpot over the malformed-input matrix and deliberately not touched here —
/// is written against the tag. Recorded rather than modelled.
pub fn b64_decoder_variant(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if matches!(ctx.get_field(this, B64_DECODER_FIELD_IS_MIME), Value::Int(v) if v != 0) {
        B64_VARIANT_MIME
    } else if matches!(ctx.get_field(this, B64_DECODER_FIELD_IS_URL), Value::Int(v) if v != 0) {
        B64_VARIANT_URL
    } else {
        B64_VARIANT_BASIC
    }
}

pub fn b64_no_padding(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    !matches!(ctx.get_field(this, B64_ENCODER_FIELD_DO_PADDING), Value::Int(v) if v != 0)
}

/// Receiver of an instance `Base64$Encoder` / `Base64$Decoder` native.
///
/// Two rules live here because every native in this family got both wrong:
///
/// 1. A null receiver is `NullPointerException` on HotSpot. The interpreter
///    throws it before an instance method body runs, so this arm is defensive
///    rather than reachable — but it must still have the right SHAPE.
/// 2. It must NOT be `Ok(None)`. Every native in this family has a descriptor
///    that returns a value (`[B`, `Ljava/lang/String;`,
///    `Ljava/util/Base64$Encoder;`), and `Ok(None)` is the VOID shape: it hands
///    the interpreter a value of the wrong kind instead of a result or a throw.
///    docs/feature-designs/native-dispatch-architecture.md
pub fn b64_receiver(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException { message: None }.into()),
    }
}

/// The `byte[]` / `String` argument of a `Base64` encode/decode native.
///
/// HotSpot 25 throws `NullPointerException` for a null argument on EVERY
/// overload — measured for all 13 encoder/decoder combinations, e.g.
///
/// ```text
/// Encoder.encode((byte[])null)  -> NPE / Cannot read the array length because "src" is null
/// Encoder.encodeToString(null)  -> NPE / Cannot read the array length because "src" is null
/// Decoder.decode((byte[])null)  -> NPE / Cannot read the array length because "src" is null
/// Decoder.decode((String)null)  -> NPE / Cannot invoke
///                                  "String.getBytes(java.nio.charset.Charset)" because "src" is null
/// ```
///
/// identically for the Url and Mime variants. The message is a HotSpot
/// helpful-NPE built from the callee's local-variable table, not part of the
/// specified contract, so `message: None` is left unset here; callers assert on
/// the TYPE (`RJdkIntrinsics2 --only=b64` checks `nameOf(t)`).
pub fn b64_arg(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.get(1) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException { message: None }.into()),
    }
}

pub fn native_b64_encode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = b64_receiver(args)?;
    let src = b64_arg(args)?;
    let shape = b64_encoder_shape(ctx, this)?;
    let bytes = b64_read_byte_array(ctx, src);
    let encoded = shape.encode(&bytes);
    let result = b64_write_byte_array(ctx, &encoded);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_b64_encode_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = b64_receiver(args)?;
    let src = b64_arg(args)?;
    let shape = b64_encoder_shape(ctx, this)?;
    let bytes = b64_read_byte_array(ctx, src);
    let encoded = shape.encode(&bytes);
    let s = String::from_utf8_lossy(&encoded);
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

/// Real `Encoder.withoutPadding()` is
/// `if (!doPadding) return this; return new Encoder(isURL, newline, linemax, false);`
///
/// It **copies the fields**, it does not re-derive them from a variant. This
/// used to call `b64_alloc_encoder(ctx, variant, true)`, which rebuilt the
/// encoder from a three-valued tag and so replaced any custom line wrapping with
/// the canonical 76/CRLF. Measured on HotSpot:
///
/// ```text
/// getMimeEncoder(20,{'\n'})                  linemax=20 newline=[10]  60B -> 83
/// getMimeEncoder(20,{'\n'}).withoutPadding() linemax=20 newline=[10]  60B -> 83
/// getMimeEncoder(20,{'\n'}).withoutPadding() 61B -> 86   (padded: 88)
/// ```
///
/// Note the copy shares the SAME `newline` array (real JDK passes the reference
/// straight through; `getMimeEncoder(int, byte[])` does not defensively copy the
/// caller's array either — measured: mutating it afterwards changes the
/// encoder's output). Copying the field VALUE reproduces that, and it is also
/// why no array is allocated here.
///
/// Identity: `withoutPadding()` on an already-unpadded encoder returns THIS
/// (`np.withoutPadding() == np` measured true), but on a padded one it returns a
/// FRESH object every call (`enc.withoutPadding() == enc.withoutPadding()`
/// measured **false**) — so unlike the six factories this one must not be
/// memoized.
pub fn native_b64_without_padding(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = b64_receiver(args)?;
    if b64_no_padding(ctx, this) {
        return Ok(Some(Value::Object(Some(this))));
    }
    // `this` crosses an allocating call, so it is rooted and re-read through the
    // collector-updated slot; see `b64_alloc_encoder` for the full hazard.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let copy =
        try_alloc_concurrent_synthetic(&mut *scope, B64_ENCODER_CLASS, B64_ENCODER_NUM_FIELDS)?;
    let copy_h = scope.root(copy);
    let this = scope.get(&this_h);
    let newline = scope.get_field(this, B64_ENCODER_FIELD_NEWLINE);
    let linemax = scope.get_field(this, B64_ENCODER_FIELD_LINEMAX);
    let is_url = scope.get_field(this, B64_ENCODER_FIELD_IS_URL);
    let copy = scope.get(&copy_h);
    scope.set_field(copy, B64_ENCODER_FIELD_NEWLINE, newline);
    scope.set_field(copy, B64_ENCODER_FIELD_LINEMAX, linemax);
    scope.set_field(copy, B64_ENCODER_FIELD_IS_URL, is_url);
    scope.set_field(copy, B64_ENCODER_FIELD_DO_PADDING, Value::Int(0));
    Ok(Some(Value::Object(Some(copy))))
}

pub fn native_b64_decode_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = b64_receiver(args)?;
    let src = b64_arg(args)?;
    let variant = b64_decoder_variant(ctx, this);
    let bytes = b64_read_byte_array(ctx, src);
    match b64_decode(&bytes, variant) {
        Ok(decoded) => {
            let result = b64_write_byte_array(ctx, &decoded);
            Ok(Some(Value::Object(Some(result))))
        }
        Err(msg) => Err(RuntimeError::IllegalArgumentException { message: msg }.into()),
    }
}

pub fn native_b64_decode_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = b64_receiver(args)?;
    let variant = b64_decoder_variant(ctx, this);
    // This was the FOURTH null-argument site, and the one a
    // `grep '"decode".*Ljava/lang/String'` misses because its registration
    // spans four lines. Defaulting a null argument to `String::new()` decoded
    // the EMPTY input instead of throwing, so `decode((String) null)` RETURNED
    // a zero-length `byte[]` where HotSpot throws NullPointerException —
    // the one shape a caller cannot distinguish from a legitimate `decode("")`.
    let src = b64_arg(args)?;
    let src_str = ctx.read_string(src).unwrap_or_default();
    // Real `Decoder.decode(String)` is `decode(src.getBytes(ISO_8859_1))`: one
    // byte per UTF-16 unit, with anything above U+00FF replaced by `'?'`.
    // Handing it UTF-8 instead splits every non-ASCII char into a multi-byte
    // sequence, so the reported illegal character was the first continuation
    // byte rather than the real one (`"AB€D"` → `e2`, not `3f`).
    let src_bytes: Vec<u8> = src_str
        .encode_utf16()
        .map(|u| if u <= 0xFF { u as u8 } else { b'?' })
        .collect();
    match b64_decode(&src_bytes, variant) {
        Ok(decoded) => {
            let result = b64_write_byte_array(ctx, &decoded);
            Ok(Some(Value::Object(Some(result))))
        }
        Err(msg) => Err(RuntimeError::IllegalArgumentException { message: msg }.into()),
    }
}


// ===========================================================================
// Charset / StandardCharsets
// Charset = 1-field synthetic (field 0 = name String)
// ===========================================================================

pub const CHARSET_FIELD_NAME: usize = 0;

pub fn register_charset_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: Charset encode/decode (UTF-8 etc.) is spec-exact and matches
    // real JDK bytecode → Intrinsic.
            let cs = "java/nio/charset/Charset";
    let std_cs = "java/nio/charset/StandardCharsets";

    // --- Charset static methods ---
    registry.register(
        cs,
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        native_charset_for_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        cs,
        "defaultCharset",
        "()Ljava/nio/charset/Charset;",
        native_charset_default, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        cs,
        "isSupported",
        "(Ljava/lang/String;)Z",
        native_charset_is_supported, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Charset instance methods ---
    registry.register(cs, "name", "()Ljava/lang/String;", native_charset_name, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        cs,
        "displayName",
        "()Ljava/lang/String;",
        native_charset_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(cs, "aliases", "()Ljava/util/Set;", native_charset_aliases, cratonvm_native_api::NativeKind::Intrinsic);
    // `contains(Charset)` is ABSTRACT on `java.nio.charset.Charset`; each
    // concrete `sun.nio.cs.*` answers for itself. CratonVM's `Charset.forName`
    // (`native_charset_for_name`, just above) hands back an instance of the
    // ABSTRACT class, so an `invokevirtual contains` resolved to the abstract
    // declaration and threw `AbstractMethodError` for every charset in real-JDK
    // mode — including `StandardCharsets.UTF_8.contains(US_ASCII)`, which
    // `String.getBytes`-style fast paths and Tomcat's `CharsetUtil` both ask.
    // Same shape as the CharBuffer abstract-method overrides further down, and
    // it shares the per-family answer table with the synthetic-jdk registrar
    // (`phases_late::charset_buffers::charset_contains`) so the two builds
    // cannot drift.
    registry.register(
        cs,
        "contains",
        "(Ljava/nio/charset/Charset;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let Some(Value::Object(Some(other))) = args.get(1).copied() else {
                // The concrete `sun.nio.cs` bodies are `instanceof` chains, so a
                // null argument NPEs there too.
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("Charset.contains: null charset".to_string()),
                }
                .into());
            };
            let a = crate::phases_late::charset_buffers::charset_name_field(ctx, this);
            let b = crate::phases_late::charset_buffers::charset_name_field(ctx, other);
            Ok(Some(Value::Int(i32::from(
                crate::phases_late::charset_buffers::charset_contains(&a, &b),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(cs, "toString", "()Ljava/lang/String;", native_charset_name, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(cs, "equals", "(Ljava/lang/Object;)Z", native_charset_equals, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(cs, "hashCode", "()I", native_charset_hash_code, cratonvm_native_api::NativeKind::Intrinsic);

    // --- StandardCharsets static fields ---
    registry.register(std_cs, "<clinit>", "()V", |ctx, _args| {
        // Populate the 6 static Charset fields during class initialization.
        // The field order matches the layout in synthetic_stub_fields:
        // UTF_8, US_ASCII, ISO_8859_1, UTF_16, UTF_16BE, UTF_16LE
        let charsets = [
            ("UTF_8", "UTF-8"),
            ("US_ASCII", "US-ASCII"),
            ("ISO_8859_1", "ISO-8859-1"),
            ("UTF_16", "UTF-16"),
            ("UTF_16BE", "UTF-16BE"),
            ("UTF_16LE", "UTF-16LE"),
        ];
        for (field_name, charset_name) in charsets {
            let obj = charset_concrete_or_synthetic(ctx, charset_name)?;
            ctx.set_static_field_by_name(
                "java/nio/charset/StandardCharsets",
                field_name,
                Value::Object(Some(obj)),
            );
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Round 58: CharBuffer abstract-method overrides for real-JDK mode.
    // Our `CharsetDecoder.decode(ByteBuffer)` native (see charset.rs)
    // returns a synthetic `java/nio/CharBuffer` instance whose layout is
    // (array=0 char[], pos=1, limit=2, capacity=3, mark=4) — matching
    // the synthetic-mode CB_FIELD_* in phases_late.rs. Real-JDK
    // `CharBuffer.get()C` is abstract (declared on the abstract class,
    // overridden in concrete subclasses HeapCharBuffer/StringCharBuffer/
    // …). Without a native here, an invokevirtual on the synthetic
    // CharBuffer instance resolves to the abstract declaration and
    // throws AbstractMethodError ("no Code attribute") — observed at
    // `org/apache/tomcat/util/buf/CharsetUtil.isAsciiSuperset:48` during
    // Spring Boot Tomcat startup for demo/eureka/insurance apps.
    //
    // The dispatch path in `invoke_on_class_shared_inner` already
    // promotes any abstract method to a native override if one is
    // registered (the `check_override = method.is_abstract() || …`
    // gate), so registering these is sufficient. The natives below
    // mirror the field layout used by `alloc_char_buffer` in
    // charset.rs (BUF_FIELD_ARRAY=0, BUF_FIELD_POS=1, BUF_FIELD_LIMIT=2).
    {
        let cb = "java/nio/CharBuffer";
        // Round 19 (nio19), NIO2-3: these three rows sit on ABSTRACT
        // `CharBuffer` declarations and serve only VM-minted carriers; they
        // are buffer shims, not charset arithmetic, so they take `Bridge`
        // explicitly rather than inheriting this function's `Intrinsic`
        // scope (the same restatement the `ByteBufferAsCharBuffer{B,L}` rows
        // below received on 2026-09-20). Restored after `put(C)`.
                
        // Read (hb, position, limit, offset) honouring the real-JDK
        // field names first and falling back to the synthetic indexed
        // layout (array=0, pos=1, limit=2). Returns (char[] arr, pos,
        // lim, offset).
        fn cb_state(
            ctx: &dyn cratonvm_native_api::NativeContext,
            this: ObjectRef,
        ) -> Option<(ObjectRef, i32, i32, i32)> {
            let arr = match ctx.get_field_by_name(this, "hb") {
                Value::Object(Some(a)) => Some(a),
                _ => match ctx.get_field(this, 0) {
                    Value::Object(Some(a)) => Some(a),
                    _ => None,
                },
            }?;
            let pos = match ctx.get_field_by_name(this, "position") {
                Value::Int(v) => v,
                _ => match ctx.get_field(this, 1) {
                    Value::Int(v) => v,
                    _ => 0,
                },
            };
            let lim = match ctx.get_field_by_name(this, "limit") {
                Value::Int(v) => v,
                _ => match ctx.get_field(this, 2) {
                    Value::Int(v) => v,
                    _ => 0,
                },
            };
            let off = match ctx.get_field_by_name(this, "offset") {
                Value::Int(v) => v,
                _ => 0,
            };
            Some((arr, pos, lim, off))        }

        fn cb_set_pos(ctx: &dyn cratonvm_native_api::NativeContext, this: ObjectRef, new_pos: i32) {
            ctx.set_field_by_name(this, "position", Value::Int(new_pos));
            ctx.set_field(this, 1, Value::Int(new_pos));
        }

        // Real-JDK `StringCharBuffer` has no `hb` char[] backing array. It
        // stores the wrapped CharSequence in `str` and uses Buffer
        // position/limit plus `offset` for indexing. The generic CharBuffer
        // natives below exist for synthetic char[]-backed buffers, but they
        // are also reached by inherited `CharBuffer.charAt(int)` on
        // StringCharBuffer. Honor the JDK layout instead of treating the null
        // hb as an error (Netty cookie decoding uses
        // `CharBuffer.wrap(String,start,end).charAt(0)`).
        // `&mut` rather than `&dyn`: the wrapped `str` is any `CharSequence`,
        // and reading a non-`String` one needs a virtual `toString()`. See
        // `charset_buffers::read_wrapped_char_sequence` — without it every
        // `CharBuffer.wrap(charChunk)` reads back empty.
        fn string_cb_state(
            ctx: &mut dyn cratonvm_native_api::NativeContext,
            this: ObjectRef,
        ) -> Option<(Vec<u16>, i32, i32, i32)> {
            let cid = ctx.class_id_of_object(this);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname != "java/nio/StringCharBuffer" {
                return None;
            }
            let str_obj = match ctx.get_field_by_name(this, "str") {
                Value::Object(Some(o)) => o,
                _ => return None,
            };
            let text =
                crate::phases_late::charset_buffers::read_wrapped_char_sequence(ctx, str_obj);
            let pos = match ctx.get_field_by_name(this, "position") {
                Value::Int(v) => v,
                _ => match ctx.get_field(this, 1) {
                    Value::Int(v) => v,
                    _ => 0,
                },
            };
            let lim = match ctx.get_field_by_name(this, "limit") {
                Value::Int(v) => v,
                _ => match ctx.get_field(this, 2) {
                    Value::Int(v) => v,
                    _ => 0,
                },
            };
            let off = match ctx.get_field_by_name(this, "offset") {
                Value::Int(v) => v,
                _ => 0,
            };
            Some((text.encode_utf16().collect(), pos, lim, off))        }

        #[allow(dead_code)]
        fn string_cb_char_at(
            ctx: &mut dyn cratonvm_native_api::NativeContext,
            this: ObjectRef,
            absolute_index: i32,
        ) -> Option<Value> {
            let (chars, _pos, _lim, off) = string_cb_state(ctx, this)?;
            let real = absolute_index + off;
            if real < 0 || real as usize >= chars.len() {
                return None;
            }
            Some(Value::Int(chars[real as usize] as i32))
        }

        // get()C — advance pos, return char at pos.
        registry.register(cb, "get", "()C", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("CharBuffer.get() on null".into()),
                    }
                    .into())
                }
            };
            if let Some((chars, pos, lim, off)) = string_cb_state(ctx, this) {
                if pos >= lim {
                    return Err(RuntimeError::BufferUnderflowException.into());
                }
                let real = pos + off;
                if real < 0 || real as usize >= chars.len() {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("index: {pos}"),
                    }
                    .into());
                }
                cb_set_pos(ctx, this, pos + 1);
                return Ok(Some(Value::Int(chars[real as usize] as i32)));
            }
            let (arr, pos, lim, off) = match cb_state(ctx, this) {
                Some(s) => s,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CharBuffer has no backing array".into(),
                    }
                    .into())
                }
            };
            if pos >= lim {
                return Err(RuntimeError::BufferUnderflowException.into());
            }
            let ch = ctx.get_array_element(arr, (pos + off) as usize);
            cb_set_pos(ctx, this, pos + 1);
            Ok(Some(ch))
        }, cratonvm_native_api::NativeKind::Bridge);
        // get(I)C — absolute get.
        registry.register(cb, "get", "(I)C", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("CharBuffer.get(I) on null".into()),
                    }
                    .into())
                }
            };
            let idx = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            // ByteBufferAsCharBuffer{B,L} fallback — see `charAt(I)C` below.
            let cid = ctx.class_id_of_object(this);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname == "java/nio/ByteBufferAsCharBufferB"
                || cname == "java/nio/ByteBufferAsCharBufferL"
            {
                let ch = bbacb_char_at(ctx, this, idx)?;
                return Ok(Some(Value::Int(ch as i32)));
            }
            if let Some((chars, _pos, lim, off)) = string_cb_state(ctx, this) {
                if idx < 0 || idx >= lim {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("index: {idx}"),
                    }
                    .into());
                }
                let real = idx + off;
                if real < 0 || real as usize >= chars.len() {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("index: {idx}"),
                    }
                    .into());
                }
                return Ok(Some(Value::Int(chars[real as usize] as i32)));
            }
            let (arr, _pos, lim, off) = match cb_state(ctx, this) {
                Some(s) => s,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CharBuffer has no backing array".into(),
                    }
                    .into())
                }
            };
            if idx < 0 || idx >= lim {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("index: {idx}"),
                }
                .into());
            }
            Ok(Some(ctx.get_array_element(arr, (idx + off) as usize)))
        }, cratonvm_native_api::NativeKind::Bridge);
        // put(C)Ljava/nio/CharBuffer; — relative put.
        registry.register(cb, "put", "(C)Ljava/nio/CharBuffer;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("CharBuffer.put(C) on null".into()),
                    }
                    .into())
                }
            };
            let ch = args.get(1).copied().unwrap_or(Value::Int(0));
            let (arr, pos, lim, off) = match cb_state(ctx, this) {
                Some(s) => s,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CharBuffer has no backing array".into(),
                    }
                    .into())
                }
            };
            if pos >= lim {
                return Err(RuntimeError::BufferOverflowException.into());
            }
            ctx.set_array_element(arr, (pos + off) as usize, ch);
            cb_set_pos(ctx, this, pos + 1);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge);
                /// What `CharBuffer.charAt(int)` raises out of range.
        ///
        /// `CharBuffer.charAt` is `get(position() + index)`, so it lands in
        /// `Buffer.checkIndex`, whose formatter builds an
        /// `IndexOutOfBoundsException` with **no** detail message. These
        /// natives used to raise `IllegalArgumentException("charAt index: N")`
        /// — not a subclass of what the caller catches, not even in the same
        /// hierarchy, so `catch (IndexOutOfBoundsException)` around a
        /// `CharSequence` walk missed it entirely and the exception escaped as
        /// an unrelated failure. See
        /// `native-builtins/src/preconditions.rs` for the formatter contract
        /// and `probes/PreconditionsFormatterProbe` for the HotSpot rows.
        fn char_buffer_index_out_of_bounds() -> RuntimeError {
            RuntimeError::ioobe_no_message()
        }

        // ByteBufferAsCharBuffer{B,L} — the JDK's view-buffer class
        // created by `HeapByteBuffer.asCharBuffer()`. Unlike our
        // synthetic CharBuffer (slot 0 = char[] / `hb` set), the JDK
        // class stores the underlying ByteBuffer in `bb` and reads
        // chars by decoding 2 bytes at a time from `bb.hb`. The CharBuffer
        // base class's `hb` is left null, so the abstract-class natives
        // above (which only know about `hb`) cannot service these
        // instances. Register dedicated natives for both endianness
        // variants that read `bb.hb` (byte[]) directly. Without this,
        // icu4j's `ICUResourceBundleReader.getStringV2` chain
        // (`bytes.asCharBuffer().charAt(I)` /
        // `bytes.asCharBuffer().subSequence(...).toString()`) trips
        // `IllegalStateException("CharBuffer has no backing array")`.
        fn bbacb_read_underlying_bytes(
            ctx: &dyn cratonvm_native_api::NativeContext,
            this: ObjectRef,
        ) -> Option<(ObjectRef, i32, i32, i32, bool)> {
            // `bb` is the underlying ByteBuffer (Ljava/nio/ByteBuffer;).
            let bb_obj = match ctx.get_field_by_name(this, "bb") {
                Value::Object(Some(o)) => o,
                _ => return None,
            };
            // `bb.hb` is the byte[] backing store.
            let byte_arr = match ctx.get_field_by_name(bb_obj, "hb") {
                Value::Object(Some(a)) => a,
                _ => match ctx.get_field(bb_obj, 0) {
                    Value::Object(Some(a)) => a,
                    _ => return None,
                },
            };
            let pos = match ctx.get_field_by_name(this, "position") {
                Value::Int(v) => v,
                _ => match ctx.get_field(this, 1) {
                    Value::Int(v) => v,
                    _ => 0,
                },
            };
            let lim = match ctx.get_field_by_name(this, "limit") {
                Value::Int(v) => v,
                _ => match ctx.get_field(this, 2) {
                    Value::Int(v) => v,
                    _ => 0,
                },
            };
            // Byte index of the view's element 0 inside `bb.hb`.
            //
            // The JDK's own answer is the view's `address`: every accessor on
            // `ByteBufferAsCharBuffer*` computes `(i << 1) + address`, and the
            // ctor seeds `address` to the SOURCE buffer's address plus its
            // position at the moment the view was made. So the view's element 0
            // is at `address - ARRAY_BYTE_BASE_OFFSET` bytes into `hb`, which
            // folds in both the source's array-base `offset` AND its position.
            //
            // Reading `bb.offset` instead — what this did — drops the source's
            // position: `bb.position(4).asCharBuffer().get(0)` decoded byte 0
            // rather than byte 4. It also goes stale, because the source's
            // position moves after the view is taken and the view must not
            // follow it.
            let bb_off = match ctx.get_field_by_name(this, "address") {
                Value::Long(addr) if addr >= 16 => (addr - 16) as i32,
                _ => match ctx.get_field_by_name(bb_obj, "offset") {
                    Value::Int(v) => v,
                    _ => 0,
                },
            };
            // Class name suffix tells us endianness: `B` = big-endian,
            // `L` = little-endian.
            let cid = ctx.class_id_of_object(this);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            let big_endian = cname.ends_with('B');
            Some((byte_arr, pos, lim, bb_off, big_endian))
        }

        /// One read, TWO index contracts — which is the whole point of the
        /// `relative` flag.
        ///
        /// `CharBuffer.charAt(int i)` is `CharSequence`'s, and the JDK's body is
        /// literally `get(position() + checkIndex(i, 1))` — RELATIVE to the
        /// current position. `CharBuffer.get(int index)` is the buffer's own
        /// ABSOLUTE accessor: `Objects.checkIndex(index, limit)` and no
        /// position anywhere.
        ///
        /// Both registrations called this helper and it did `pos + idx`, so
        /// `get(int)` was silently position-relative. MEASURED against HotSpot
        /// (`probes/L4TypedBufferSweep.java`, row `char/view get(out,1,2)`):
        ///
        /// ```text
        ///   CharBuffer cb = ByteBuffer.allocate(8).asCharBuffer();  // "abcd"
        ///   cb.flip(); cb.get();            // position is now 1
        ///   cb.get(new char[4], 1, 2);
        ///     HotSpot   .bc.      CratonVM  .cd.
        /// ```
        ///
        /// It reads as a bulk-copy defect and is not one: the position
        /// bookkeeping was right at every step (1 after the single get, 3
        /// after the bulk), and `IntBuffer`/`ShortBuffer` views were correct.
        /// `CharBuffer.get(char[],int,int)` has no native, so the REAL bytecode
        /// ran and called the absolute `get(int)` once per element — with the
        /// right indices, onto a body that added the position to them again.
        /// **The wrong characters were returned with every visible number
        /// correct.**
        ///
        /// This is an `Intrinsic`, so `--real-jdk` does not drop it and the
        /// defect was identical in both modes.
        fn bbacb_char_at_impl(
            ctx: &dyn cratonvm_native_api::NativeContext,
            this: ObjectRef,
            idx: i32,
            relative: bool,
        ) -> Result<u16, RuntimeError> {
            let (byte_arr, pos, lim, bb_off, big_endian) = bbacb_read_underlying_bytes(ctx, this)
                .ok_or(
                RuntimeError::IllegalStateException {
                    message: "ByteBufferAsCharBuffer: missing underlying bb.hb".into(),
                },
            )?;
            let real = if relative { pos + idx } else { idx };
            if idx < 0 || real >= lim {
                return Err(char_buffer_index_out_of_bounds());
            }
            let byte_idx = (bb_off + 2 * real) as usize;
            let hi = ctx
                .get_array_element(byte_arr, byte_idx)
                .as_int()
                .unwrap_or(0)
                & 0xFF;
            let lo = ctx
                .get_array_element(byte_arr, byte_idx + 1)
                .as_int()
                .unwrap_or(0)
                & 0xFF;
            let ch = if big_endian {
                ((hi << 8) | lo) as u16
            } else {
                ((lo << 8) | hi) as u16
            };
            Ok(ch)
        }

        /// `charAt(int)` — RELATIVE to the position (`CharSequence`'s contract).
        fn bbacb_char_at(
            ctx: &dyn cratonvm_native_api::NativeContext,
            this: ObjectRef,
            idx: i32,
        ) -> Result<u16, RuntimeError> {
            bbacb_char_at_impl(ctx, this, idx, true)
        }

        /// `get(int)` — ABSOLUTE (`Buffer`'s contract).
        fn bbacb_char_absolute(
            ctx: &dyn cratonvm_native_api::NativeContext,
            this: ObjectRef,
            idx: i32,
        ) -> Result<u16, RuntimeError> {
            bbacb_char_at_impl(ctx, this, idx, false)
        }

        // THE KIND HERE IS NOT THE CHARSET SCOPE'S, AND IT USED TO BE.
        //
        // `register_charset_natives` opens with `set_category(Intrinsic)` and a
        // census tag that says why: *"Charset encode/decode (UTF-8 etc.) is
        // spec-exact and matches real JDK bytecode"*. That scope runs to the end
        // of the function, and these buffer-view rows are inside it, so all five
        // (x2 classes) were tagged `Intrinsic` by inheritance --
        // `kind_stated: false`, `kind_chosen: true`, exactly the shape
        // `NativeCensusEntry::kind_stated`'s doc calls "a `set_category` line
        // that covered more registrations than its author was thinking about".
        //
        // The consequence is not cosmetic. `register`'s retirement re-tag only
        // fires when `effective_category() == Bridge`, so a table row on an
        // `Intrinsic` triple is INERT -- present, sorted, reachable, and doing
        // nothing. (The `PrintStream.charset` guard in `retired_shadow.rs` says
        // the same thing about the same scope, one registrar down.) MEASURED
        // 2026-09-20 on linux/x86_64: wave 19 put these eleven rows in its table
        // and the census still reported them `intrinsic`, `owns_slot: true`,
        // un-retired, while the other 51 rows of the same table moved.
        //
        // They are ordinary §1.4 shadows: `ByteBufferAsCharBuffer{B,L}` and
        // `CharBuffer` are real classes in every supported image, with `Code`,
        // and `L4CharBufferSweep` / `L4TypedBufferSweep` read their values back
        // over both byte orders. So the kind is stated here as `Bridge`, and the
        // wave-19 table retires them.
                        for bbacb in &[
            "java/nio/ByteBufferAsCharBufferB",
            "java/nio/ByteBufferAsCharBufferL",
        ] {
            registry.register(bbacb, "charAt", "(I)C", |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("ByteBufferAsCharBuffer.charAt(I) on null".into()),
                        }
                        .into())
                    }
                };
                let idx = match args.get(1) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                };
                let ch = bbacb_char_at(ctx, this, idx)?;
                Ok(Some(Value::Int(ch as i32)))
            }, cratonvm_native_api::NativeKind::Bridge);
            registry.register(bbacb, "get", "(I)C", |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("ByteBufferAsCharBuffer.get(I) on null".into()),
                        }
                        .into())
                    }
                };
                let idx = match args.get(1) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                };
                // ABSOLUTE — see `bbacb_char_at_impl`. This shared
                // `bbacb_char_at` with `charAt` and was therefore relative.
                let ch = bbacb_char_absolute(ctx, this, idx)?;
                Ok(Some(Value::Int(ch as i32)))
            }, cratonvm_native_api::NativeKind::Bridge);
            // get()C — the RELATIVE getter, and the one method on this class
            // whose out-of-range answer is NOT `IndexOutOfBoundsException`.
            //
            // `CharBuffer.get()` is `get(nextGetIndex())`, and
            // `Buffer.nextGetIndex()` is:
            //
            // ```java
            // if (position >= limit) throw new BufferUnderflowException();
            // return position++;
            // ```
            //
            // so the refusal is raised by the POSITION bookkeeping, before any
            // index reaches `checkIndex`. The absolute `get(int)`/`charAt(int)`
            // and `put(int, char)` on this same class go straight to
            // `Buffer.checkIndex` and keep `IndexOutOfBoundsException`, and the
            // relative `put(char)` answers `BufferOverflowException` — four
            // methods, three exception types, one class. MEASURED on HotSpot
            // 25.0.3+9 for `ByteBufferAsCharBuffer{B,L,RB}`, including through
            // `slice()` and `duplicate()` (which return the same class) and the
            // read-only `RB` form (which inherits this body).
            //
            // This delegated to `bbacb_char_at(ctx, this, 0)` and so reported
            // the absolute class for all four. `phases_late/nio_buffer.rs`'s
            // `relative::<WIDTH>` is the worked exemplar of the same split.
            //
            // `position` advances ONLY on success — HotSpot leaves it at 4
            // after a failed `get()` at the limit — which is why the check
            // precedes the read and the store follows it.
            registry.register(bbacb, "get", "()C", |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("ByteBufferAsCharBuffer.get() on null".into()),
                        }
                        .into())
                    }
                };
                // `position`/`limit` are read through the same helper the read
                // path uses, NOT by a bare `get_field_by_name`: it carries the
                // slot-index fallbacks for the layouts where the names do not
                // resolve. A `limit` that silently read 0 here would make every
                // relative `get()` throw, so the fallback is load-bearing.
                let (_, pos, lim, _, _) = match bbacb_read_underlying_bytes(ctx, this) {
                    Some(s) => s,
                    None => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "ByteBufferAsCharBuffer: missing underlying bb.hb".into(),
                        }
                        .into())
                    }
                };
                if pos >= lim {
                    return Err(RuntimeError::BufferUnderflowException.into());
                }
                let ch = bbacb_char_at(ctx, this, 0)?;
                ctx.set_field_by_name(this, "position", Value::Int(pos + 1));
                Ok(Some(Value::Int(ch as i32)))
            }, cratonvm_native_api::NativeKind::Bridge);
            // hasArray() — BBACB has no char[] backing array; return false.
            registry.register(bbacb, "hasArray", "()Z", |_ctx, _args| {
                Ok(Some(Value::Int(0)))
            }, cratonvm_native_api::NativeKind::Bridge);
            // subSequence(II) -- DELETED, deliberately.
            //
            // This used to return a fresh char[]-backed buffer stamped with
            // the ABSTRACT `java/nio/CharBuffer`, on the reasoning that a
            // copy is "sufficient for `subSequence(...).toString()`". It is
            // not: the JDK returns a slice of the same BBACB type over the
            // same bytes, so a copy loses write-through, reports
            // `hasArray() == true`, and -- the part that actually broke --
            // leaves every method with no native on the abstract class
            // throwing `AbstractMethodError`.
            //
            // Now that `asCharBuffer` hands back a real
            // `ByteBufferAsCharBuffer{B,L}` (see `s2_bb_as_char_buffer`),
            // the JDK's own `subSequence` body is present and correct,
            // including the `Objects.checkFromToIndex` bounds check this
            // override was originally written to add. Leaving the override
            // in place would shadow it and re-introduce all three
            // divergences.
            // toString() — read all chars between pos..lim.
            registry.register(bbacb, "toString", "()Ljava/lang/String;", |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
                };
                let (byte_arr, pos, lim, bb_off, big_endian) =
                    match bbacb_read_underlying_bytes(ctx, this) {
                        Some(s) => s,
                        None => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
                    };
                let mut chars: Vec<u16> = Vec::new();
                for i in pos..lim {
                    let bi = (bb_off + 2 * i) as usize;
                    let hi = ctx.get_array_element(byte_arr, bi).as_int().unwrap_or(0) & 0xFF;
                    let lo = ctx
                        .get_array_element(byte_arr, bi + 1)
                        .as_int()
                        .unwrap_or(0)
                        & 0xFF;
                    let ch = if big_endian {
                        ((hi << 8) | lo) as u16
                    } else {
                        ((lo << 8) | hi) as u16
                    };
                    chars.push(ch);
                }
                let s = String::from_utf16_lossy(&chars);
                Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
            }, cratonvm_native_api::NativeKind::Bridge);
        }

        // charAt(I)C — for CharSequence interop. The eleventh row of the note
        // above: `java/nio/CharBuffer.charAt` is bucket A (declared with `Code`
        // by the image) and inherited the same `Intrinsic`. Its three siblings
        // `get()C`, `get(I)C` and `put(C)` are bucket C -- abstract in the image
        // -- so no door dispatches a retirement of them and they are left as
        // they are.
        registry.register(cb, "charAt", "(I)C", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("CharBuffer.charAt(I) on null".into()),
                    }
                    .into())
                }
            };
            let idx = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            // If the receiver is a ByteBufferAsCharBuffer{B,L}, read
            // chars by decoding 2 bytes at a time from the underlying
            // ByteBuffer's `hb`. JDK creates BBACB instances from
            // `HeapByteBuffer.asCharBuffer()`; their CharBuffer.hb is
            // null and our cb_state would otherwise fail.
            let cid = ctx.class_id_of_object(this);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname == "java/nio/ByteBufferAsCharBufferB"
                || cname == "java/nio/ByteBufferAsCharBufferL"
            {
                let ch = bbacb_char_at(ctx, this, idx)?;
                return Ok(Some(Value::Int(ch as i32)));
            }
            if let Some((chars, pos, lim, off)) = string_cb_state(ctx, this) {
                let real = pos + idx;
                if idx < 0 || real >= lim {
                    return Err(char_buffer_index_out_of_bounds().into());
                }
                let str_idx = real + off;
                if str_idx < 0 || str_idx as usize >= chars.len() {
                    return Err(char_buffer_index_out_of_bounds().into());
                }
                return Ok(Some(Value::Int(chars[str_idx as usize] as i32)));
            }
            let (arr, pos, lim, off) = match cb_state(ctx, this) {
                Some(s) => s,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CharBuffer has no backing array".into(),
                    }
                    .into())
                }
            };
            let real = pos + idx;
            if idx < 0 || real >= lim {
                return Err(char_buffer_index_out_of_bounds().into());
            }
            Ok(Some(ctx.get_array_element(arr, (real + off) as usize)))
        }, cratonvm_native_api::NativeKind::Bridge);
            }

    // -----------------------------------------------------------------------
    // PrintStream.charset() — guarantee a non-null Charset.
    //
    // Tomcat 10.1 `Catalina.initStreams()` does `new SystemLogHandler(System.out)`.
    // `SystemLogHandler extends PrintStream`, so the super-ctor chain runs the
    // real-JDK `PrintStream(boolean, OutputStream)` bytecode. At pc 22 that
    // bytecode tests `out instanceof PrintStream`; because `out` IS `System.out`
    // (a PrintStream), it takes the pc-33 branch and calls `out.charset()`,
    // stores the result in its own `charset` field, then at pc 53/56 builds
    // `new OutputStreamWriter(out, charset)`. The real `OutputStreamWriter`
    // (OutputStream, Charset) ctor null-checks the charset at pc 6 and throws
    // `NullPointerException("charset")` when it is null.
    //
    // `PrintStream.charset()` is just `getfield charset`. The VM-pinned
    // `System.out` / `System.err` objects are allocated by
    // `SharedVm::ensure_system_streams` with only slot 0 (the fd id) populated;
    // their `charset` field is left null unless `System.initPhase1`'s
    // `install_charset` happens to stamp it. Rather than depend on that
    // stamping (which can silently fail if the `final charset` field cannot be
    // resolved by name at bootstrap time), override `charset()` so it can
    // never return null: read the declared `charset` field, and if it is
    // absent / null fall back to a fresh UTF-8 Charset (also writing it back so
    // later reads stay consistent). This makes every freshly-constructed
    // PrintStream — and any OutputStreamWriter built from one — receive a
    // valid non-null charset, matching real-JDK `Charset.defaultCharset()`.
    // STATED, not inherited. This row is an `Intrinsic` on purpose -- §5 of the
    // lane-4 page counts it as the class's one, and `retired_shadow.rs`'s
    // `wave_six_printstream_*` guard says a table row on it would be INERT
    // because `register`'s retirement re-tag only fires on a `Bridge`. Until
    // 2026-09-20 the kind came from `register_charset_natives`' enclosing
    // `set_category(Intrinsic)`, i.e. from a scope whose census tag is about
    // Charset encode/decode, so the census reported `kind_stated: false` for a
    // decision that HAS been reviewed. Saying it here is what makes the column
    // mean what it says.
    registry.register(
        "java/io/PrintStream",
        "charset",
        "()Ljava/nio/charset/Charset;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    // No receiver — still hand back a usable UTF-8 Charset
                    // rather than null so callers never NPE.
                    let cs = charset_concrete_or_synthetic(ctx, "UTF-8")?;
                    return Ok(Some(Value::Object(Some(cs))));
                }
            };
            // Honour an already-set charset (e.g. a PrintStream constructed
            // with an explicit charset, or one stamped by install_charset) --
            // but ONLY if it is concrete. `install_charset` runs at bootstrap,
            // before `Charset.forName` is safe to call, so it stamps a
            // hand-allocated `java/nio/charset/Charset`, and that class is
            // ABSTRACT: `newEncoder()` on it has no `Code` attribute. Returning
            // it here is what made `new OutputStreamWriter(System.err)` die with
            // `AbstractMethodError` (MEASURED: `probes/PrintStreamCharset.java`,
            // System.out and System.err in BOTH modes). Repair it instead --
            // this accessor runs lazily, long after bootstrap, so asking the
            // real `Charset.forName` is safe HERE even though it is not there.
            if let Value::Object(Some(cs)) = ctx.get_field_by_name(this, "charset") {
                let cid = ctx.class_id_of_object(cs);
                if !matches!(
                    ctx.class_name_of_id(cid).as_deref(),
                    Some("java/nio/charset/Charset")
                ) {
                    return Ok(Some(Value::Object(Some(cs))));
                }
                // Abstract stand-in: replace it, and write the repair back so
                // the direct `getfield charset` that real `PrintStream` bytecode
                // performs sees the concrete one too.
                let want = printstream_repair_encoding(&*ctx, Some(this));
                let fixed = charset_concrete_or_synthetic(ctx, &want)?;
                ctx.set_field_by_name(this, "charset", Value::Object(Some(fixed)));
                return Ok(Some(Value::Object(Some(fixed))));
            }
            // Field missing or null: resolve UTF-8 and write it back. This must
            // be a CONCRETE charset: the caller's next move is almost always
            // `new OutputStreamWriter(this)`, which asks a `PrintStream` for its
            // charset and then calls `newEncoder()` on it — abstract on the
            // fabricated base, so a stand-in here trades the NPE this override
            // exists to prevent for an `AbstractMethodError` one call later.
            let want = printstream_repair_encoding(&*ctx, Some(this));
            let cs = charset_concrete_or_synthetic(ctx, &want)?;
            ctx.set_field_by_name(this, "charset", Value::Object(Some(cs)));
            Ok(Some(Value::Object(Some(cs))))
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

/// Stubs for `org.apache.tomcat.jni.Library` (APR/tcnative). Real `tcnative-*.dll`
/// RegisterNatives + libffi dispatch is unsafe on Windows; `vm_exec` also refuses
/// `find_jni_native` / `resolve_jni_native_in_libraries` for this package.
// JDK-ONLY-CLASSIFY: stub — stated for the whole registrar, not adjudicated
// per row. Every one of these was among the 200 registrations the real boot
// made with NO category scope over them, which `--dump-native-registry`
// could not report until `current_category` became an `Option`: the old
// `category_chosen` flag was set by the first `set_category` in boot and
// never cleared, so everything after it claimed to have been chosen.
// `SyntheticStub` is the kind these carried before and after — verified by
// a census A/B — and it is the right one on the merits: `org.apache.tomcat.jni.*` is a third-party APR binding
// this VM fakes rather than links; there is no CratonVM boundary here to
// cross, only a `<clinit>` to satisfy.
pub fn register_tomcat_jni_natives(registry: &mut NativeMethodRegistry) {
    let lib = "org/apache/tomcat/jni/Library";

    registry.register(lib, "version", "(I)I", |_ctx, args| {
        let what = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let v = match what {
            0x01 => 2, // TCN_MAJOR_VERSION
            0x02 => 0, // TCN_MINOR_VERSION
            0x03 => 0, // TCN_PATCH_VERSION
            0x04 => 0, // TCN_IS_DEV_VERSION
            0x11 => 1, // APR_MAJOR_VERSION (must be >= 1)
            0x12 => 7, // APR_MINOR_VERSION
            0x13 => 0, // APR_PATCH_VERSION
            0x14 => 0, // APR_IS_DEV_VERSION
            _ => 0,
        };
        Ok(Some(Value::Int(v)))    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // KEEP: correct constant for the shim. `Library.has(flag)` asks whether an
    // APR/tcnative capability is compiled in; this whole surface IS the
    // capability set Tomcat probes, so every flag it asks about is "present".
    registry.register(lib, "has", "(I)Z", |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(lib, "size", "(I)I", |_ctx, args| {
        let what = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let v = match what {
            1 => 8,
            2 => 4096,
            3 => 256,
            4 => 1024,
            5 => 30,
            6 => 128 * 1024,
            7 => 8 * 1024 * 1024,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // KEEP: correct constant. `globalPool()` returns the APR global memory-pool
    // handle; we allocate no APR pools (see `terminate()` below), and 0 is APR's
    // own null-pool value.
    registry.register(lib, "globalPool", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        lib,
        "versionString",
        "()Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.create_string("2.0.38-cratonvm-stub"),
            ))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        lib,
        "aprVersionString",
        "()Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.create_string("1.7.0-cratonvm-stub"),
            ))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // KEEP: correct constant. `initialize()` reports success — there is nothing
    // to initialise, and reporting failure would abort AprLifecycleListener.
    registry.register(lib, "initialize", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // `Library.terminate()` tears down the APR pools + tcnative globals that
    // `initialize()` allocated. We never allocate them — `initialize()` above
    // just reports success — so there is nothing to tear down, and the JNI
    // entry point it would call does not exist in this process.
    registry.register(lib, "terminate", "()V", native_noop, cratonvm_native_api::NativeKind::SyntheticStub);

    // Tomcat 9.x `OS.<clinit>` calls native `is(int)` for every `IS_*` flag.
    // `AprLifecycleListener` then reflectively invokes `SSL.randSet`,
    // `SSL.initialize`, `SSL.version`, and the FIPS probes. Host
    // `tcnative-*.dll` symbols are ABI-incompatible with our libffi JNI bridge
    // on Windows (`vm_exec` blanks external resolution under
    // `org/apache/tomcat/jni/`); without Rust stubs the connector bootstrap can
    // fault with `STATUS_ACCESS_VIOLATION` immediately after Spring
    // `ConfigurationClassEnhancer` work (see `applogs/letsgo_windows_access_violation_analysis.txt`).
    let ssl = "org/apache/tomcat/jni/SSL";
    // `SSL.randSet(path)` points OpenSSL's PRNG at an entropy file. There is
    // no OpenSSL here (the whole `org/apache/tomcat/jni/SSL` surface is a
    // shim), and nothing in this process consumes that PRNG, so seeding it is
    // a no-op rather than a dropped security control.
    registry.register(ssl, "randSet", "(Ljava/lang/String;)V", native_noop, cratonvm_native_api::NativeKind::SyntheticStub);
    // KEEP: correct constant for the shim — non-zero reports success to
    // AprLifecycleListener, which only checks for an exception. There is no
    // OpenSSL in this process to initialise.
    registry.register(ssl, "initialize", "(Ljava/lang/String;)I", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(ssl, "version", "()I", |_ctx, _args| {
        // OpenSSL 1.1.x-style packed version (used only for logging / FIPS gating).
        Ok(Some(Value::Int(0x1010107f)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        ssl,
        "versionString",
        "()Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.create_string("OpenSSL 1.1.1w-cratonvm-stub"),
            ))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // KEEP: correct constant. There is no OpenSSL here, so FIPS mode is
    // genuinely off; `fipsModeSet` below echoes back whatever it is handed
    // rather than pretending a mode change happened.
    registry.register(ssl, "fipsModeGet", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(ssl, "fipsModeSet", "(I)I", |_ctx, args| {
        let mode = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        Ok(Some(Value::Int(mode)))    }, cratonvm_native_api::NativeKind::SyntheticStub);

    let os = "org/apache/tomcat/jni/OS";
    registry.register(os, "is", "(I)Z", |_ctx, args| {
        let t = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let ok = if cfg!(target_os = "windows") {
            t == 3 || t == 4 // WIN32 / WIN64
        } else if cfg!(target_os = "linux") {
            t == 5 || t == 1 // LINUX or UNIX
        } else if cfg!(target_os = "macos") {
            t == 8 || t == 1 // MACOSX or UNIX
        } else {
            t == 1
        };
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    }

/// Public wrapper so vm_init.rs can register charset natives in real-JDK mode.
pub fn register_charset_natives_pub(registry: &mut NativeMethodRegistry) {
    register_charset_natives(registry);
    register_tomcat_jni_natives(registry);
    register_netty_internal_tcnative_natives(registry);
}

