// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A faithful native `java.text.DateFormat.format(Date)` for
//! `java.text.SimpleDateFormat` receivers.
//!
//! # Why this exists
//!
//! `org.apache.juli.TestOneLineFormatterPerformance` is a RATIO test: it
//! asserts Tomcat's `DateFormatCache` beats
//! `String.format("%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS", …)` over a million
//! iterations. A uniformly slow VM still passes it. CratonVM failed it because
//! the two sides are not uniformly slow — **we accelerate one of them and not
//! the other**:
//!
//! | | HotSpot | CratonVM | ratio |
//! |---|---|---|---|
//! | `String.format("%t…")` | 1277 ns | 2082 ns | 1.6x — served by our own intrinsic |
//! | `SimpleDateFormat.format(Date)` | 248 ns | 138,472 ns | **557x** — interpreted JDK bytecode |
//!
//! Measured by `probes/DateFormatCostProbe.java` / `DateFormatPathProbe.java`
//! on a fixed timestamp. Both VMs take the same branches (`useDateFormatSymbols
//! = false`, `locale = en_US`, `gregory`, `zeroDigit = '0'`), the native-call
//! census is ~5 natives per format (so this is not the native funnel), and
//! `--nojit` costs the same as the default (so the JIT is not engaged here).
//! It is simply this VM executing a few thousand JDK bytecodes.
//!
//! This module closes the asymmetry the same way `String.format`,
//! `String.replaceAll` and the collection natives already do: a faithful Rust
//! implementation of the common case, with the real bytecode as the fallback.
//!
//! # Faithfulness is CHECKED, not argued
//!
//! An alternate implementation of a JDK formatter has a large surface to get
//! subtly wrong (era, DST, month-name provenance, `zeroPaddingNumber`'s
//! truncation rules). Rather than reason about it, this module
//! **cross-checks itself against the real bytecode at run time**, once per
//! distinct *output shape*: the first time a given `(plan, month, weekday, era,
//! am/pm, dst-flag, negative-offset)` tuple is formatted, the native result is
//! compared against `format(Date, StringBuffer, FieldPosition)` run as
//! bytecode. A mismatch returns the BYTECODE answer and disables the fast path
//! for that plan permanently. So a bug here costs performance, never
//! correctness, and `CRATONVM_DBG=dateformat` names it.
//!
//! The tuple space is what actually varies the text (12 months x 7 weekdays x
//! 2 eras x 2 am/pm x 2 dst x 2 sign = 1344 worst case, one or two in a hot
//! loop), so the check is amortised to nothing while still covering every
//! branch that produces a different string.
//!
//! # What is NOT accelerated
//!
//! Anything outside the checked subset falls through to the real bytecode with
//! no behaviour change at all:
//!
//! * a receiver that is not exactly `java.text.SimpleDateFormat` (a subclass
//!   may override the virtual `format(Date, StringBuffer, FieldPosition)` that
//!   the `final` `format(Date)` dispatches to);
//! * a calendar that is not exactly `java.util.GregorianCalendar`, or one whose
//!   Gregorian cutover has been moved;
//! * instants before the default Gregorian cutover (1582-10-15), where
//!   `GregorianCalendar` switches to Julian rules;
//! * a `zeroDigit` other than `'0'` (Arabic-Indic digit locales);
//! * `forceStandaloneForm` (locales whose month names differ standalone);
//! * the pattern letters `z L F w W Y u` — zone display names, standalone
//!   months, and the week-number family, whose rules depend on
//!   `TimeZone`-provided strings or the calendar's `firstDayOfWeek` /
//!   `minimalDaysInFirstWeek`.
//!
//! # Not a contract §1.4 shadow, and it should stop being re-argued
//!
//! This module's single registration —
//! `java/text/DateFormat.format(Ljava/util/Date;)Ljava/lang/String;`, the only
//! row a census attributes to this file — is a native in front of image
//! bytecode, which is the shape §1.4 calls a shadow. It is nonetheless
//! `cratonvm_native_api::NativeKind::Intrinsic` rather than `Bridge`, deliberately, and so is
//! outside the `bridge_shadows_bytecode` population and exempt from the
//! `--real-jdk` yield.
//!
//! The reason is the "faithfulness is CHECKED" section above, restated as the
//! property a retirement wave needs: **this native cannot give an answer the
//! bytecode would not.** Every unsupported shape falls through to the bytecode
//! and every supported one is cross-checked against it, with a mismatch
//! returning the BYTECODE answer and disabling the fast path for good. So
//! retiring it changes no observable and costs the 557x it was written to
//! close. Confirmed against a 2026-08-11 census (`kind: intrinsic`, one row);
//! recorded so the next wave does not re-derive it from the "native in front
//! of `Code`" predicate alone. See
//! docs/feature-designs/native-dispatch-architecture.md §5.

use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::OnceLock;

use cratonvm_native_api::registry::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::obj_arg;

type MethodCallResult = Result<Option<Value>, MethodCallFailed>;

/// Milliseconds in a day.
const MS_PER_DAY: i64 = 86_400_000;

/// The default `GregorianCalendar` Gregorian cutover, 1582-10-15T00:00:00Z.
/// A calendar whose cutover has been moved is declined outright.
const DEFAULT_CUTOVER_MILLIS: i64 = -12_219_292_800_000;

/// 1583-01-01T00:00:00Z. The whole of 1582 is declined, not just the instants
/// before the cutover: that year is TEN DAYS SHORT (1582-10-04 Julian is
/// followed by 1582-10-15 Gregorian), so `GregorianCalendar`'s DAY_OF_MONTH and
/// DAY_OF_YEAR both diverge from proleptic-Gregorian arithmetic across it.
/// Caught by the run-time cross-check first — 196 cells of the parity battery
/// — and turned into an up-front decline so no formatter gets poisoned for it.
const FIRST_FULLY_GREGORIAN_MILLIS: i64 = -12_212_553_600_000;

/// One element of a compiled pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// Literal text, already unquoted.
    Literal(String),
    /// A run of `count` copies of pattern letter `letter`.
    Field { letter: char, count: usize },
}

/// The exact `pattern` `String` and `DateFormatSymbols` objects a [`Plan`]
/// was last proven against, and the collection epoch in which that held.
///
/// gc-common w34-a (`common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`,
/// row 1): the plan used to be validated by the identity HASHES of those two
/// objects. `applyPattern` installs a new `String` -- the same class as the
/// old one, which is exactly the pair the 8-byte header's 20-bit hash makes
/// collide every 2^20 mints -- so one collision kept the OLD plan and its
/// `verified` shapes, and `format` rendered the old pattern unchecked.
///
/// Nothing moves within one counted collection epoch
/// (`NativeContext::gc_address_epoch`, gci i1/wr: on G1 it also moves at the
/// `cleanup` reclaim, which books no collection), so within an epoch an address
/// names that very object. A collection since the stamp was taken makes it a
/// miss, and the slow path re-proves the plan by CONTENT (the pattern text
/// and every symbol string, [`plan_row_refresh`]) -- one decode per formatter
/// per collection -- rather than trusting a hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlanStamp {
    pattern_addr: usize,
    symbols_addr: usize,
    epoch: u64,
}

/// A compiled, accelerable pattern plus the receiver state it was compiled
/// against. Rebuilt whenever that state's CONTENT changes (see
/// [`PlanStamp`]).
#[derive(Debug, Clone)]
struct Plan {
    pattern: String,
    /// Which objects, in which epoch, the plan was last proven against. The
    /// row may be reached by a DIFFERENT formatter whose identity hash
    /// collides (the row key is the hash); the plan is a function of the
    /// pattern text and the symbol strings alone, so a colliding formatter
    /// either proves the same content (and may share the plan and its
    /// verified shapes) or rebuilds it.
    stamp: PlanStamp,
    pieces: std::sync::Arc<Vec<Piece>>,
    symbols: std::sync::Arc<Symbols>,
}

/// Per-`SimpleDateFormat` cached state, keyed by the receiver's identity hash.
struct Entry {
    plan: Option<Plan>,
    /// Set once a cross-check has disagreed: the fast path is off for good.
    poisoned: bool,
    /// Output shapes already cross-checked against the bytecode.
    verified: FxHashSet<ShapeKey>,
    /// Set once a parse cross-check has disagreed: the parse fast path is off for good.
    parse_poisoned: bool,
    /// Parse shapes already cross-checked against the bytecode.
    parse_verified: FxHashSet<ParseShapeKey>,
}

/// The `DateFormatSymbols` strings a plan can need, decoded once when the plan
/// is built rather than on every format.
///
/// Reading one month name per format cost two heap field reads plus a
/// `read_string` — which allocates a Rust `String` and decodes UTF-16 — and
/// `GenerationalHeap::is_object_address` (reached through every `get_field`)
/// was 8.9% of the native in a perf profile. These are safe to hold across
/// calls because the only public route to different symbols is
/// `setDateFormatSymbols`, which installs a CLONE — a new object, so a new
/// address, so a stamp miss and a content re-check (gc-common w34-a: equal
/// strings keep the plan, different ones rebuild it).
/// `probes/DateFormatParityProbe.java` covers that case explicitly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Symbols {
    eras: Vec<String>,
    months: Vec<String>,
    short_months: Vec<String>,
    weekdays: Vec<String>,
    short_weekdays: Vec<String>,
    ampms: Vec<String>,
}

fn read_string_array(ctx: &mut dyn NativeContext, obj: ObjectRef, slot: usize) -> Vec<String> {
    let Some(arr) = obj_slot(ctx, obj, slot) else {
        return Vec::new();
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => out.push(ctx.read_string(s).unwrap_or_default()),
            _ => out.push(String::new()),
        }
    }
    out
}

fn read_symbols(ctx: &mut dyn NativeContext, sl: &Slots, symbols: ObjectRef) -> Symbols {
    Symbols {
        eras: read_string_array(ctx, symbols, sl.dfs_eras),
        months: read_string_array(ctx, symbols, sl.dfs_months),
        short_months: read_string_array(ctx, symbols, sl.dfs_short_months),
        weekdays: read_string_array(ctx, symbols, sl.dfs_weekdays),
        short_weekdays: read_string_array(ctx, symbols, sl.dfs_short_weekdays),
        ampms: read_string_array(ctx, symbols, sl.dfs_ampms),
    }
}

/// The tuple that determines which *text* a plan emits. See the module doc.
type ShapeKey = (u8, u8, u8, u8, bool, bool, bool);
/// Semantic branch key for `parse`: `(month_gte_jul, hour_pm, lenient, tz_kind)`.
type ParseShapeKey = (bool, bool, bool, u8);

/// `(vm_identity, the formatter's identity hash)`.
///
/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 31): the key used to be the bare identity hash. Every heap numbers
/// identity hashes from the same seed, so two VMs in one process running the
/// same code line up their formatter, pattern-String and symbols hashes, and
/// VM B's formatter could reach VM A's plan (A's pattern) with B's output
/// shape already marked verified, skipping the cross-check. The VM is in the
/// key (and in the thread-local memo's key). Since gc-common w34-a a row is
/// no longer trusted on hashes at all: it answers only when its
/// [`PlanStamp`] names the formatter's current pattern and symbols objects,
/// and is otherwise re-proven by content, so a colliding formatter (the key
/// is still its hash) costs a re-check, never a wrong plan.
type PlanKey = (usize, i32);

fn plan_cache() -> &'static parking_lot::Mutex<FxHashMap<PlanKey, Entry>> {
    static C: OnceLock<parking_lot::Mutex<FxHashMap<PlanKey, Entry>>> = OnceLock::new();
    C.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Upper bound on [`plan_cache`] rows.
///
/// gc-common w19-g: the cache is keyed by the formatter's identity hash and
/// nothing ever removed a row, so the (very common) `new SimpleDateFormat(..)`
/// per call grew it by one row, with its decoded symbol tables, per formatter
/// ever used, in every VM of the process, for the life of the process. A row
/// is only a memo (its plan is re-derived from the receiver and every output
/// shape re-verified against the bytecode), so dropping all rows when the
/// bound is reached costs at most one re-verification per shape and changes
/// no answer. A poisoned row that is dropped is re-poisoned by its next
/// cross-check, which again returns the bytecode's answer.
const PLAN_CACHE_MAX_ROWS: usize = 4096;

/// The row for `this_id`, inserted (empty) when missing. Clears the whole map
/// first when inserting would exceed [`PLAN_CACHE_MAX_ROWS`].
fn plan_cache_row(cache: &mut FxHashMap<PlanKey, Entry>, this_id: PlanKey) -> &mut Entry {
    if cache.len() >= PLAN_CACHE_MAX_ROWS && !cache.contains_key(&this_id) {
        cache.clear();
    }
    cache.entry(this_id).or_insert_with(|| Entry {
        plan: None,
        poisoned: false,
        verified: Default::default(),
        parse_poisoned: false,
        parse_verified: Default::default(),
    })
}

/// What a plan row hands the formatter: its pieces, pattern text (for
/// diagnostics) and decoded symbols.
type PlanParts = (std::sync::Arc<Vec<Piece>>, String, std::sync::Arc<Symbols>);

/// `this_id`'s plan, but only when it was last proven against exactly the
/// objects `stamp` names (gc-common w34-a). `None` means "re-prove it":
/// decode the pattern and symbols (outside the lock) and call
/// [`plan_row_refresh`].
fn plan_row_current(
    cache: &mut FxHashMap<PlanKey, Entry>,
    this_id: PlanKey,
    stamp: PlanStamp,
) -> Option<PlanParts> {
    plan_cache_row(cache, this_id)
        .plan
        .as_ref()
        .filter(|p| p.stamp == stamp)
        .map(|p| (p.pieces.clone(), p.pattern.clone(), p.symbols.clone()))
}

/// Re-prove `this_id`'s row against the formatter's CURRENT pattern text and
/// symbol strings, decoded by the caller outside the cache lock, and stamp it
/// with `stamp`. The same content keeps the plan and every shape verified
/// against it (the plan is a function of that content alone), so a
/// collection costs one decode, not a re-verification against the bytecode.
/// Different content -- `applyPattern`, `setDateFormatSymbols`, or a
/// colliding formatter reaching this row -- compiles a new plan with nothing
/// verified. A pattern outside the supported subset leaves no plan.
fn plan_row_refresh(
    cache: &mut FxHashMap<PlanKey, Entry>,
    this_id: PlanKey,
    stamp: PlanStamp,
    text: String,
    syms: Symbols,
) {
    let entry = plan_cache_row(cache, this_id);
    if let Some(plan) = entry.plan.as_mut() {
        if plan.pattern == text && *plan.symbols == syms {
            plan.stamp = stamp;
            return;
        }
    }
    entry.verified.clear();
    entry.parse_verified.clear();
    entry.plan = compile_pattern(&text).map(|pieces| Plan {
        pattern: text,
        stamp,
        pieces: std::sync::Arc::new(pieces),
        symbols: std::sync::Arc::new(syms),
    });
}

/// Note that `shape` is about to be (or has been) proven for `this_id`'s plan
/// and answer whether it still needs its cross-check. When the row no longer
/// holds the plan `stamp` names (another thread re-proved it for other
/// content between our read and now), record nothing and check: the row's
/// `verified` set belongs to that other plan.
fn plan_row_note_shape(
    cache: &mut FxHashMap<PlanKey, Entry>,
    this_id: PlanKey,
    stamp: PlanStamp,
    shape: ShapeKey,
) -> bool {
    let entry = plan_cache_row(cache, this_id);
    if entry.plan.as_ref().is_some_and(|p| p.stamp == stamp) {
        entry.verified.insert(shape)
    } else {
        true
    }
}

/// The row's verified shapes, when its plan is still the one `stamp` names.
fn plan_row_verified(
    cache: &FxHashMap<PlanKey, Entry>,
    this_id: PlanKey,
    stamp: PlanStamp,
) -> Option<FxHashSet<ShapeKey>> {
    cache
        .get(&this_id)
        .filter(|e| e.plan.as_ref().is_some_and(|p| p.stamp == stamp))
        .map(|e| e.verified.clone())
}

fn plan_row_note_parse_shape(
    cache: &mut FxHashMap<PlanKey, Entry>,
    this_id: PlanKey,
    stamp: PlanStamp,
    shape: ParseShapeKey,
) -> bool {
    let entry = plan_cache_row(cache, this_id);
    if entry.plan.as_ref().is_some_and(|p| p.stamp == stamp) {
        entry.parse_verified.insert(shape)
    } else {
        true
    }
}

fn plan_row_parse_verified(
    cache: &FxHashMap<PlanKey, Entry>,
    this_id: PlanKey,
    stamp: PlanStamp,
) -> Option<FxHashSet<ParseShapeKey>> {
    cache
        .get(&this_id)
        .filter(|e| e.plan.as_ref().is_some_and(|p| p.stamp == stamp))
        .map(|e| e.parse_verified.clone())
}

/// The key of the one-entry thread-local [`LastPlan`]: the VM, the formatter's
/// ADDRESS and the plan's [`PlanStamp`]. All addresses, one epoch: exact
/// (gc-common w34-a; it used to be three identity hashes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LastPlanKey {
    /// Keeps a thread that runs two VMs from reusing one VM's plan in the
    /// other (gc-common w29-d); the addresses and epoch are per VM.
    vm: usize,
    this_addr: usize,
    stamp: PlanStamp,
}

/// One-entry per-thread cache in front of [`plan_cache`].
///
/// The shared map costs two `Mutex` acquisitions and two hashes per format,
/// which is real money once a format is ~1.3 us. A formatter is almost always
/// used from one thread in a loop, so a single-entry thread-local keyed on
/// the formatter, its pattern and its symbols skips both locks entirely on
/// the steady-state path. It caches only what has ALREADY been proven against
/// the bytecode (`verified` is a copy, so a thread that has not yet seen a
/// shape still takes the slow path and cross-checks it) and is invalidated by
/// any change of those objects or by any collection ([`LastPlanKey`]).
struct LastPlan {
    key: LastPlanKey,
    calendar_addr: usize,
    zone_addr: usize,
    tz_id_addr: usize,
    zone_rules: Option<std::sync::Arc<crate::tzdb::ZoneRulesData>>,
    pieces: std::sync::Arc<Vec<Piece>>,
    symbols: std::sync::Arc<Symbols>,
    has_z: bool,
    zone_id: Option<std::sync::Arc<str>>,
    z_locale_addr: usize,
    z_names: [Option<String>; 4],
    last_verified: Option<ShapeKey>,
    verified: FxHashSet<ShapeKey>,
}

struct LastParsePlan {
    key: LastPlanKey,
    calendar_addr: usize,
    zone_addr: usize,
    tz_id_addr: usize,
    zone_is_zi: bool,
    zone_rules: Option<std::sync::Arc<crate::tzdb::ZoneRulesData>>,
    number_format_addr: usize,
    nf_symbols_addr: usize,
    pieces: std::sync::Arc<Vec<Piece>>,
    symbols: std::sync::Arc<Symbols>,
    has_z: bool,
    z_locale_addr: usize,
    z_names: [Option<String>; 4],
    last_verified: Option<ParseShapeKey>,
    verified: FxHashSet<ParseShapeKey>,
}

thread_local! {
    static LAST_PLAN: std::cell::RefCell<Option<LastPlan>> =
        const { std::cell::RefCell::new(None) };
    static LAST_PARSE_PLAN: std::cell::RefCell<Option<LastParsePlan>> =
        const { std::cell::RefCell::new(None) };
    /// Reused render buffer — one allocation per thread rather than per format.
    static SCRATCH: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn dbg_on() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v.split(',').any(|t| t.trim() == "dateformat"))
            .unwrap_or(false)
    })
}

/// Field slot indices and `ClassId`s, resolved once.
///
/// The first cut of this module used `get_field_by_name`/`set_field_by_name`
/// and `class_name_of_id` throughout. A perf profile of the hot loop put
/// `get_field_by_name` at 8.0%, `set_field_by_name` at 5.8%, `memcmp` at 4.1%
/// and `slot_for_exact` at 3.3% — i.e. a fifth of the native was re-resolving
/// the same twenty names, under the class-manager read lock, on every single
/// format. These are stable for the lifetime of the VM (a field's slot cannot
/// move; a class redefinition would give a NEW `ClassId`, which the
/// `class_id_of_object` comparison below catches), so resolve them once.
#[derive(Clone, Copy)]
struct Slots {
    sdf_class: cratonvm_types::ClassId,
    gcal_class: cratonvm_types::ClassId,
    date_class: cratonvm_types::ClassId,
    /// The `TimeZone` classes whose `getOffset(long)` this VM itself
    /// implements (`register_tzdb_offset_natives_for`). For those — and ONLY
    /// those — the offset can be read straight from the tzdb helper instead of
    /// dispatching into Java and paying a native-funnel entry. Any other
    /// receiver may be an application subclass with its own override, or a
    /// `java.util.SimpleTimeZone` whose id is an opaque LABEL rather than a
    /// zone, so it keeps the virtual call. C12-1 removed
    /// `java/util/SimpleTimeZone` from this set; do not restore it without
    /// reading `docs/feature-designs/native-dispatch-architecture.md`.
    zoneinfo_class: Option<cratonvm_types::ClassId>,
    timezone_class: Option<cratonvm_types::ClassId>,
    tz_id: usize,

    sdf_pattern: usize,
    sdf_format_data: usize,
    sdf_zero_digit: usize,
    sdf_force_standalone: usize,
    df_calendar: usize,

    gcal_cutover: usize,
    cal_zone: usize,
    cal_time: usize,
    cal_is_time_set: usize,
    cal_are_fields_set: usize,
    cal_are_all_fields_set: usize,

    date_fast_time: usize,
    date_cdate: usize,

    dfs_eras: usize,
    dfs_months: usize,
    dfs_short_months: usize,
    dfs_weekdays: usize,
    dfs_short_weekdays: usize,
    dfs_ampms: usize,
    dfs_locale: Option<usize>,
    dfs_is_zone_strings_set: Option<usize>,

    pp_class: Option<cratonvm_types::ClassId>,
    pp_index: Option<usize>,
    cal_lenient: Option<usize>,
    df_number_format: Option<usize>,
    sdf_original_number_format: Option<usize>,
    sdf_has_following_minus_sign: Option<usize>,
    dec_fmt_class: Option<cratonvm_types::ClassId>,
    dec_fmt_grouping_used: Option<usize>,
    dec_fmt_multiplier: Option<usize>,
    dec_fmt_use_exp: Option<usize>,
    dec_fmt_symbols: Option<usize>,
    dec_fmt_syms_zero_digit: Option<usize>,
    zoneinfo_raw_offset_diff: Option<usize>,
}

fn slots(ctx: &mut dyn NativeContext) -> Option<Slots> {
    thread_local! {
        static S: std::cell::Cell<Option<(usize, Option<Slots>)>> =
            const { std::cell::Cell::new(None) };
    }
    let vm = ctx.vm_identity();
    if vm != 0 {
        if let Some((cached_vm, cached)) = S.with(|c| c.get()) {
            if cached_vm == vm {
                return cached;
            }
        }
    }
    let resolved = (|| {
        let sdf_class = ctx.class_id_by_name("java/text/SimpleDateFormat")?;
        let gcal_class = ctx.class_id_by_name("java/util/GregorianCalendar")?;
        let date_class = ctx.class_id_by_name("java/util/Date")?;
        let dfs_class = ctx.class_id_by_name("java/text/DateFormatSymbols")?;
        let f = |cid: cratonvm_types::ClassId, name: &str| {
            ctx.resolve_field_index_by_class_id(cid, name)
        };
        let timezone_class = ctx.class_id_by_name("java/util/TimeZone");
        let zoneinfo_class = ctx.class_id_by_name("sun/util/calendar/ZoneInfo");
        let pp_class = ctx.class_id_by_name("java/text/ParsePosition");
        let dec_fmt_class = ctx.class_id_by_name("java/text/DecimalFormat");
        let dec_fmt_syms_class = ctx.class_id_by_name("java/text/DecimalFormatSymbols");
        Some(Slots {
            sdf_class,
            gcal_class,
            date_class,
            zoneinfo_class,
            timezone_class,
            tz_id: timezone_class.and_then(|c| ctx.resolve_field_index_by_class_id(c, "ID"))?,
            sdf_pattern: f(sdf_class, "pattern")?,
            sdf_format_data: f(sdf_class, "formatData")?,
            sdf_zero_digit: f(sdf_class, "zeroDigit")?,
            sdf_force_standalone: f(sdf_class, "forceStandaloneForm")?,
            df_calendar: f(sdf_class, "calendar")?,
            gcal_cutover: f(gcal_class, "gregorianCutover")?,
            cal_zone: f(gcal_class, "zone")?,
            cal_time: f(gcal_class, "time")?,
            cal_is_time_set: f(gcal_class, "isTimeSet")?,
            cal_are_fields_set: f(gcal_class, "areFieldsSet")?,
            cal_are_all_fields_set: f(gcal_class, "areAllFieldsSet")?,
            date_fast_time: f(date_class, "fastTime")?,
            date_cdate: f(date_class, "cdate")?,
            dfs_eras: f(dfs_class, "eras")?,
            dfs_months: f(dfs_class, "months")?,
            dfs_short_months: f(dfs_class, "shortMonths")?,
            dfs_weekdays: f(dfs_class, "weekdays")?,
            dfs_short_weekdays: f(dfs_class, "shortWeekdays")?,
            dfs_ampms: f(dfs_class, "ampms")?,
            dfs_locale: f(dfs_class, "locale"),
            dfs_is_zone_strings_set: f(dfs_class, "isZoneStringsSet"),
            pp_class,
            pp_index: pp_class.and_then(|c| f(c, "index")),
            cal_lenient: f(gcal_class, "lenient"),
            df_number_format: f(sdf_class, "numberFormat"),
            sdf_original_number_format: f(sdf_class, "originalNumberFormat"),
            sdf_has_following_minus_sign: f(sdf_class, "hasFollowingMinusSign"),
            dec_fmt_class,
            dec_fmt_grouping_used: dec_fmt_class.and_then(|c| f(c, "groupingUsed")),
            dec_fmt_multiplier: dec_fmt_class.and_then(|c| f(c, "multiplier")),
            dec_fmt_use_exp: dec_fmt_class.and_then(|c| f(c, "useExponentialNotation")),
            dec_fmt_symbols: dec_fmt_class.and_then(|c| f(c, "symbols")),
            dec_fmt_syms_zero_digit: dec_fmt_syms_class.and_then(|c| f(c, "zeroDigit")),
            zoneinfo_raw_offset_diff: zoneinfo_class.and_then(|c| f(c, "rawOffsetDiff")),
        })
    })();
    if vm != 0 {
        S.with(|c| c.set(Some((vm, resolved))));
    }
    resolved
}

// ---------------------------------------------------------------------------
// Pattern compilation
// ---------------------------------------------------------------------------

/// The JDK's pattern letters, in `DateFormatSymbols.patternChars` order.
/// Anything alphabetic and NOT in here is an `IllegalArgumentException` in
/// `SimpleDateFormat.compile()` — we decline instead and let the bytecode
/// raise it, so the exception's type, message and stack all stay the JDK's.
const PATTERN_CHARS: &str = "GyMdkHmsSEDFwWahKzZYuXL";

/// Pattern letters this module implements. The rest fall back — see the module
/// doc for why each is excluded.
fn letter_supported(c: char) -> bool {
    matches!(
        c,
        'G' | 'y'
            | 'M'
            | 'd'
            | 'k'
            | 'H'
            | 'm'
            | 's'
            | 'S'
            | 'E'
            | 'D'
            | 'a'
            | 'h'
            | 'K'
            | 'z'
            | 'Z'
            | 'X'
    )
}

/// Compile a pattern to `Piece`s, mirroring `SimpleDateFormat.compile()`'s
/// quoting rules. Returns `None` when the pattern is malformed or uses a letter
/// outside [`letter_supported`] — both mean "run the bytecode".
fn compile_pattern(pattern: &str) -> Option<Vec<Piece>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut pieces: Vec<Piece> = Vec::new();
    let mut literal = String::new();
    let mut i = 0usize;
    let mut in_quote = false;

    let flush = |literal: &mut String, pieces: &mut Vec<Piece>| {
        if !literal.is_empty() {
            pieces.push(Piece::Literal(std::mem::take(literal)));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // `''` is an escaped quote in BOTH modes; a lone `'` toggles.
            if i + 1 < chars.len() && chars[i + 1] == '\'' {
                literal.push('\'');
                i += 2;
                continue;
            }
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if in_quote {
            literal.push(c);
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() {
            if !PATTERN_CHARS.contains(c) {
                // The JDK throws IllegalArgumentException here. Decline so the
                // bytecode throws it, with its own message.
                return None;
            }
            if !letter_supported(c) {
                return None;
            }
            let mut count = 0usize;
            while i < chars.len() && chars[i] == c {
                count += 1;
                i += 1;
            }
            flush(&mut literal, &mut pieces);
            pieces.push(Piece::Field { letter: c, count });
            continue;
        }
        literal.push(c);
        i += 1;
    }
    if in_quote {
        // Unterminated quote — the JDK throws. Decline.
        return None;
    }
    flush(&mut literal, &mut pieces);
    Some(pieces)
}

// ---------------------------------------------------------------------------
// Civil date arithmetic (proleptic Gregorian, matching GregorianCalendar
// at and after the default 1582-10-15 cutover)
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 -> (year, month 1..=12, day 1..=31). Howard Hinnant's
/// `civil_from_days`, which is exact for the whole `i64` range and agrees with
/// `GregorianCalendar` wherever `GregorianCalendar` is Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Inverse of [`civil_from_days`]: (year, month 1..=12, day 1..=31) -> days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_adj = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * m_adj + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn day_of_year(y: i64, m: u32, d: u32) -> u32 {
    const CUM: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut doy = CUM[(m - 1) as usize] + d;
    if m > 2 && is_leap(y) {
        doy += 1;
    }
    doy
}

/// Broken-down local time, in the same field terms `Calendar` uses.
struct Fields {
    year: i64,
    month0: u32,      // 0-based, as Calendar.MONTH
    day: u32,         // DAY_OF_MONTH
    day_of_week: u32, // 1 = SUNDAY, as Calendar.DAY_OF_WEEK
    day_of_year: u32,
    hour_of_day: u32,
    minute: u32,
    second: u32,
    millis: u32,
}

fn fields_from_local_millis(local: i64) -> Fields {
    let days = local.div_euclid(MS_PER_DAY);
    let ms_of_day = local.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    Fields {
        year,
        month0: month - 1,
        day,
        // 1970-01-01 was a Thursday; Calendar spells SUNDAY as 1, so
        // THURSDAY is 5 and day 0 must map to 5.
        day_of_week: ((days + 4).rem_euclid(7) + 1) as u32,
        day_of_year: day_of_year(year, month, day),
        hour_of_day: (ms_of_day / 3_600_000) as u32,
        minute: (ms_of_day / 60_000 % 60) as u32,
        second: (ms_of_day / 1_000 % 60) as u32,
        millis: (ms_of_day % 1_000) as u32,
    }
}

// ---------------------------------------------------------------------------
// zeroPaddingNumber
// ---------------------------------------------------------------------------

/// `SimpleDateFormat.zeroPaddingNumber(value, min_digits, max_digits, buf)`
/// with `zeroDigit == '0'`.
///
/// Both of the JDK's arms reduce to the same rule for a non-negative value:
/// take the decimal digits, keep the RIGHTMOST `max_digits` of them, then
/// left-pad with zeros to `min_digits`. The fast arm handles 1-2 and 4 digit
/// cases inline; the slow arm reaches `DecimalFormat` with
/// `setMinimumIntegerDigits`/`setMaximumIntegerDigits`, whose integer-digit
/// maximum truncates from the left. `SimpleDateFormat` only ever passes
/// `max_digits` of 2 or `Integer.MAX_VALUE`.
fn zero_padding_number(out: &mut String, value: i64, min_digits: usize, max_digits: usize) {
    if value >= 0 {
        if min_digits == 2 && max_digits >= 2 && value < 100 {
            let v = value as u8;
            out.push((b'0' + v / 10) as char);
            out.push((b'0' + v % 10) as char);
            return;
        }
        if min_digits == 4 && max_digits >= 4 && (1000..10000).contains(&value) {
            let v = value as u16;
            out.push((b'0' + (v / 1000) as u8) as char);
            out.push((b'0' + ((v / 100) % 10) as u8) as char);
            out.push((b'0' + ((v / 10) % 10) as u8) as char);
            out.push((b'0' + (v % 10) as u8) as char);
            return;
        }
    }
    let mut abs = value.unsigned_abs();
    let mut buf = [0u8; 20];
    let mut pos = buf.len();
    loop {
        pos -= 1;
        buf[pos] = b'0' + (abs % 10) as u8;
        abs /= 10;
        if abs == 0 {
            break;
        }
    }
    let len = buf.len() - pos;
    let kept_start = if len > max_digits {
        buf.len() - max_digits
    } else {
        pos
    };
    let kept = &buf[kept_start..];
    if value < 0 {
        out.push('-');
    }
    for _ in kept.len()..min_digits {
        out.push('0');
    }
    // SAFETY: `kept` contains only ASCII digits `b'0'..=b'9'`.
    out.push_str(unsafe { std::str::from_utf8_unchecked(kept) });
}

// ---------------------------------------------------------------------------
// Reading the receiver
// ---------------------------------------------------------------------------

/// Everything the fast path needs off the Java objects, read once per call.
struct Inputs {
    millis: i64,
    offset_ms: i32,
    in_dst: bool,
    fields: Fields,
    calendar: ObjectRef,
    symbols: ObjectRef,
    vm_implemented: bool,
    zone: Option<ObjectRef>,
}

fn int_field(ctx: &mut dyn NativeContext, obj: ObjectRef, name: &str) -> Option<i32> {
    ctx.get_field_by_name(obj, name).as_int()
}

fn obj_field(ctx: &mut dyn NativeContext, obj: ObjectRef, name: &str) -> Option<ObjectRef> {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

fn obj_slot(ctx: &mut dyn NativeContext, obj: ObjectRef, slot: usize) -> Option<ObjectRef> {
    match ctx.get_field(obj, slot) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

fn class_name_is(ctx: &dyn NativeContext, obj: ObjectRef, want: &str) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(want)
}

/// Read a `String[]` element as a Rust `String`.
fn string_array_elem(ctx: &mut dyn NativeContext, arr: ObjectRef, index: usize) -> Option<String> {
    if index >= ctx.array_length(arr) {
        return None;
    }
    match ctx.get_array_element(arr, index) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The fast path
// ---------------------------------------------------------------------------

/// Render `pieces` for the given instant. `None` means "something in here is
/// outside the supported subset after all" — run the bytecode.
#[allow(clippy::too_many_arguments)]
fn render(pieces: &[Piece], inputs: &Inputs, syms: &Symbols, z_names: &[Option<String>; 4], out: &mut String) -> Option<()> {
    let f = &inputs.fields;
    out.clear();
    for piece in pieces {
        match piece {
            Piece::Literal(text) => out.push_str(text),
            Piece::Field { letter, count } => {
                let count = *count;
                match letter {
                    // 'G' — era. Guarded to AD by the cutover check, so index 1.
                    'G' => {
                        out.push_str(syms.eras.get(1)?);
                    }
                    'y' => {
                        if count == 2 {
                            zero_padding_number(out, f.year, 2, 2);
                        } else {
                            zero_padding_number(out, f.year, count, usize::MAX);
                        }
                    }
                    'M' => {
                        if count >= 4 {
                            out.push_str(syms.months.get(f.month0 as usize)?);
                        } else if count == 3 {
                            out.push_str(syms.short_months.get(f.month0 as usize)?);
                        } else {
                            zero_padding_number(out, f.month0 as i64 + 1, count, usize::MAX);
                        }
                    }
                    'd' => zero_padding_number(out, f.day as i64, count, usize::MAX),
                    'D' => zero_padding_number(out, f.day_of_year as i64, count, usize::MAX),
                    // 'k' — 1..24. GregorianCalendar's getMaximum(HOUR_OF_DAY) is 23.
                    'k' => {
                        let v = if f.hour_of_day == 0 {
                            24
                        } else {
                            f.hour_of_day
                        };
                        zero_padding_number(out, v as i64, count, usize::MAX);
                    }
                    'H' => zero_padding_number(out, f.hour_of_day as i64, count, usize::MAX),
                    // 'h' — 1..12. getLeastMaximum(HOUR) is 11.
                    'h' => {
                        let v = f.hour_of_day % 12;
                        let v = if v == 0 { 12 } else { v };
                        zero_padding_number(out, v as i64, count, usize::MAX);
                    }
                    // 'K' — 0..11, no wrap.
                    'K' => zero_padding_number(out, (f.hour_of_day % 12) as i64, count, usize::MAX),
                    'm' => zero_padding_number(out, f.minute as i64, count, usize::MAX),
                    's' => zero_padding_number(out, f.second as i64, count, usize::MAX),
                    'S' => zero_padding_number(out, f.millis as i64, count, usize::MAX),
                    'E' => {
                        let table = if count >= 4 {
                            &syms.weekdays
                        } else {
                            &syms.short_weekdays
                        };
                        out.push_str(table.get(f.day_of_week as usize)?);
                    }
                    'a' => {
                        out.push_str(syms.ampms.get(usize::from(f.hour_of_day >= 12))?);
                    }
                    // 'z' — general time zone display name (TimeZone.getDisplayName).
                    'z' => {
                        let idx = (usize::from(inputs.in_dst) << 1) | usize::from(count >= 4);
                        out.push_str(z_names[idx].as_deref()?);
                    }
                    // 'Z' — RFC 822, always +hhmm / -hhmm (5 chars with sign).
                    'Z' => {
                        let mins = inputs.offset_ms / 60_000;
                        let hhmm = (mins / 60) * 100 + (mins % 60);
                        if mins >= 0 {
                            out.push('+');
                            zero_padding_number(out, hhmm as i64, 4, 4);
                        } else {
                            // The JDK hands a NEGATIVE value to
                            // zeroPaddingNumber with width 5, so DecimalFormat
                            // emits the sign and pads the magnitude to 4.
                            out.push('-');
                            zero_padding_number(out, (-hhmm) as i64, 4, 4);
                        }
                    }
                    // 'X' — ISO 8601. The JDK throws for count > 3.
                    'X' => {
                        if count > 3 {
                            return None;
                        }
                        if inputs.offset_ms == 0 {
                            out.push('Z');
                        } else {
                            let mut mins = inputs.offset_ms / 60_000;
                            if mins >= 0 {
                                out.push('+');
                            } else {
                                out.push('-');
                                mins = -mins;
                            }
                            zero_padding_number(out, (mins / 60) as i64, 2, 2);
                            if count != 1 {
                                if count == 3 {
                                    out.push(':');
                                }
                                zero_padding_number(out, (mins % 60) as i64, 2, 2);
                            }
                        }
                    }
                    _ => return None,
                }
            }
        }
    }
    Some(())
}

// ---------------------------------------------------------------------------
// Fallback: run the real bytecode
// ---------------------------------------------------------------------------

/// `DateFormat.format(Date)`'s own non-`java.text` branch, run as bytecode:
/// `format(date, new StringBuffer(), DontCareFieldPosition.INSTANCE).toString()`.
/// Both branches of the real method produce the same string; the `StringBuf`
/// one is only an allocation optimisation.
fn format_via_bytecode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    date: Option<ObjectRef>,
) -> Result<Option<Value>, MethodCallFailed> {
    // Two allocations and two calls follow, any of which can move these
    // objects — root and re-read (family-1 shape).
    //
    // gc-common w19-g: one handle scope instead of paired pins, so the roots
    // are released on every exit; the old pins leaked on both constructors'
    // `?` paths (a Java exception from `<init>`).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let date_h = date.map(|d| scope.root(d));
    let sb_new = match scope.new_object_initialized("java/lang/StringBuffer", "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("StringBuffer <init> produced no object".to_string()),
            }
            .into());
        }
    };
    let sb_h = scope.root(sb_new);
    let fp =
        match scope.new_object_initialized("java/text/FieldPosition", "(I)V", &[Value::Int(0)])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("FieldPosition <init> produced no object".to_string()),
                }
                .into());
            }
        };
    let this_now = scope.get(&this_h);
    let date_now = match &date_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        // A null Date: hand it on so the JDK raises its own NPE, from its own
        // frame, with its own message.
        None => Value::Object(None),
    };
    let sb_now = scope.get(&sb_h);
    let result = scope.invoke_virtual_bytecode_only(
        this_now,
        "format",
        "(Ljava/util/Date;Ljava/lang/StringBuffer;Ljava/text/FieldPosition;)Ljava/lang/StringBuffer;",
        &[date_now, Value::Object(Some(sb_now)), Value::Object(Some(fp))],
    )?;
    let buf: ObjectRef = match result {
        Some(Value::Object(Some(o))) => o,
        other => return Ok(other),
    };
    scope.invoke_virtual(buf, "toString", "()Ljava/lang/String;", &[])
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Gather the receiver state the fast path needs, or `None` to decline.
///
/// `this` and `date` are handed back CURRENT on every return, `None`
/// included: the `getOffset` below may run Java, and so collect (gc-common
/// w34-a; they used to be taken by value and the caller went on reading
/// through the copies it held from before that call).
/// Whether a formatter's concrete class is `SimpleDateFormat` or a subclass
/// that inherits both the 3-arg `format(Date, StringBuffer, FieldPosition)`
/// and `useDateFormatSymbols()` un-overridden.
fn is_sdf_format_eligible(
    ctx: &dyn NativeContext,
    sl: &Slots,
    cid: cratonvm_types::ClassId,
) -> bool {
    if cid == sl.sdf_class {
        return true;
    }
    let vm = ctx.vm_identity();
    thread_local! {
        static CACHE: std::cell::RefCell<FxHashMap<(usize, cratonvm_types::ClassId), bool>> =
            std::cell::RefCell::new(FxHashMap::default());
    }
    if let Some(cached) = CACHE.with(|c| c.borrow().get(&(vm, cid)).copied()) {
        return cached;
    }
    let mut cur = Some(cid);
    let mut eligible = false;
    while let Some(c) = cur {
        if c == sl.sdf_class {
            eligible = true;
            break;
        }
        if ctx.class_declares_method(
            c,
            "format",
            "(Ljava/util/Date;Ljava/lang/StringBuffer;Ljava/text/FieldPosition;)Ljava/lang/StringBuffer;",
        ) || ctx.class_declares_method(c, "useDateFormatSymbols", "()Z")
        {
            eligible = false;
            break;
        }
        cur = ctx.superclass_of(c);
    }
    CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= 256 {
            map.clear();
        }
        map.insert((vm, cid), eligible);
    });
    eligible
}

fn gather(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    this: &mut ObjectRef,
    date: &mut ObjectRef,
) -> Option<Inputs> {
    if !is_sdf_format_eligible(ctx, sl, ctx.class_id_of_object(*this)) {
        return None;
    }
    if ctx.class_id_of_object(*date) != sl.date_class {
        return None;
    }
    // A locale whose digits are not ASCII '0'..'9' reaches DecimalFormat.
    match ctx.get_field(*this, sl.sdf_zero_digit).as_int() {
        // 0 means "not yet cached"; the bytecode caches it on first use, so
        // decline until it has.
        Some(z) if z == '0' as i32 => {}
        _ => return None,
    }
    if ctx
        .get_field(*this, sl.sdf_force_standalone)
        .as_int()
        .unwrap_or(1)
        != 0
    {
        return None;
    }

    let mut calendar = obj_slot(ctx, *this, sl.df_calendar)?;
    if ctx.class_id_of_object(calendar) != sl.gcal_class {
        return None;
    }
    // A moved cutover changes which rules apply where; only the default is
    // modelled.
    match ctx.get_field(calendar, sl.gcal_cutover) {
        Value::Long(v) if v == DEFAULT_CUTOVER_MILLIS => {}
        _ => return None,
    }
    let mut symbols = obj_slot(ctx, *this, sl.sdf_format_data)?;

    // `Date.getTime()` is `getTimeImpl()`, which normalises first when a
    // deprecated setter left `cdate` dirty. With no `cdate` there is nothing to
    // normalise and `fastTime` is the answer.
    if obj_slot(ctx, *date, sl.date_cdate).is_some() {
        return None;
    }
    let millis = match ctx.get_field(*date, sl.date_fast_time) {
        Value::Long(v) => v,
        _ => return None,
    };
    if millis < FIRST_FULLY_GREGORIAN_MILLIS {
        return None;
    }

    // The UTC offset — raw + DST, exactly what `GregorianCalendar.computeFields`
    // adds. For the two `TimeZone` classes this VM implements natively itself
    // the answer comes straight out of `tzdb`, saving a Java dispatch and a
    // native-funnel entry per format (~400-760 ns, measured). Anything else may
    // be an application subclass overriding `getOffset`, so it gets the real
    // virtual call.
    let zone = obj_slot(ctx, calendar, sl.cal_zone)?;
    let zone_class = ctx.class_id_of_object(zone);
    // `java/util/SimpleTimeZone` is deliberately NOT here — see
    // `docs/feature-designs/native-dispatch-architecture.md` and
    // `E1-1-simpledateformat-format-zone-arm-landed.md`. The fast arm below
    // answers from the zone's `ID` FIELD via tzdb. That is right for a
    // `ZoneInfo` (whose id IS the zone) and for the abstract `TimeZone` itself
    // (an instance of that exact class can only be one this VM fabricated). It
    // is wrong for a `java.util.SimpleTimeZone`, whose id is by contract an
    // opaque LABEL and whose offset is the `rawOffset` its constructor stored:
    // `new SimpleTimeZone(0, "America/Sao_Paulo")` formatted an instant at
    // -03:00 here and at +00:00 on HotSpot. Nothing in this VM fabricates a
    // `SimpleTimeZone` any more (`alloc_synth_timezone`) and nothing implements
    // its `getOffset` (`register_tzdb_offset_natives_for`), so every such
    // receiver is one the APPLICATION built. The `else` arm's virtual call
    // reaches its real bytecode, which is correct for it: `SimpleTimeZone`
    // DECLARES `getOffset(J)I` with code, so `invoke_or_native`'s
    // `has_own_bytecode` gate skips the superclass climb and
    // `java/util/TimeZone`'s surviving tzdb native does not capture it either.
    let vm_implemented =
        Some(zone_class) == sl.zoneinfo_class || Some(zone_class) == sl.timezone_class;
    let (offset_ms, dst_ms) = if vm_implemented {
        zone_offset_and_dst_cached(ctx, sl, zone, millis)?
    } else {
        // gc-common w34-a: `getOffset` is Java -- a GC point. Root every
        // object this call and its caller still use across it, and read them
        // all back (the oldest pin is released once, after the last read).
        let this_pin = ctx.pin_native_root(*this);
        let date_pin = ctx.pin_native_root(*date);
        let calendar_pin = ctx.pin_native_root(calendar);
        let symbols_pin = ctx.pin_native_root(symbols);
        let r = ctx.invoke_virtual(zone, "getOffset", "(J)I", &[Value::Long(millis)]);
        *this = ctx.read_native_pin(this_pin, *this);
        *date = ctx.read_native_pin(date_pin, *date);
        calendar = ctx.read_native_pin(calendar_pin, calendar);
        symbols = ctx.read_native_pin(symbols_pin, symbols);
        ctx.unpin_native_roots(this_pin);
        let off = match r {
            Ok(Some(v)) => v.as_int()?,
            _ => return None,
        };
        (off, 0)
    };

    let zone_after = obj_slot(ctx, calendar, sl.cal_zone);
    let local = millis.checked_add(offset_ms as i64)?;
    let fields = fields_from_local_millis(local);
    // The zone offset can pull a UTC instant just after the boundary back into
    // 1582 locally, so re-check on the LOCAL year. This also covers the BC era
    // (`YEAR = 1 - y` with `ERA = 0`), which is out of scope.
    if fields.year <= 1582 {
        return None;
    }
    Some(Inputs {
        millis,
        offset_ms,
        in_dst: dst_ms != 0,
        fields,
        calendar,
        symbols,
        vm_implemented,
        zone: zone_after,
    })
}

/// Reproduce `calendar.setTime(date)`'s observable effect without paying for
/// its eager `computeFields()`.
///
/// `Calendar.setTimeInMillis` stores the instant, marks the fields stale and
/// then recomputes them EAGERLY — that recompute is 20-40 us on this VM and is
/// the bulk of what this native exists to avoid. Leaving the fields marked
/// stale is observationally identical: `Calendar.get`/`complete()` recompute
/// on demand, and `getTimeInMillis()` reads `time` either way.
fn stamp_calendar(ctx: &mut dyn NativeContext, sl: &Slots, calendar: ObjectRef, millis: i64) {
    // Write only what actually changes. `set_field` re-derives the slot's field
    // descriptor to coerce the value, and that showed up at 15.4% of the native
    // in a perf profile — four writes per format, four different slots, so the
    // VM's one-entry descriptor cache missed every time. Three of the four
    // fields are already at the value we want after the first format (the flags
    // only move again when `Calendar.complete()` runs), so a read-and-skip
    // turns three writes into three much cheaper reads.
    if ctx.get_field(calendar, sl.cal_time) != Value::Long(millis) {
        ctx.set_field(calendar, sl.cal_time, Value::Long(millis));
    }
    if ctx.get_field(calendar, sl.cal_is_time_set).as_int() != Some(1) {
        ctx.set_field(calendar, sl.cal_is_time_set, Value::Int(1));
    }
    if ctx.get_field(calendar, sl.cal_are_fields_set).as_int() != Some(0) {
        ctx.set_field(calendar, sl.cal_are_fields_set, Value::Int(0));
    }
    if ctx.get_field(calendar, sl.cal_are_all_fields_set).as_int() != Some(0) {
        ctx.set_field(calendar, sl.cal_are_all_fields_set, Value::Int(0));
    }
}

/// A zone's resolved tzdb rules and its id text.
type ZoneRulesAnswer = (
    Option<std::sync::Arc<crate::tzdb::ZoneRulesData>>,
    std::sync::Arc<str>,
);

/// The zone's offset in ms at `millis`, memoising its tzdb rules per thread so
/// the Java `String` is decoded once per zone and hits do not clone `Arc`s.
fn zone_offset_and_dst_cached(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    zone: ObjectRef,
    millis: i64,
) -> Option<(i32, i32)> {
    let id_obj = obj_slot(ctx, zone, sl.tz_id)?;
    let vm = ctx.vm_identity();
    let id_addr = id_obj.as_ptr() as usize;
    let epoch = ctx.gc_address_epoch();
    let epoch_sec = millis.div_euclid(1000);
    let fast_hit = ZONE_RULES.with(|c| {
        let b = c.borrow();
        let s = b.as_ref()?;
        if s.vm == vm && s.id_addr == id_addr && s.epoch == epoch {
            if let Some(ref r) = s.rules {
                if epoch_sec >= crate::tzdb::ZONEINFO_LEGACY_FLOOR_EPOCH_SEC {
                    return Some(Some(crate::tzdb::legacy_offsets_ms_of(r, millis)));
                }
            }
            return Some(None);
        }
        None
    });
    match fast_hit {
        Some(Some(ans)) => return Some(ans),
        Some(None) => {
            let (_, id) = zone_rules_for_id(ctx, id_obj, crate::tzdb::get_zone_rules)?;
            return Some(crate::tzdb::legacy_offsets_ms(ctx, &id, millis));
        }
        None => {}
    }
    let (rules, id) = zone_rules_for_id(ctx, id_obj, crate::tzdb::get_zone_rules)?;
    Some(match rules {
        Some(r) if epoch_sec >= crate::tzdb::ZONEINFO_LEGACY_FLOOR_EPOCH_SEC => {
            crate::tzdb::legacy_offsets_ms_of(&r, millis)
        }
        _ => crate::tzdb::legacy_offsets_ms(ctx, &id, millis),
    })
}

fn pieces_z_requirements(pieces: &[Piece]) -> (bool, bool) {
    let mut need_short = false;
    let mut need_long = false;
    for p in pieces {
        if let Piece::Field { letter: 'z', count } = p {
            if *count >= 4 {
                need_long = true;
            } else {
                need_short = true;
            }
        }
    }
    (need_short, need_long)
}

fn z_slot_idx(in_dst: bool, long_style: bool) -> usize {
    (usize::from(in_dst) << 1) | usize::from(long_style)
}

#[derive(Clone)]
struct ZoneDisplayCacheEntry {
    vm: usize,
    zone_id: std::sync::Arc<str>,
    locale_addr: usize,
    epoch: u64,
    in_dst: bool,
    long_style: bool,
    name: String,
}

thread_local! {
    static ZONE_DISPLAY_CACHE: std::cell::RefCell<Vec<ZoneDisplayCacheEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn cached_zone_display_name(
    vm: usize,
    zone_id: &str,
    locale_addr: usize,
    epoch: u64,
    in_dst: bool,
    long_style: bool,
) -> Option<String> {
    ZONE_DISPLAY_CACHE.with(|c| {
        c.borrow()
            .iter()
            .find(|e| {
                e.vm == vm
                    && *e.zone_id == *zone_id
                    && e.locale_addr == locale_addr
                    && e.epoch == epoch
                    && e.in_dst == in_dst
                    && e.long_style == long_style
            })
            .map(|e| e.name.clone())
    })
}

fn insert_zone_display_name(entry: ZoneDisplayCacheEntry) {
    ZONE_DISPLAY_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= 32 {
            cache.clear();
        }
        if !cache.iter().any(|e| {
            e.vm == entry.vm
                && *e.zone_id == *entry.zone_id
                && e.locale_addr == entry.locale_addr
                && e.epoch == entry.epoch
                && e.in_dst == entry.in_dst
                && e.long_style == entry.long_style
        }) {
            cache.push(entry);
        }
    });
}

fn collect_cached_z_names(
    vm: usize,
    zone_id: &str,
    locale_addr: usize,
    epoch: u64,
) -> [Option<String>; 4] {
    let mut out: [Option<String>; 4] = [None, None, None, None];
    ZONE_DISPLAY_CACHE.with(|c| {
        for e in c.borrow().iter() {
            if e.vm == vm
                && *e.zone_id == *zone_id
                && e.locale_addr == locale_addr
                && e.epoch == epoch
            {
                out[z_slot_idx(e.in_dst, e.long_style)] = Some(e.name.clone());
            }
        }
    });
    out
}

/// Resolve zone display names (`'z'`) for the styles referenced by `pieces`.
/// Returns `Ok(Some((z_names, z_locale_addr, zone_id)))` when supported, or
/// `Ok(None)` to decline cleanly to bytecode (e.g. custom `TimeZone`, null
/// `locale`, or explicit `DateFormatSymbols.setZoneStrings`).
#[allow(clippy::too_many_arguments)]
fn resolve_z_names_for_call(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    pieces: &[Piece],
    this: &mut ObjectRef,
    date: &mut ObjectRef,
    inputs: &mut Inputs,
) -> Result<Option<([Option<String>; 4], usize, Option<std::sync::Arc<str>>)>, MethodCallFailed> {
    let (need_short, need_long) = pieces_z_requirements(pieces);
    if !need_short && !need_long {
        return Ok(Some(([None, None, None, None], 0, None)));
    }
    if !inputs.vm_implemented {
        return Ok(None);
    }
    let Some(zone_init) = inputs.zone else {
        return Ok(None);
    };
    let (Some(zs_slot), Some(loc_slot)) = (sl.dfs_is_zone_strings_set, sl.dfs_locale) else {
        return Ok(None);
    };
    if ctx.get_field(inputs.symbols, zs_slot).as_int() != Some(0) {
        return Ok(None);
    }
    let Some(loc_init) = obj_slot(ctx, inputs.symbols, loc_slot) else {
        return Ok(None);
    };
    let Some(id_obj) = obj_slot(ctx, zone_init, sl.tz_id) else {
        return Ok(None);
    };
    let Some((_, zone_id)) = zone_rules_for_id(ctx, id_obj, crate::tzdb::get_zone_rules) else {
        return Ok(None);
    };

    let vm = ctx.vm_identity();
    let mut zone = zone_init;
    let mut locale = loc_init;
    let mut z_names: [Option<String>; 4] = [None, None, None, None];
    let in_dst = inputs.in_dst;
    for long_style in [false, true] {
        let needed = if long_style { need_long } else { need_short };
        if !needed {
            continue;
        }
        let idx = z_slot_idx(in_dst, long_style);
        let epoch_now = ctx.gc_address_epoch();
        let loc_addr = locale.as_ptr() as usize;
        if let Some(hit) =
            cached_zone_display_name(vm, &zone_id, loc_addr, epoch_now, in_dst, long_style)
        {
            z_names[idx] = Some(hit);
            continue;
        }
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(*this);
        let date_h = scope.root(*date);
        let cal_h = scope.root(inputs.calendar);
        let syms_h = scope.root(inputs.symbols);
        let zone_h = scope.root(zone);
        let loc_h = scope.root(locale);
        let zone_now = scope.get(&zone_h);
        let loc_now = scope.get(&loc_h);
        let call_res = scope.invoke_virtual(
            zone_now,
            "getDisplayName",
            "(ZILjava/util/Locale;)Ljava/lang/String;",
            &[
                Value::Int(i32::from(in_dst)),
                Value::Int(i32::from(long_style)),
                Value::Object(Some(loc_now)),
            ],
        );
        *this = scope.get(&this_h);
        *date = scope.get(&date_h);
        inputs.calendar = scope.get(&cal_h);
        inputs.symbols = scope.get(&syms_h);
        zone = scope.get(&zone_h);
        inputs.zone = Some(zone);
        locale = scope.get(&loc_h);
        drop(scope);

        let name = match call_res? {
            Some(Value::Object(Some(s_obj))) => match ctx.read_string(s_obj) {
                Some(s) => s,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        let post_epoch = ctx.gc_address_epoch();
        let post_loc_addr = locale.as_ptr() as usize;
        insert_zone_display_name(ZoneDisplayCacheEntry {
            vm,
            zone_id: zone_id.clone(),
            locale_addr: post_loc_addr,
            epoch: post_epoch,
            in_dst,
            long_style,
            name: name.clone(),
        });
        z_names[idx] = Some(name);
    }
    let post_loc_addr = locale.as_ptr() as usize;
    Ok(Some((z_names, post_loc_addr, Some(zone_id))))
}

/// The zone's tzdb id and rules, memoised per thread so the Java `String`
/// is decoded once per zone rather than once per format.
#[allow(dead_code)]
fn zone_rules_cached(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    zone: ObjectRef,
) -> Option<ZoneRulesAnswer> {
    let id_obj = obj_slot(ctx, zone, sl.tz_id)?;
    zone_rules_for_id(ctx, id_obj, crate::tzdb::get_zone_rules)
}

/// [`zone_rules_cached`] once the zone's `ID` `String` is in hand; `resolve`
/// is the catalog lookup (`tzdb::get_zone_rules`; a test passes its own).
///
/// The rules are a function of the id TEXT alone, so the memo is keyed by
/// the `ID` String itself: its address within one collection epoch, which is
/// exact (nothing moves within an epoch). gc-common w34-a
/// (`common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`,
/// row 2): it used to be keyed by the zone's and the ID String's identity
/// HASHES, so after `setID(id)` a new id String whose hash collided with the
/// old one's (same class, so every 2^20 mints under the 8-byte header) was
/// served the OLD zone's rules and id. A miss decodes the id and, when the
/// text equals the memo's (the String moved, or an equal copy was
/// installed), keeps the rules it already resolved; only a different text
/// pays the catalog lookup.
fn zone_rules_for_id(
    ctx: &mut dyn NativeContext,
    id_obj: ObjectRef,
    resolve: fn(&mut dyn NativeContext, &str) -> Option<std::sync::Arc<crate::tzdb::ZoneRulesData>>,
) -> Option<ZoneRulesAnswer> {
    // gc-common w29-d: the VM too, so a thread that runs two VMs cannot reuse
    // one VM's zone (addresses and epochs are per VM).
    let vm = ctx.vm_identity();
    let id_addr = id_obj.as_ptr() as usize;
    let epoch = ctx.gc_address_epoch();
    let hit = ZONE_RULES.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|s| s.vm == vm && s.id_addr == id_addr && s.epoch == epoch)
            .map(|s| (s.rules.clone(), s.id.clone()))
    });
    if hit.is_some() {
        return hit;
    }
    let text = ctx.read_string(id_obj)?;
    let same_text = ZONE_RULES.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|s| s.vm == vm && *s.id == *text)
            .map(|s| (s.rules.clone(), s.id.clone()))
    });
    let (rules, id) = match same_text {
        Some(answer) => answer,
        None => {
            let id: std::sync::Arc<str> = std::sync::Arc::from(text.as_str());
            (resolve(ctx, &id), id)
        }
    };
    ZONE_RULES.with(|c| {
        *c.borrow_mut() = Some(ZoneRulesSlot {
            vm,
            id_addr,
            epoch,
            rules: rules.clone(),
            id: id.clone(),
        })
    });
    Some((rules, id))
}

/// The one-entry [`ZONE_RULES`] memo: which `ID` String (VM, address,
/// collection epoch) the rules were resolved for.
struct ZoneRulesSlot {
    vm: usize,
    id_addr: usize,
    epoch: u64,
    rules: Option<std::sync::Arc<crate::tzdb::ZoneRulesData>>,
    id: std::sync::Arc<str>,
}

thread_local! {
    /// Last `TimeZone` `ID` string -> resolved tzdb rules. A `TimeZone`'s `ID`
    /// can be reassigned (`setID`), so the memo names the ID String itself
    /// (address + collection epoch, gc-common w34-a); that check is three
    /// integer compares against a per-format string decode plus a catalog
    /// lookup.
    static ZONE_RULES: std::cell::RefCell<Option<ZoneRulesSlot>> =
        const { std::cell::RefCell::new(None) };
}

fn shape_key(inputs: &Inputs, has_z: bool) -> ShapeKey {
    (
        inputs.fields.month0 as u8,
        inputs.fields.day_of_week as u8,
        u8::from(inputs.fields.year <= 0),
        u8::from(inputs.fields.hour_of_day >= 12),
        inputs.offset_ms != 0,
        inputs.offset_ms < 0,
        has_z && inputs.in_dst,
    )
}


// ---------------------------------------------------------------------------
// Parsing helpers (`SimpleDateFormat.parse(String, ParsePosition)`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ParseSlots {
    pp_class: cratonvm_types::ClassId,
    pp_index: usize,
    cal_lenient: usize,
    df_number_format: usize,
    sdf_original_number_format: usize,
    sdf_has_following_minus_sign: usize,
    dec_fmt_class: cratonvm_types::ClassId,
    dec_fmt_grouping_used: usize,
    dec_fmt_multiplier: usize,
    dec_fmt_use_exp: usize,
    dec_fmt_symbols: usize,
    dec_fmt_syms_zero_digit: usize,
}

fn parse_slots(sl: &Slots) -> Option<ParseSlots> {
    Some(ParseSlots {
        pp_class: sl.pp_class?,
        pp_index: sl.pp_index?,
        cal_lenient: sl.cal_lenient?,
        df_number_format: sl.df_number_format?,
        sdf_original_number_format: sl.sdf_original_number_format?,
        sdf_has_following_minus_sign: sl.sdf_has_following_minus_sign?,
        dec_fmt_class: sl.dec_fmt_class?,
        dec_fmt_grouping_used: sl.dec_fmt_grouping_used?,
        dec_fmt_multiplier: sl.dec_fmt_multiplier?,
        dec_fmt_use_exp: sl.dec_fmt_use_exp?,
        dec_fmt_symbols: sl.dec_fmt_symbols?,
        dec_fmt_syms_zero_digit: sl.dec_fmt_syms_zero_digit?,
    })
}

/// Validates that `nf` is an un-exponentiated base-1x `DecimalFormat` with
/// ASCII `'0'` digits and grouping disabled, returning its `symbols` address.
fn standard_ascii_decimal_format_symbols_addr(
    ctx: &mut dyn NativeContext,
    ps: &ParseSlots,
    nf: ObjectRef,
) -> Option<usize> {
    if ctx.class_id_of_object(nf) != ps.dec_fmt_class {
        return None;
    }
    let mut vals = [Value::Object(None); 4];
    ctx.get_fields_typed(
        nf,
        &[
            (ps.dec_fmt_grouping_used, b'Z'),
            (ps.dec_fmt_multiplier, b'I'),
            (ps.dec_fmt_use_exp, b'Z'),
            (ps.dec_fmt_symbols, b'L'),
        ],
        &mut vals,
    );
    if vals[0].as_int() != Some(0)
        || vals[1].as_int() != Some(1)
        || vals[2].as_int() != Some(0)
    {
        return None;
    }
    let Value::Object(Some(dfs)) = vals[3] else {
        return None;
    };
    if ctx.get_field_typed(dfs, ps.dec_fmt_syms_zero_digit, b'C').as_int() != Some('0' as i32) {
        return None;
    }
    Some(dfs.as_ptr() as usize)
}

fn piece_is_numeric_following(piece: &Piece) -> bool {
    match piece {
        Piece::Literal(_) => false,
        Piece::Field { letter, count } => match letter {
            'M' | 'L' => *count <= 2,
            'y' | 'd' | 'k' | 'H' | 'm' | 's' | 'S' | 'D' | 'F' | 'w' | 'W' | 'h' | 'K'
            | 'Y' | 'u' => true,
            _ => false,
        },
    }
}

fn utf16_offset_to_byte(text: &str, start_utf16: i32) -> Option<usize> {
    if start_utf16 == 0 {
        return Some(0);
    }
    if start_utf16 < 0 {
        return None;
    }
    let target = start_utf16 as usize;
    let mut u16_pos = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if u16_pos == target {
            return Some(byte_idx);
        }
        u16_pos += ch.len_utf16();
    }
    if u16_pos == target {
        Some(text.len())
    } else {
        None
    }
}

fn strip_ci_prefix<'a>(haystack: &'a str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        let nb = needle.as_bytes();
        let hb = haystack.as_bytes();
        if hb.len() >= nb.len() && hb[..nb.len()].eq_ignore_ascii_case(nb) {
            return Some((nb.len(), nb.len()));
        }
        return None;
    }
    let mut hc = haystack.char_indices();
    let mut u16_len = 0usize;
    let mut matched_bytes = 0usize;
    for nc in needle.chars() {
        let (idx, hch) = hc.next()?;
        if !(hch == nc || hch.to_lowercase().eq(nc.to_lowercase())) {
            return None;
        }
        matched_bytes = idx + hch.len_utf8();
        u16_len += hch.len_utf16();
    }
    Some((matched_bytes, u16_len))
}

fn match_table_ci(
    haystack: &str,
    candidates: &[&[String]],
    start_idx: usize,
    end_excl: usize,
) -> Option<(usize, usize, usize)> {
    let mut best: Option<(usize, usize, usize)> = None;
    let mut best_utf16 = 0usize;
    for table in candidates {
        let limit = end_excl.min(table.len());
        for idx in start_idx..limit {
            if let Some((blen, u16len)) = strip_ci_prefix(haystack, &table[idx]) {
                if u16len > best_utf16 {
                    best_utf16 = u16len;
                    best = Some((idx, blen, u16len));
                }
            }
        }
    }
    best
}

fn parse_ascii_uint(s: &str, max_digits: Option<usize>) -> Option<(u32, usize)> {
    let b = s.as_bytes();
    let limit = max_digits.map_or(b.len(), |m| m.min(b.len())).min(9);
    let mut val: u32 = 0;
    let mut i = 0usize;
    while i < limit {
        let d = b[i];
        if !d.is_ascii_digit() {
            break;
        }
        val = val * 10 + u32::from(d - b'0');
        i += 1;
    }
    if i == 0 {
        None
    } else {
        Some((val, i))
    }
}

fn parse_two_digits(b: &[u8]) -> Option<u32> {
    if b.len() < 2 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() {
        return None;
    }
    Some(u32::from(b[0] - b'0') * 10 + u32::from(b[1] - b'0'))
}

fn parse_rfc822_offset(s: &str) -> Option<(i32, usize)> {
    let b = s.as_bytes();
    if b.len() < 5 {
        return None;
    }
    let sign = match b[0] {
        b'+' => 1i32,
        b'-' => -1i32,
        _ => return None,
    };
    let hh = parse_two_digits(&b[1..3])?;
    let mm = parse_two_digits(&b[3..5])?;
    if hh >= 24 || mm >= 60 {
        return None;
    }
    Some((sign * ((hh * 60 + mm) * 60_000) as i32, 5))
}

fn parse_iso8601_offset(s: &str, count: usize) -> Option<(i32, usize)> {
    let b = s.as_bytes();
    if b.first() == Some(&b'Z') {
        return Some((0, 1));
    }
    let (&sign_b, rest) = b.split_first()?;
    let sign = match sign_b {
        b'+' => 1i32,
        b'-' => -1i32,
        _ => return None,
    };
    match count {
        1 => {
            let hh = parse_two_digits(rest)?;
            if hh >= 24 {
                return None;
            }
            Some((sign * (hh * 3_600_000) as i32, 3))
        }
        2 => {
            if rest.len() < 4 {
                return None;
            }
            let hh = parse_two_digits(&rest[0..2])?;
            let mm = parse_two_digits(&rest[2..4])?;
            if hh >= 24 || mm >= 60 {
                return None;
            }
            Some((sign * ((hh * 60 + mm) * 60_000) as i32, 5))
        }
        3 => {
            if rest.len() < 5 || rest[2] != b':' {
                return None;
            }
            let hh = parse_two_digits(&rest[0..2])?;
            let mm = parse_two_digits(&rest[3..5])?;
            if hh >= 24 || mm >= 60 {
                return None;
            }
            Some((sign * ((hh * 60 + mm) * 60_000) as i32, 6))
        }
        _ => None,
    }
}

struct ParsedOutcome {
    millis: i64,
    end_utf16: i32,
    shape: ParseShapeKey,
}

#[allow(clippy::too_many_arguments)]
fn parse_with_pieces(
    text: &str,
    start_utf16: i32,
    pieces: &[Piece],
    syms: &Symbols,
    z_names: &[Option<String>; 4],
    zone_rules: Option<&crate::tzdb::ZoneRulesData>,
    lenient: bool,
) -> Option<ParsedOutcome> {
    if pieces.is_empty() {
        return None;
    }
    let mut bpos = utf16_offset_to_byte(text, start_utf16)?;
    let mut u16pos = start_utf16;

    let mut era_seen = false;
    let mut year: Option<i64> = None;
    let mut month1: Option<u32> = None;
    let mut day: Option<u32> = None;
    let mut hour_h24: Option<u32> = None;
    let mut hour_h12: Option<u32> = None;
    let mut is_pm: Option<bool> = None;
    let mut minute: Option<u32> = None;
    let mut second: Option<u32> = None;
    let mut milli: Option<u32> = None;
    let mut dow: Option<u32> = None;
    let mut saw_z = false;
    let mut parsed_z_dst: Option<bool> = None;
    let mut explicit_offset_ms: Option<i32> = None;

    for (idx, piece) in pieces.iter().enumerate() {
        let rest = &text[bpos..];
        match piece {
            Piece::Literal(lit) => {
                if !rest.starts_with(lit.as_str()) {
                    return None;
                }
                bpos += lit.len();
                u16pos = u16pos.checked_add(i32::try_from(lit.encode_utf16().count()).ok()?)?;
            }
            Piece::Field { letter, count } => {
                let obey = pieces.get(idx + 1).is_some_and(piece_is_numeric_following);
                let max_d = obey.then_some(*count);
                match letter {
                    'G' => {
                        if era_seen {
                            return None;
                        }
                        let (e_idx, blen, ulen) =
                            match_table_ci(rest, &[&syms.eras], 0, 2)?;
                        if e_idx != 1 {
                            return None;
                        }
                        era_seen = true;
                        bpos += blen;
                        u16pos += ulen as i32;
                    }
                    'y' => {
                        if *count == 2 || year.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        let y = i64::from(v);
                        if y <= 1582 {
                            return None;
                        }
                        year = Some(y);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'M' => {
                        if month1.is_some() {
                            return None;
                        }
                        if *count >= 3 {
                            let (m_idx, blen, ulen) = match_table_ci(
                                rest,
                                &[&syms.months, &syms.short_months],
                                0,
                                12,
                            )?;
                            month1 = Some(m_idx as u32 + 1);
                            bpos += blen;
                            u16pos += ulen as i32;
                        } else {
                            let (v, n) = parse_ascii_uint(rest, max_d)?;
                            if !(1..=12).contains(&v) {
                                return None;
                            }
                            month1 = Some(v);
                            bpos += n;
                            u16pos += n as i32;
                        }
                    }
                    'd' => {
                        if day.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if !(1..=31).contains(&v) {
                            return None;
                        }
                        day = Some(v);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'H' => {
                        if hour_h24.is_some() || hour_h12.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if v > 23 {
                            return None;
                        }
                        hour_h24 = Some(v);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'k' => {
                        if hour_h24.is_some() || hour_h12.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if !(1..=24).contains(&v) {
                            return None;
                        }
                        hour_h24 = Some(v % 24);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'h' => {
                        if hour_h24.is_some() || hour_h12.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if !(1..=12).contains(&v) {
                            return None;
                        }
                        hour_h12 = Some(v % 12);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'K' => {
                        if hour_h24.is_some() || hour_h12.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if v > 11 {
                            return None;
                        }
                        hour_h12 = Some(v);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'a' => {
                        if is_pm.is_some() {
                            return None;
                        }
                        let (a_idx, blen, ulen) =
                            match_table_ci(rest, &[&syms.ampms], 0, 2)?;
                        is_pm = Some(a_idx == 1);
                        bpos += blen;
                        u16pos += ulen as i32;
                    }
                    'm' => {
                        if minute.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if v > 59 {
                            return None;
                        }
                        minute = Some(v);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    's' => {
                        if second.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if v > 59 {
                            return None;
                        }
                        second = Some(v);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'S' => {
                        if milli.is_some() {
                            return None;
                        }
                        let (v, n) = parse_ascii_uint(rest, max_d)?;
                        if v > 999 {
                            return None;
                        }
                        milli = Some(v);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'E' => {
                        if dow.is_some() {
                            return None;
                        }
                        let (w_idx, blen, ulen) = match_table_ci(
                            rest,
                            &[&syms.weekdays, &syms.short_weekdays],
                            1,
                            8,
                        )?;
                        dow = Some(w_idx as u32);
                        bpos += blen;
                        u16pos += ulen as i32;
                    }
                    'z' => {
                        if saw_z || explicit_offset_ms.is_some() {
                            return None;
                        }
                        let mut best_slot: Option<(usize, usize, usize)> = None;
                        let mut best_u16 = 0usize;
                        let mut matched_std = false;
                        let mut matched_dst = false;
                        for (slot_idx, opt) in z_names.iter().enumerate() {
                            let Some(ref cand) = opt else { continue };
                            let Some((blen, ulen)) = strip_ci_prefix(rest, cand) else {
                                continue;
                            };
                            if ulen > best_u16 {
                                best_u16 = ulen;
                                best_slot = Some((slot_idx, blen, ulen));
                                matched_std = slot_idx < 2;
                                matched_dst = slot_idx >= 2;
                            } else if ulen == best_u16 && ulen > 0 {
                                if slot_idx < 2 {
                                    matched_std = true;
                                } else {
                                    matched_dst = true;
                                }
                            }
                        }
                        let (_, blen, ulen) = best_slot?;
                        let after = &rest[blen..];
                        // Decline custom offsets like `"GMT+05:00"` so bytecode parses the offset.
                        if after.starts_with('+') || after.starts_with('-') {
                            return None;
                        }
                        saw_z = true;
                        parsed_z_dst = match (matched_std, matched_dst) {
                            (true, false) => Some(false),
                            (false, true) => Some(true),
                            _ => None,
                        };
                        bpos += blen;
                        u16pos += ulen as i32;
                    }
                    'Z' => {
                        if saw_z || explicit_offset_ms.is_some() {
                            return None;
                        }
                        let (off, n) = parse_rfc822_offset(rest)?;
                        explicit_offset_ms = Some(off);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    'X' => {
                        if saw_z || explicit_offset_ms.is_some() {
                            return None;
                        }
                        let (off, n) = parse_iso8601_offset(rest, *count)?;
                        explicit_offset_ms = Some(off);
                        bpos += n;
                        u16pos += n as i32;
                    }
                    _ => return None,
                }
            }
        }
    }

    let hour = match (hour_h24, hour_h12, is_pm) {
        (Some(h), None, None) => h,
        (None, Some(h), pm) => h + if pm.unwrap_or(false) { 12 } else { 0 },
        (None, None, None) => 0,
        _ => return None,
    };
    let y = year.unwrap_or(1970);
    let m = month1.unwrap_or(1);
    let d = day.unwrap_or(1);
    if y <= 1582 || d > days_in_month(y, m) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    if let Some(parsed_dow) = dow {
        if year.is_none() || month1.is_none() || day.is_none() {
            return None;
        }
        let actual_dow = ((days + 4).rem_euclid(7) + 1) as u32;
        if parsed_dow != actual_dow {
            return None;
        }
    }
    let mi = minute.unwrap_or(0);
    let se = second.unwrap_or(0);
    let ms = milli.unwrap_or(0);
    let local_ms = days * MS_PER_DAY
        + i64::from(hour) * 3_600_000
        + i64::from(mi) * 60_000
        + i64::from(se) * 1_000
        + i64::from(ms);

    let millis = if let Some(off_ms) = explicit_offset_ms {
        let cand = local_ms.checked_sub(i64::from(off_ms))?;
        // HotSpot's CalendarBuilder leaves ZONE_OFFSET/DST_OFFSET and HOUR_OF_DAY
        // in the parsed offset without recomputing fields, whereas stamp_calendar
        // resets areFieldsSet=false so get() recomputes in the calendar's zone.
        // Decline when the parsed offset disagrees with the calendar's own zone
        // at `cand` so post-parse Calendar.get(...) stays HotSpot-identical.
        if let Some(rules) = zone_rules {
            let (zone_off_ms, _) = crate::tzdb::legacy_offsets_ms_of(rules, cand);
            if zone_off_ms != off_ms {
                return None;
            }
        }
        cand
    } else {
        let rules = zone_rules?;
        let est_off_s = crate::tzdb::offset_at_local(rules, local_ms.div_euclid(1000));
        let cand = local_ms.checked_sub(i64::from(est_off_s) * 1000)?;
        if cand < FIRST_FULLY_GREGORIAN_MILLIS {
            return None;
        }
        let prev_sec = (cand - MS_PER_DAY).div_euclid(1000);
        let next_sec = (cand + MS_PER_DAY).div_euclid(1000);
        if prev_sec < crate::tzdb::ZONEINFO_LEGACY_FLOOR_EPOCH_SEC
            || next_sec > crate::tzdb::ZONEINFO_UTC2100_EPOCH_SEC
        {
            return None;
        }
        let (o0, d0) = crate::tzdb::legacy_offsets_ms_of(rules, cand);
        let (om, dm) = crate::tzdb::legacy_offsets_ms_of(rules, cand - MS_PER_DAY);
        let (op, dp) = crate::tzdb::legacy_offsets_ms_of(rules, cand + MS_PER_DAY);
        if (o0, d0) != (om, dm)
            || (o0, d0) != (op, dp)
            || i64::from(o0) != i64::from(est_off_s) * 1000
        {
            return None;
        }
        if let Some(z_dst) = parsed_z_dst {
            if z_dst != (d0 != 0) {
                return None;
            }
        }
        cand
    };
    if millis < FIRST_FULLY_GREGORIAN_MILLIS {
        return None;
    }

    let tz_kind = if explicit_offset_ms.is_some() {
        3
    } else if saw_z {
        if parsed_z_dst == Some(true) {
            2
        } else {
            1
        }
    } else {
        0
    };
    Some(ParsedOutcome {
        millis,
        end_utf16: u16pos,
        shape: (m >= 7, hour >= 12, lenient, tz_kind),
    })
}

fn alloc_date_with_time(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    millis: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let date_obj = match ctx.try_alloc_object(sl.date_class, 0) {
        Some(obj) => obj,
        None => crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
            ctx.try_alloc_object(sl.date_class, 0)
        })
        .ok_or_else(|| crate::util_concurrent_ext::native_heap_oom("java/util/Date"))?,
    };
    ctx.set_field(date_obj, sl.date_fast_time, Value::Long(millis));
    Ok(date_obj)
}

fn parse_via_bytecode(
    ctx: &mut dyn NativeContext,
    sdf_class: cratonvm_types::ClassId,
    this: ObjectRef,
    text: Option<ObjectRef>,
    pos: Option<ObjectRef>,
) -> MethodCallResult {
    ctx.invoke_special_bytecode_only_by_class_id(
        sdf_class,
        "java/text/SimpleDateFormat",
        "parse",
        "(Ljava/lang/String;Ljava/text/ParsePosition;)Ljava/util/Date;",
        &[Value::Object(Some(this)), Value::Object(text), Value::Object(pos)],
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_z_names_for_parse(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    pieces: &[Piece],
    rules: Option<&crate::tzdb::ZoneRulesData>,
    this: &mut ObjectRef,
    text: &mut ObjectRef,
    pos: &mut ObjectRef,
    cal: &mut ObjectRef,
    syms: &mut ObjectRef,
    zone: &mut ObjectRef,
) -> Result<Option<([Option<String>; 4], usize, Option<std::sync::Arc<str>>)>, MethodCallFailed> {
    let (need_short, need_long) = pieces_z_requirements(pieces);
    if !need_short && !need_long {
        return Ok(Some(([None, None, None, None], 0, None)));
    }
    let Some(rules) = rules else {
        return Ok(None);
    };
    let (Some(zs_slot), Some(loc_slot)) = (sl.dfs_is_zone_strings_set, sl.dfs_locale) else {
        return Ok(None);
    };
    if ctx.get_field(*syms, zs_slot).as_int() != Some(0) {
        return Ok(None);
    }
    let Some(loc_init) = obj_slot(ctx, *syms, loc_slot) else {
        return Ok(None);
    };
    let Some(id_obj) = obj_slot(ctx, *zone, sl.tz_id) else {
        return Ok(None);
    };
    let Some((_, zone_id)) = zone_rules_for_id(ctx, id_obj, crate::tzdb::get_zone_rules) else {
        return Ok(None);
    };

    let vm = ctx.vm_identity();
    let mut locale = loc_init;
    let mut z_names: [Option<String>; 4] = [None, None, None, None];
    let dst_states: &[bool] = if crate::tzdb::uses_daylight_time(rules) {
        &[false, true]
    } else {
        &[false]
    };

    for &in_dst in dst_states {
        for long_style in [false, true] {
            let needed = if long_style { need_long } else { need_short };
            if !needed {
                continue;
            }
            let idx = z_slot_idx(in_dst, long_style);
            let epoch_now = ctx.gc_address_epoch();
            let loc_addr = locale.as_ptr() as usize;
            if let Some(hit) =
                cached_zone_display_name(vm, &zone_id, loc_addr, epoch_now, in_dst, long_style)
            {
                z_names[idx] = Some(hit);
                continue;
            }
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(*this);
            let text_h = scope.root(*text);
            let pos_h = scope.root(*pos);
            let cal_h = scope.root(*cal);
            let syms_h = scope.root(*syms);
            let zone_h = scope.root(*zone);
            let loc_h = scope.root(locale);
            let zone_now = scope.get(&zone_h);
            let loc_now = scope.get(&loc_h);
            let call_res = scope.invoke_virtual(
                zone_now,
                "getDisplayName",
                "(ZILjava/util/Locale;)Ljava/lang/String;",
                &[
                    Value::Int(i32::from(in_dst)),
                    Value::Int(i32::from(long_style)),
                    Value::Object(Some(loc_now)),
                ],
            );
            *this = scope.get(&this_h);
            *text = scope.get(&text_h);
            *pos = scope.get(&pos_h);
            *cal = scope.get(&cal_h);
            *syms = scope.get(&syms_h);
            *zone = scope.get(&zone_h);
            locale = scope.get(&loc_h);
            drop(scope);

            let name = match call_res? {
                Some(Value::Object(Some(s_obj))) => match ctx.read_string(s_obj) {
                    Some(s) => s,
                    None => return Ok(None),
                },
                _ => return Ok(None),
            };
            let post_epoch = ctx.gc_address_epoch();
            let post_loc_addr = locale.as_ptr() as usize;
            insert_zone_display_name(ZoneDisplayCacheEntry {
                vm,
                zone_id: zone_id.clone(),
                locale_addr: post_loc_addr,
                epoch: post_epoch,
                in_dst,
                long_style,
                name: name.clone(),
            });
            z_names[idx] = Some(name);
        }
    }
    let post_loc_addr = locale.as_ptr() as usize;
    Ok(Some((z_names, post_loc_addr, Some(zone_id))))
}

fn zone_is_vm_impl(sl: &Slots, ctx: &dyn NativeContext, zone: ObjectRef) -> (bool, bool) {
    let zc = ctx.class_id_of_object(zone);
    let is_zi = Some(zc) == sl.zoneinfo_class;
    let is_tz = Some(zc) == sl.timezone_class;
    (is_zi || is_tz, is_zi)
}

fn zone_raw_offset_diff_zero(
    ctx: &dyn NativeContext,
    sl: &Slots,
    zone: ObjectRef,
    is_zi: bool,
) -> bool {
    if !is_zi {
        return true;
    }
    match sl.zoneinfo_raw_offset_diff {
        Some(slot) => ctx.get_field_typed(zone, slot, b'I').as_int() == Some(0),
        None => false,
    }
}

fn try_parse_fast_tls(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    ps: &ParseSlots,
    vm: usize,
    epoch: u64,
    this: ObjectRef,
    text: ObjectRef,
    pos: ObjectRef,
) -> Option<ParsedOutcome> {
    LAST_PARSE_PLAN.with(|c| {
        let mut last = c.borrow_mut();
        let l = last.as_mut()?;
        if l.key.vm != vm
            || l.key.this_addr != this.as_ptr() as usize
            || l.key.stamp.epoch != epoch
        {
            return None;
        }
        let mut this_vals = [Value::Object(None); 4];
        ctx.get_fields_typed(
            this,
            &[
                (sl.sdf_pattern, b'L'),
                (sl.sdf_format_data, b'L'),
                (sl.df_calendar, b'L'),
                (ps.df_number_format, b'L'),
            ],
            &mut this_vals,
        );
        let Value::Object(Some(pat)) = this_vals[0] else {
            return None;
        };
        let Value::Object(Some(syms_obj)) = this_vals[1] else {
            return None;
        };
        let Value::Object(Some(cal)) = this_vals[2] else {
            return None;
        };
        let Value::Object(Some(nf)) = this_vals[3] else {
            return None;
        };
        if pat.as_ptr() as usize != l.key.stamp.pattern_addr
            || syms_obj.as_ptr() as usize != l.key.stamp.symbols_addr
            || cal.as_ptr() as usize != l.calendar_addr
            || nf.as_ptr() as usize != l.number_format_addr
        {
            return None;
        }
        let mut nf_vals = [Value::Object(None); 4];
        ctx.get_fields_typed(
            nf,
            &[
                (ps.dec_fmt_grouping_used, b'Z'),
                (ps.dec_fmt_multiplier, b'I'),
                (ps.dec_fmt_use_exp, b'Z'),
                (ps.dec_fmt_symbols, b'L'),
            ],
            &mut nf_vals,
        );
        if nf_vals[0].as_int() != Some(0)
            || nf_vals[1].as_int() != Some(1)
            || nf_vals[2].as_int() != Some(0)
        {
            return None;
        }
        let Value::Object(Some(dfs_nf)) = nf_vals[3] else {
            return None;
        };
        if dfs_nf.as_ptr() as usize != l.nf_symbols_addr {
            return None;
        }
        let mut cal_vals = [Value::Object(None); 7];
        ctx.get_fields_typed(
            cal,
            &[
                (sl.cal_zone, b'L'),
                (sl.gcal_cutover, b'J'),
                (ps.cal_lenient, b'Z'),
                (sl.cal_time, b'J'),
                (sl.cal_is_time_set, b'Z'),
                (sl.cal_are_fields_set, b'Z'),
                (sl.cal_are_all_fields_set, b'Z'),
            ],
            &mut cal_vals,
        );
        let Value::Object(Some(zone)) = cal_vals[0] else {
            return None;
        };
        if zone.as_ptr() as usize != l.zone_addr {
            return None;
        }
        match cal_vals[1] {
            Value::Long(v) if v == DEFAULT_CUTOVER_MILLIS => {}
            _ => return None,
        }
        let Value::Object(Some(tz_id)) = ctx.get_field_typed(zone, sl.tz_id, b'L') else {
            return None;
        };
        if tz_id.as_ptr() as usize != l.tz_id_addr {
            return None;
        }
        if !zone_raw_offset_diff_zero(ctx, sl, zone, l.zone_is_zi) {
            return None;
        }
        if l.has_z {
            let (Some(zs_slot), Some(loc_slot)) =
                (sl.dfs_is_zone_strings_set, sl.dfs_locale)
            else {
                return None;
            };
            if l.z_locale_addr == 0 {
                return None;
            }
            let mut dfs_vals = [Value::Object(None); 2];
            ctx.get_fields_typed(
                syms_obj,
                &[(zs_slot, b'Z'), (loc_slot, b'L')],
                &mut dfs_vals,
            );
            if dfs_vals[0].as_int() != Some(0) {
                return None;
            }
            let Value::Object(Some(loc_obj)) = dfs_vals[1] else {
                return None;
            };
            if loc_obj.as_ptr() as usize != l.z_locale_addr {
                return None;
            }
        }
        let lenient = cal_vals[2].as_int()? != 0;
        let start_utf16 = ctx.get_field_typed(pos, ps.pp_index, b'I').as_int()?;
        let text_str = ctx.read_string(text)?;
        let out = parse_with_pieces(
            &text_str,
            start_utf16,
            &l.pieces,
            &l.symbols,
            &l.z_names,
            l.zone_rules.as_deref(),
            lenient,
        )?;
        let shape_ok = if l.last_verified == Some(out.shape) {
            true
        } else if l.verified.contains(&out.shape) {
            l.last_verified = Some(out.shape);
            true
        } else {
            false
        };
        if !shape_ok {
            return None;
        }
        ctx.set_field(pos, ps.pp_index, Value::Int(out.end_utf16));
        if cal_vals[3] != Value::Long(out.millis) {
            ctx.set_field(cal, sl.cal_time, Value::Long(out.millis));
        }
        if cal_vals[4].as_int() != Some(1) {
            ctx.set_field(cal, sl.cal_is_time_set, Value::Int(1));
        }
        if cal_vals[5].as_int() != Some(0) {
            ctx.set_field(cal, sl.cal_are_fields_set, Value::Int(0));
        }
        if cal_vals[6].as_int() != Some(0) {
            ctx.set_field(cal, sl.cal_are_all_fields_set, Value::Int(0));
        }
        Some(out)
    })
}

struct ParsePrepared {
    sl: Slots,
    ps: ParseSlots,
    vm: usize,
    this: ObjectRef,
    text: ObjectRef,
    pos: ObjectRef,
    cal: ObjectRef,
    syms_obj: ObjectRef,
    pat_obj: ObjectRef,
    stamp: PlanStamp,
}

fn prepare_parse_slow(
    ctx: &mut dyn NativeContext,
    sl: Slots,
    ps: ParseSlots,
    vm: usize,
    epoch: u64,
    this: ObjectRef,
    text: ObjectRef,
    pos: ObjectRef,
) -> Result<ParsePrepared, MethodCallResult> {
    let zd = ctx.get_field(this, sl.sdf_zero_digit).as_int();
    if zd != Some('0' as i32) && zd != Some(0) {
        return Err(parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos)));
    }
    if ctx
        .get_field(this, sl.sdf_force_standalone)
        .as_int()
        .unwrap_or(1)
        != 0
    {
        return Err(parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos)));
    }
    if ctx
        .get_field(this, ps.sdf_has_following_minus_sign)
        .as_int()
        .unwrap_or(1)
        != 0
    {
        return Err(parse_via_bytecode(
            ctx,
            sl.sdf_class,
            this,
            Some(text),
            Some(pos),
        ));
    }
    let Some(nf) = obj_slot(ctx, this, ps.df_number_format) else {
        return Err(parse_via_bytecode(
            ctx,
            sl.sdf_class,
            this,
            Some(text),
            Some(pos),
        ));
    };
    let Some(orig_nf) = obj_slot(ctx, this, ps.sdf_original_number_format) else {
        return Err(parse_via_bytecode(
            ctx,
            sl.sdf_class,
            this,
            Some(text),
            Some(pos),
        ));
    };
    if !std::ptr::eq(nf.as_ptr(), orig_nf.as_ptr()) {
        return Err(parse_via_bytecode(
            ctx,
            sl.sdf_class,
            this,
            Some(text),
            Some(pos),
        ));
    }
    let Some(_nf_syms_addr) = standard_ascii_decimal_format_symbols_addr(ctx, &ps, nf) else {
        return Err(parse_via_bytecode(
            ctx,
            sl.sdf_class,
            this,
            Some(text),
            Some(pos),
        ));
    };
    let cal = match obj_slot(ctx, this, sl.df_calendar) {
        Some(c) if ctx.class_id_of_object(c) == sl.gcal_class => c,
        _ => return Err(parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos))),
    };
    if !matches!(ctx.get_field(cal, sl.gcal_cutover), Value::Long(v) if v == DEFAULT_CUTOVER_MILLIS)
    {
        return Err(parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos)));
    }
    let Some(syms_obj) = obj_slot(ctx, this, sl.sdf_format_data) else {
        return Err(parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos)));
    };
    let Some(pat_obj) = obj_slot(ctx, this, sl.sdf_pattern) else {
        return Err(parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos)));
    };
    let stamp = PlanStamp {
        pattern_addr: pat_obj.as_ptr() as usize,
        symbols_addr: syms_obj.as_ptr() as usize,
        epoch,
    };
    Ok(ParsePrepared {
        sl,
        ps,
        vm,
        this,
        text,
        pos,
        cal,
        syms_obj,
        pat_obj,
        stamp,
    })
}

fn finish_parse_slow(
    ctx: &mut dyn NativeContext,
    prep: ParsePrepared,
    this_id: PlanKey,
) -> MethodCallResult {
    let sl = prep.sl;
    let ps = prep.ps;
    let vm = prep.vm;
    let mut this = prep.this;
    let mut text = prep.text;
    let mut pos = prep.pos;
    let mut cal = prep.cal;
    let mut syms_obj = prep.syms_obj;
    let mut pat_obj = prep.pat_obj;
    let mut stamp = prep.stamp;

    if plan_cache()
        .lock()
        .get(&this_id)
        .is_some_and(|e| e.parse_poisoned)
    {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    }
    let current = plan_row_current(&mut plan_cache().lock(), this_id, stamp);
    let (pieces, pattern, syms) = match current {
        Some(parts) => parts,
        None => {
            let Some(pat_text) = ctx.read_string(pat_obj) else {
                return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
            };
            let decoded_syms = read_symbols(ctx, &sl, syms_obj);
            let mut cache = plan_cache().lock();
            plan_row_refresh(&mut cache, this_id, stamp, pat_text, decoded_syms);
            let parts = plan_row_current(&mut cache, this_id, stamp);
            drop(cache);
            match parts {
                Some(parts) => parts,
                None => return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos)),
            }
        }
    };

    let Some(mut zone) = obj_slot(ctx, cal, sl.cal_zone) else {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    };
    let (vm_impl, is_zi) = zone_is_vm_impl(&sl, ctx, zone);
    if !vm_impl || !zone_raw_offset_diff_zero(ctx, &sl, zone, is_zi) {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    }
    let Some(id_obj) = obj_slot(ctx, zone, sl.tz_id) else {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    };
    let Some(zone_rules_arc) =
        zone_rules_for_id(ctx, id_obj, crate::tzdb::get_zone_rules).and_then(|(r, _)| r)
    else {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    };
    let zone_rules = Some(zone_rules_arc);

    let (need_short, need_long) = pieces_z_requirements(&pieces);
    let has_z = need_short || need_long;
    let Some((z_names, z_locale_addr, z_id_opt)) = resolve_z_names_for_parse(
        ctx,
        &sl,
        &pieces,
        zone_rules.as_deref(),
        &mut this,
        &mut text,
        &mut pos,
        &mut cal,
        &mut syms_obj,
        &mut zone,
    )?
    else {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    };

    let epoch_after_z = ctx.gc_address_epoch();
    if epoch_after_z != stamp.epoch {
        let Some(po) = obj_slot(ctx, this, sl.sdf_pattern) else {
            return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
        };
        pat_obj = po;
        stamp = PlanStamp {
            pattern_addr: pat_obj.as_ptr() as usize,
            symbols_addr: syms_obj.as_ptr() as usize,
            epoch: epoch_after_z,
        };
    }

    let lenient = ctx
        .get_field_typed(cal, ps.cal_lenient, b'Z')
        .as_int()
        .unwrap_or(1)
        != 0;
    let Some(start_utf16) = ctx.get_field_typed(pos, ps.pp_index, b'I').as_int() else {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    };
    let Some(text_str) = ctx.read_string(text) else {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    };
    let Some(out) = parse_with_pieces(
        &text_str,
        start_utf16,
        &pieces,
        &syms,
        &z_names,
        zone_rules.as_deref(),
        lenient,
    ) else {
        return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
    };

    let needs_check =
        plan_row_note_parse_shape(&mut plan_cache().lock(), this_id, stamp, out.shape);
    if needs_check {
        let mut scope = NativeHandleScope::new(ctx);
        let pos_h = scope.root(pos);
        let this_now = this;
        let text_now = text;
        let pos_now = scope.get(&pos_h);
        let reference = parse_via_bytecode(
            &mut *scope,
            sl.sdf_class,
            this_now,
            Some(text_now),
            Some(pos_now),
        )?;
        let pos_after = scope.get(&pos_h);
        drop(scope);

        let ref_ok = match reference {
            Some(Value::Object(Some(ref_date))) => {
                let ref_cdate_none = obj_slot(ctx, ref_date, sl.date_cdate).is_none();
                let ref_time = ctx.get_field_typed(ref_date, sl.date_fast_time, b'J');
                let ref_idx = ctx.get_field_typed(pos_after, ps.pp_index, b'I');
                ref_cdate_none
                    && ref_time == Value::Long(out.millis)
                    && ref_idx == Value::Int(out.end_utf16)
            }
            _ => false,
        };
        if !ref_ok {
            {
                let mut cache = plan_cache().lock();
                if let Some(entry) = cache.get_mut(&this_id) {
                    entry.parse_poisoned = true;
                }
            }
            LAST_PARSE_PLAN.with(|c| *c.borrow_mut() = None);
            eprintln!(
                "[cratonvm] date-parse fast path DISABLED for pattern {pattern:?}: \
                 fast path produced ({}, idx={}) which disagreed with JDK; \
                 falling back to bytecode for this formatter",
                out.millis, out.end_utf16
            );
            return Ok(reference);
        }
        if dbg_on() {
            eprintln!(
                "[dbg-dateformat] verified parse pattern {pattern:?} shape {:?} -> ({}, idx={})",
                out.shape, out.millis, out.end_utf16
            );
        }
        return Ok(reference);
    }

    let verified = plan_row_parse_verified(&plan_cache().lock(), this_id, stamp);
    if let Some(verified) = verified {
        if let (Some(nf_now), Some(id_now)) = (
            obj_slot(ctx, this, ps.df_number_format),
            obj_slot(ctx, zone, sl.tz_id),
        ) {
          if let Some(nf_symbols_addr) = standard_ascii_decimal_format_symbols_addr(ctx, &ps, nf_now) {
            let all_z_names = if has_z {
                if let Some(ref zid) = z_id_opt {
                    collect_cached_z_names(vm, zid, z_locale_addr, stamp.epoch)
                } else {
                    z_names
                }
            } else {
                [None, None, None, None]
            };
            let (_, now_zi) = zone_is_vm_impl(&sl, ctx, zone);
            LAST_PARSE_PLAN.with(|c| {
                *c.borrow_mut() = Some(LastParsePlan {
                    key: LastPlanKey {
                        vm,
                        this_addr: this.as_ptr() as usize,
                        stamp,
                    },
                    calendar_addr: cal.as_ptr() as usize,
                    zone_addr: zone.as_ptr() as usize,
                    tz_id_addr: id_now.as_ptr() as usize,
                    zone_is_zi: now_zi,
                    zone_rules: zone_rules.clone(),
                    number_format_addr: nf_now.as_ptr() as usize,
                    nf_symbols_addr,
                    pieces: pieces.clone(),
                    symbols: syms.clone(),
                    has_z,
                    z_locale_addr,
                    z_names: all_z_names,
                    last_verified: Some(out.shape),
                    verified,
                });
            });
          }
        }
    }

    ctx.set_field(pos, ps.pp_index, Value::Int(out.end_utf16));
    stamp_calendar(ctx, &sl, cal, out.millis);
    let date_obj = alloc_date_with_time(ctx, &sl, out.millis)?;
    Ok(Some(Value::Object(Some(date_obj))))
}

fn try_format_fast_tls(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    vm: usize,
    epoch: u64,
    this: ObjectRef,
    date: ObjectRef,
) -> Option<ObjectRef> {
    if !is_sdf_format_eligible(ctx, sl, ctx.class_id_of_object(this))
        || ctx.class_id_of_object(date) != sl.date_class
    {
        return None;
    }
    LAST_PLAN.with(|c| {
        let mut last = c.borrow_mut();
        let l = last.as_mut()?;
        if l.key.vm != vm
            || l.key.this_addr != this.as_ptr() as usize
            || l.key.stamp.epoch != epoch
        {
            return None;
        }
        let rules = l.zone_rules.as_ref()?;
        let mut this_vals = [Value::Object(None); 3];
        ctx.get_fields_typed(
            this,
            &[
                (sl.sdf_pattern, b'L'),
                (sl.sdf_format_data, b'L'),
                (sl.df_calendar, b'L'),
            ],
            &mut this_vals,
        );
        let Value::Object(Some(pat)) = this_vals[0] else {
            return None;
        };
        if pat.as_ptr() as usize != l.key.stamp.pattern_addr {
            return None;
        }
        let Value::Object(Some(syms_obj)) = this_vals[1] else {
            return None;
        };
        if syms_obj.as_ptr() as usize != l.key.stamp.symbols_addr {
            return None;
        }
        let Value::Object(Some(cal)) = this_vals[2] else {
            return None;
        };
        if cal.as_ptr() as usize != l.calendar_addr {
            return None;
        }
        let mut cal_vals = [Value::Object(None); 6];
        ctx.get_fields_typed(
            cal,
            &[
                (sl.cal_zone, b'L'),
                (sl.gcal_cutover, b'J'),
                (sl.cal_time, b'J'),
                (sl.cal_is_time_set, b'Z'),
                (sl.cal_are_fields_set, b'Z'),
                (sl.cal_are_all_fields_set, b'Z'),
            ],
            &mut cal_vals,
        );
        let Value::Object(Some(zone)) = cal_vals[0] else {
            return None;
        };
        if zone.as_ptr() as usize != l.zone_addr {
            return None;
        }
        match cal_vals[1] {
            Value::Long(v) if v == DEFAULT_CUTOVER_MILLIS => {}
            _ => return None,
        }
        let Value::Object(Some(tz_id)) = ctx.get_field_typed(zone, sl.tz_id, b'L') else {
            return None;
        };
        if tz_id.as_ptr() as usize != l.tz_id_addr {
            return None;
        }
        let mut date_vals = [Value::Object(None); 2];
        ctx.get_fields_typed(
            date,
            &[(sl.date_cdate, b'L'), (sl.date_fast_time, b'J')],
            &mut date_vals,
        );
        if !matches!(date_vals[0], Value::Object(None)) {
            return None;
        }
        let millis = match date_vals[1] {
            Value::Long(v) if v >= FIRST_FULLY_GREGORIAN_MILLIS => v,
            _ => return None,
        };
        let epoch_sec = millis.div_euclid(1000);
        if epoch_sec < crate::tzdb::ZONEINFO_LEGACY_FLOOR_EPOCH_SEC {
            return None;
        }
        let (offset_ms, in_dst) = if l.has_z {
            let (Some(zs_slot), Some(loc_slot)) =
                (sl.dfs_is_zone_strings_set, sl.dfs_locale)
            else {
                return None;
            };
            if l.z_locale_addr == 0 {
                return None;
            }
            let mut dfs_vals = [Value::Object(None); 2];
            ctx.get_fields_typed(
                syms_obj,
                &[(zs_slot, b'Z'), (loc_slot, b'L')],
                &mut dfs_vals,
            );
            if dfs_vals[0].as_int() != Some(0) {
                return None;
            }
            let Value::Object(Some(loc_obj)) = dfs_vals[1] else {
                return None;
            };
            if loc_obj.as_ptr() as usize != l.z_locale_addr {
                return None;
            }
            let (off, dst) = crate::tzdb::legacy_offsets_ms_of(rules, millis);
            (off, dst != 0)
        } else {
            (
                crate::tzdb::offset_at_instant(rules, epoch_sec).saturating_mul(1000),
                false,
            )
        };
        let local = millis.saturating_add(i64::from(offset_ms));
        let fields = fields_from_local_millis(local);
        if fields.year <= 1582 {
            return None;
        }
        let inputs = Inputs {
            millis,
            offset_ms,
            in_dst,
            fields,
            calendar: cal,
            symbols: syms_obj,
            vm_implemented: true,
            zone: Some(zone),
        };
        let shape = shape_key(&inputs, l.has_z);
        let shape_ok = if l.last_verified == Some(shape) {
            true
        } else if l.verified.contains(&shape) {
            l.last_verified = Some(shape);
            true
        } else {
            false
        };
        if !shape_ok {
            return None;
        }
        SCRATCH.with(|buf| {
            let mut buf = buf.borrow_mut();
            if render(&l.pieces, &inputs, &l.symbols, &l.z_names, &mut buf).is_some() {
                if cal_vals[2] != Value::Long(millis) {
                    ctx.set_field(cal, sl.cal_time, Value::Long(millis));
                }
                if cal_vals[3].as_int() != Some(1) {
                    ctx.set_field(cal, sl.cal_is_time_set, Value::Int(1));
                }
                if cal_vals[4].as_int() != Some(0) {
                    ctx.set_field(cal, sl.cal_are_fields_set, Value::Int(0));
                }
                if cal_vals[5].as_int() != Some(0) {
                    ctx.set_field(cal, sl.cal_are_all_fields_set, Value::Int(0));
                }
                Some(ctx.create_string_uninterned_gc_safe(&buf))
            } else {
                None
            }
        })
    })
}

struct FormatPrepared {
    sl: Slots,
    vm: usize,
    this: ObjectRef,
    date: ObjectRef,
    inputs: Inputs,
    pattern_obj: ObjectRef,
    stamp: PlanStamp,
    last_key: LastPlanKey,
    shape_no_z: ShapeKey,
    shape_with_z: ShapeKey,
}

fn prepare_format_slow(
    ctx: &mut dyn NativeContext,
    sl: Slots,
    vm: usize,
    mut this: ObjectRef,
    mut date: ObjectRef,
) -> Result<FormatPrepared, MethodCallResult> {
    let Some(inputs) = gather(ctx, &sl, &mut this, &mut date) else {
        return Err(format_via_bytecode(ctx, this, Some(date)));
    };
    let Some(pattern_obj) = obj_slot(ctx, this, sl.sdf_pattern) else {
        return Err(format_via_bytecode(ctx, this, Some(date)));
    };
    let stamp = PlanStamp {
        pattern_addr: pattern_obj.as_ptr() as usize,
        symbols_addr: inputs.symbols.as_ptr() as usize,
        epoch: ctx.gc_address_epoch(),
    };
    let last_key = LastPlanKey {
        vm,
        this_addr: this.as_ptr() as usize,
        stamp,
    };
    let shape_no_z = shape_key(&inputs, false);
    let shape_with_z = shape_key(&inputs, true);

    let mut hit_and_rendered = false;
    let fast_res = LAST_PLAN.with(|c| {
        let mut last = c.borrow_mut();
        if let Some(l) = last.as_mut() {
            if l.key == last_key {
                let shape = if l.has_z { shape_with_z } else { shape_no_z };
                if l.has_z {
                    let (Some(zs_slot), Some(loc_slot)) =
                        (sl.dfs_is_zone_strings_set, sl.dfs_locale)
                    else {
                        return None;
                    };
                    if l.z_locale_addr == 0
                        || ctx.get_field(inputs.symbols, zs_slot).as_int() != Some(0)
                        || obj_slot(ctx, inputs.symbols, loc_slot).map(|o| o.as_ptr() as usize)
                            != Some(l.z_locale_addr)
                    {
                        return None;
                    }
                }
                let shape_ok = if l.last_verified == Some(shape) {
                    true
                } else if l.verified.contains(&shape) {
                    l.last_verified = Some(shape);
                    true
                } else {
                    false
                };
                if shape_ok {
                    return SCRATCH.with(|buf| {
                        let mut buf = buf.borrow_mut();
                        if render(&l.pieces, &inputs, &l.symbols, &l.z_names, &mut buf).is_some() {
                            hit_and_rendered = true;
                            stamp_calendar(ctx, &sl, inputs.calendar, inputs.millis);
                            Some(ctx.create_string_uninterned_gc_safe(&buf))
                        } else {
                            None
                        }
                    });
                }
            }
        }
        None
    });
    if hit_and_rendered {
        if let Some(text_obj) = fast_res {
            return Err(Ok(Some(Value::Object(Some(text_obj)))));
        }
        return Err(format_via_bytecode(ctx, this, Some(date)));
    }
    Ok(FormatPrepared {
        sl,
        vm,
        this,
        date,
        inputs,
        pattern_obj,
        stamp,
        last_key,
        shape_no_z,
        shape_with_z,
    })
}

fn finish_format_slow(
    ctx: &mut dyn NativeContext,
    prep: FormatPrepared,
    this_id: PlanKey,
) -> MethodCallResult {
    let sl = prep.sl;
    let vm = prep.vm;
    let mut this = prep.this;
    let mut date = prep.date;
    let mut inputs = prep.inputs;
    let mut pattern_obj = prep.pattern_obj;
    let mut stamp = prep.stamp;
    let mut last_key = prep.last_key;
    let shape_no_z = prep.shape_no_z;
    let shape_with_z = prep.shape_with_z;

    if plan_cache()
        .lock()
        .get(&this_id)
        .is_some_and(|e| e.poisoned)
    {
        return format_via_bytecode(ctx, this, Some(date));
    }
    let current = plan_row_current(&mut plan_cache().lock(), this_id, stamp);
    let (pieces, pattern, syms) = match current {
        Some(parts) => parts,
        None => {
            let Some(text) = ctx.read_string(pattern_obj) else {
                return format_via_bytecode(ctx, this, Some(date));
            };
            let syms = read_symbols(ctx, &sl, inputs.symbols);
            let mut cache = plan_cache().lock();
            plan_row_refresh(&mut cache, this_id, stamp, text, syms);
            let parts = plan_row_current(&mut cache, this_id, stamp);
            drop(cache);
            match parts {
                Some(parts) => parts,
                None => return format_via_bytecode(ctx, this, Some(date)),
            }
        }
    };

    let (need_z_short, need_z_long) = pieces_z_requirements(&pieces);
    let has_z = need_z_short || need_z_long;
    let shape = if has_z { shape_with_z } else { shape_no_z };
    let Some((z_names, z_locale_addr, z_id_opt)) =
        resolve_z_names_for_call(ctx, &sl, &pieces, &mut this, &mut date, &mut inputs)?
    else {
        return format_via_bytecode(ctx, this, Some(date));
    };
    if ctx.gc_address_epoch() != stamp.epoch {
        let Some(po) = obj_slot(ctx, this, sl.sdf_pattern) else {
            return format_via_bytecode(ctx, this, Some(date));
        };
        pattern_obj = po;
        stamp = PlanStamp {
            pattern_addr: pattern_obj.as_ptr() as usize,
            symbols_addr: inputs.symbols.as_ptr() as usize,
            epoch: ctx.gc_address_epoch(),
        };
        last_key = LastPlanKey {
            vm,
            this_addr: this.as_ptr() as usize,
            stamp,
        };
    }

    let mut fast = String::new();
    if render(&pieces, &inputs, &syms, &z_names, &mut fast).is_none() {
        return format_via_bytecode(ctx, this, Some(date));
    }

    let needs_check = plan_row_note_shape(&mut plan_cache().lock(), this_id, stamp, shape);
    if needs_check {
        let reference = format_via_bytecode(ctx, this, Some(date))?;
        let reference_str = match reference {
            Some(Value::Object(Some(s))) => ctx.read_string(s),
            _ => None,
        };
        if reference_str.as_deref() != Some(fast.as_str()) {
            {
                let mut cache = plan_cache().lock();
                if let Some(entry) = cache.get_mut(&this_id) {
                    entry.poisoned = true;
                }
            }
            LAST_PLAN.with(|c| *c.borrow_mut() = None);
            eprintln!(
                "[cratonvm] date-format fast path DISABLED for pattern {pattern:?}: \
                 produced {fast:?} but the JDK produced {reference_str:?}; \
                 falling back to bytecode for this formatter"
            );
            return Ok(reference);
        }
        if dbg_on() {
            eprintln!(
                "[dbg-dateformat] verified pattern {pattern:?} shape {shape:?} -> {fast:?}"
            );
        }
    } else {
        stamp_calendar(ctx, &sl, inputs.calendar, inputs.millis);
    }

    let verified = plan_row_verified(&plan_cache().lock(), this_id, stamp);
    if let Some(verified) = verified {
        let cal = inputs.calendar;
        let (zone_addr, tz_id_addr, zone_rules) = match obj_slot(ctx, cal, sl.cal_zone) {
            Some(zone) => {
                let zc = ctx.class_id_of_object(zone);
                let vm_impl = Some(zc) == sl.zoneinfo_class || Some(zc) == sl.timezone_class;
                if vm_impl {
                    if let Some(id_obj) = obj_slot(ctx, zone, sl.tz_id) {
                        let zr = ZONE_RULES.with(|c| {
                            let b = c.borrow();
                            b.as_ref()
                                .filter(|s| {
                                    s.vm == vm
                                        && s.id_addr == id_obj.as_ptr() as usize
                                        && s.epoch == stamp.epoch
                                })
                                .and_then(|s| s.rules.clone())
                        });
                        (zone.as_ptr() as usize, id_obj.as_ptr() as usize, zr)
                    } else {
                        (0, 0, None)
                    }
                } else {
                    (0, 0, None)
                }
            }
            None => (0, 0, None),
        };
        let all_z_names = if has_z {
            if let Some(ref zid) = z_id_opt {
                collect_cached_z_names(vm, zid, z_locale_addr, stamp.epoch)
            } else {
                z_names
            }
        } else {
            [None, None, None, None]
        };
        LAST_PLAN.with(|c| {
            *c.borrow_mut() = Some(LastPlan {
                key: last_key,
                calendar_addr: cal.as_ptr() as usize,
                zone_addr,
                tz_id_addr,
                zone_rules,
                pieces: pieces.clone(),
                symbols: syms.clone(),
                has_z,
                zone_id: z_id_opt,
                z_locale_addr,
                z_names: all_z_names,
                last_verified: Some(shape),
                verified,
            })
        });
    }
    let s = ctx.create_string_uninterned_gc_safe(&fast);
    Ok(Some(Value::Object(Some(s))))
}

pub fn register_date_format_fast(r: &mut NativeMethodRegistry) {
    let sdf_handler: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        enum SlowPrep {
            Format(FormatPrepared),
            Parse(ParsePrepared),
        }
        let this = obj_arg(args, 0)?;
        let prep = if args.len() >= 3 {
            let text_arg = match args.get(1) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            let pos_arg = match args.get(2) {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            let Some(sl) = slots(ctx) else {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "parse",
                    "(Ljava/lang/String;Ljava/text/ParsePosition;)Ljava/util/Date;",
                    &[Value::Object(text_arg), Value::Object(pos_arg)],
                );
            };
            let (Some(text), Some(pos), Some(ps)) = (text_arg, pos_arg, parse_slots(&sl)) else {
                return parse_via_bytecode(ctx, sl.sdf_class, this, text_arg, pos_arg);
            };
            if ctx.class_id_of_object(pos) != ps.pp_class
                || !is_sdf_format_eligible(ctx, &sl, ctx.class_id_of_object(this))
            {
                return parse_via_bytecode(ctx, sl.sdf_class, this, Some(text), Some(pos));
            }
            let vm = ctx.vm_identity();
            let epoch = ctx.gc_address_epoch();
            if let Some(out) = try_parse_fast_tls(ctx, &sl, &ps, vm, epoch, this, text, pos) {
                let date_obj = alloc_date_with_time(ctx, &sl, out.millis)?;
                return Ok(Some(Value::Object(Some(date_obj))));
            }
            match prepare_parse_slow(ctx, sl, ps, vm, epoch, this, text, pos) {
                Ok(p) => SlowPrep::Parse(p),
                Err(res) => return res,
            }
        } else {
            let date = match args.get(1) {
                Some(Value::Object(Some(d))) => *d,
                _ => return format_via_bytecode(ctx, this, None),
            };
            let Some(sl) = slots(ctx) else {
                return format_via_bytecode(ctx, this, Some(date));
            };
            let vm = ctx.vm_identity();
            let epoch = ctx.gc_address_epoch();
            if let Some(text_obj) = try_format_fast_tls(ctx, &sl, vm, epoch, this, date) {
                return Ok(Some(Value::Object(Some(text_obj))));
            }
            match prepare_format_slow(ctx, sl, vm, this, date) {
                Ok(f) => SlowPrep::Format(f),
                Err(res) => return res,
            }
        };

        let vm = ctx.vm_identity();
        let this_id: PlanKey = (vm, ctx.identity_hash_code(this));
        match prep {
            SlowPrep::Format(f) => finish_format_slow(ctx, f, this_id),
            SlowPrep::Parse(p) => finish_parse_slow(ctx, p, this_id),
        }
    };

    r.register(
        "java/text/DateFormat",
        "format",
        "(Ljava/util/Date;)Ljava/lang/String;",
        sdf_handler,
        cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/text/SimpleDateFormat",
        "parse",
        "(Ljava/lang/String;Ljava/text/ParsePosition;)Ljava/util/Date;",
        sdf_handler,
        cratonvm_native_api::NativeKind::Intrinsic,
    );

    crate::tzdb::register_zoneinfo_dst_natives(r);
}

#[cfg(test)]
mod tests;
