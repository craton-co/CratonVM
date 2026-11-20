// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Real IANA/TZDB-backed time-zone offset computation.
//!
//! Ported from `${java.home}/lib/tzdb.dat` — the same binary file and
//! algorithm `java.time.zone.ZoneRules`/`sun.util.calendar.ZoneInfoFile`
//! use internally (verified against OpenJDK 25's own source for both the
//! binary format and the `ZoneRules.getOffset(Instant)` /
//! `ZoneOffsetTransitionRule.createTransition(year)` algorithms). Cross-
//! checked bit-for-bit against real HotSpot JDK 25 across all 604 IANA
//! zones and a dozen instants spanning 1900-2100 (7248/7248 exact matches)
//! before being wired in here.
//!
//! Replaces the previous fixed-offset-per-zone table (`iana_zone_offset_seconds`
//! in `util_time.rs`, and the `tz_dst_rule`/`dst_start_year` hand-rolled
//! `SimpleTimeZone` approximations in `lib.rs`), which only modeled a
//! year-round standard offset (or, for ~20 special-cased zones, a single
//! *modern* recurring DST rule) — wrong for any zone/date where DST
//! actually applies, and for the ~580 zones never covered by that table
//! at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use cratonvm_native_api::registry::NativeMethodRegistry;
use cratonvm_native_api::{NativeCallback, NativeContext};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Binary cursor over tzdb.dat / per-zone rule byte blobs.
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn i8(&mut self) -> Option<i8> {
        self.u8().map(|b| b as i8)
    }
    fn u16(&mut self) -> Option<u16> {
        if self.pos + 2 > self.data.len() {
            return None;
        }
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Some(v)
    }
    fn i32(&mut self) -> Option<i32> {
        if self.pos + 4 > self.data.len() {
            return None;
        }
        let v = i32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().ok()?);
        self.pos += 4;
        Some(v)
    }
    fn i64(&mut self) -> Option<i64> {
        if self.pos + 8 > self.data.len() {
            return None;
        }
        let v = i64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().ok()?);
        self.pos += 8;
        Some(v)
    }
    fn utf(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        if self.pos + len > self.data.len() {
            return None;
        }
        let s = String::from_utf8_lossy(&self.data[self.pos..self.pos + len]).into_owned();
        self.pos += len;
        Some(s)
    }
    fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        if self.pos + n > self.data.len() {
            return None;
        }
        let v = self.data[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Some(v)
    }
}

/// Matches `sun.util.calendar.ZoneInfoFile.readEpochSec`.
fn read_epoch_sec(c: &mut Cursor) -> Option<i64> {
    let hi = c.u8()? as i32;
    if hi == 255 {
        c.i64()
    } else {
        let mid = c.u8()? as i32;
        let lo = c.u8()? as i32;
        let tot = (hi << 16) + (mid << 8) + lo;
        Some((tot as i64) * 900 - 4_575_744_000i64)
    }
}

/// Matches `sun.util.calendar.ZoneInfoFile.readOffset`.
fn read_offset(c: &mut Cursor) -> Option<i32> {
    let b = c.i8()? as i32;
    if b == 127 {
        c.i32()
    } else {
        Some(b * 900)
    }
}

// ---------------------------------------------------------------------------
// Per-zone rule data — mirrors `java.time.zone.ZoneRules`'s own fields.
// ---------------------------------------------------------------------------

/// A recurring transition rule for years beyond the last explicit
/// transition in the table (e.g. "DST starts second Sunday of March").
/// Field layout/semantics match `sun.util.calendar.ZoneInfoFile`'s private
/// `ZoneOffsetTransitionRule` (itself a raw-int mirror of the real
/// `java.time.zone.ZoneOffsetTransitionRule`).
#[derive(Clone, Copy, Debug)]
struct TransitionRule {
    month: u8,
    /// Day of month; negative counts back from the end of the month.
    dom: i8,
    /// ISO day-of-week 1(Monday)..7(Sunday), or -1 for "exact day of month".
    dow: i8,
    second_of_day: i32,
    /// 0 = UTC, 1 = WALL, 2 = STANDARD.
    time_definition: u8,
    standard_offset: i32,
    offset_before: i32,
    offset_after: i32,
}

fn parse_transition_rule(c: &mut Cursor) -> Option<TransitionRule> {
    let raw = c.i32()? as u32;
    let dow_byte = (raw >> 19) & 7;
    let time_byte = (raw >> 14) & 31;
    let std_byte = (raw >> 4) & 255;
    let before_byte = (raw >> 2) & 3;
    let after_byte = raw & 3;
    let month = (raw >> 28) as u8;
    let dom = (((raw >> 22) & 63) as i32 - 32) as i8;
    let dow = if dow_byte == 0 { -1i8 } else { dow_byte as i8 };
    let second_of_day = if time_byte == 31 {
        c.i32()?
    } else {
        (time_byte * 3600) as i32
    };
    let time_definition = ((raw >> 12) & 3) as u8;
    let standard_offset = if std_byte == 255 {
        c.i32()?
    } else {
        (std_byte as i32 - 128) * 900
    };
    let offset_before = if before_byte == 3 {
        c.i32()?
    } else {
        standard_offset + before_byte as i32 * 1800
    };
    let offset_after = if after_byte == 3 {
        c.i32()?
    } else {
        standard_offset + after_byte as i32 * 1800
    };
    Some(TransitionRule {
        month,
        dom,
        dow,
        second_of_day,
        time_definition,
        standard_offset,
        offset_before,
        offset_after,
    })
}

/// A zone's full rule set, mirroring `java.time.zone.ZoneRules`'s private
/// fields (`standardTransitions`/`standardOffsets`/
/// `savingsInstantTransitions`/`wallOffsets`/`lastRules`).
pub struct ZoneRulesData {
    standard_transitions: Vec<i64>,
    standard_offsets: Vec<i32>,
    savings_instant_transitions: Vec<i64>,
    wall_offsets: Vec<i32>,
    last_rules: Vec<TransitionRule>,
}

/// Mirrors `sun.util.calendar.ZoneInfoFile.getZoneInfo(DataInput, String)`
/// (the per-zone rule-byte format, shared with `java.time.zone.Ser`'s
/// `ZoneRules` serialization form).
fn parse_zone_rules(bytes: &[u8]) -> Option<ZoneRulesData> {
    let mut c = Cursor::new(bytes);
    let _type = c.u8()?;
    let std_size = c.i32()? as usize;
    let mut standard_transitions = Vec::with_capacity(std_size);
    for _ in 0..std_size {
        standard_transitions.push(read_epoch_sec(&mut c)?);
    }
    let mut standard_offsets = Vec::with_capacity(std_size + 1);
    for _ in 0..(std_size + 1) {
        standard_offsets.push(read_offset(&mut c)?);
    }
    let sav_size = c.i32()? as usize;
    let mut savings_instant_transitions = Vec::with_capacity(sav_size);
    for _ in 0..sav_size {
        savings_instant_transitions.push(read_epoch_sec(&mut c)?);
    }
    let mut wall_offsets = Vec::with_capacity(sav_size + 1);
    for _ in 0..(sav_size + 1) {
        wall_offsets.push(read_offset(&mut c)?);
    }
    let rule_size = c.i8()? as usize;
    let mut last_rules = Vec::with_capacity(rule_size);
    for _ in 0..rule_size {
        last_rules.push(parse_transition_rule(&mut c)?);
    }
    Some(ZoneRulesData {
        standard_transitions,
        standard_offsets,
        savings_instant_transitions,
        wall_offsets,
        last_rules,
    })
}

// ---------------------------------------------------------------------------
// Catalog — parses the tzdb.dat header (regions/aliases/rule blobs).
// ---------------------------------------------------------------------------

struct TzdbCatalog {
    rule_bytes: Vec<Vec<u8>>,
    region_to_rule_idx: HashMap<String, usize>,
    regions_ordered: Vec<String>,
    aliases: HashMap<String, String>,
}

/// `java.time.ZoneId.SHORT_IDS` — merged into the alias table by real
/// `ZoneInfoFile.load()` (`aliases.putAll(ZoneId.SHORT_IDS)`) and also
/// contributed to `TimeZone.getAvailableIDs()`/`ZoneId.getAvailableZoneIds()`.
const SHORT_IDS: &[(&str, &str)] = &[
    ("ACT", "Australia/Darwin"),
    ("AET", "Australia/Sydney"),
    ("AGT", "America/Argentina/Buenos_Aires"),
    ("ART", "Africa/Cairo"),
    ("AST", "America/Anchorage"),
    ("BET", "America/Sao_Paulo"),
    ("BST", "Asia/Dhaka"),
    ("CAT", "Africa/Harare"),
    ("CNT", "America/St_Johns"),
    ("CST", "America/Chicago"),
    ("CTT", "Asia/Shanghai"),
    ("EAT", "Africa/Addis_Ababa"),
    ("ECT", "Europe/Paris"),
    ("IET", "America/Indiana/Indianapolis"),
    ("IST", "Asia/Kolkata"),
    ("JST", "Asia/Tokyo"),
    ("MIT", "Pacific/Apia"),
    ("NET", "Asia/Yerevan"),
    ("NST", "Pacific/Auckland"),
    ("PLT", "Asia/Karachi"),
    ("PNT", "America/Phoenix"),
    ("PRT", "America/Puerto_Rico"),
    ("PST", "America/Los_Angeles"),
    ("SST", "Pacific/Guadalcanal"),
    ("VST", "Asia/Ho_Chi_Minh"),
    ("EST", "America/Panama"),
    ("MST", "America/Phoenix"),
    ("HST", "Pacific/Honolulu"),
];

/// Mirrors `sun.util.calendar.ZoneInfoFile.load(DataInputStream)`.
fn parse_catalog(data: &[u8]) -> Option<TzdbCatalog> {
    let mut c = Cursor::new(data);
    if c.u8()? != 1 {
        return None;
    }
    let group = c.utf()?;
    if group != "TZDB" {
        return None;
    }
    let version_count = c.u16()? as usize;
    for _ in 0..version_count {
        c.utf()?;
    }
    let region_count = c.u16()? as usize;
    let mut region_array = Vec::with_capacity(region_count);
    for _ in 0..region_count {
        region_array.push(c.utf()?);
    }
    let rule_count = c.u16()? as usize;
    let mut rule_bytes = Vec::with_capacity(rule_count);
    for _ in 0..rule_count {
        let len = c.u16()? as usize;
        rule_bytes.push(c.bytes(len)?);
    }
    let mut region_to_rule_idx = HashMap::new();
    let mut regions_ordered = Vec::new();
    for _ in 0..version_count {
        let rc = c.u16()? as usize;
        region_to_rule_idx.clear();
        regions_ordered = Vec::with_capacity(rc);
        for _ in 0..rc {
            let ridx = c.u16()? as usize;
            let name = region_array.get(ridx)?.clone();
            let rule_idx = c.u16()? as usize;
            region_to_rule_idx.insert(name.clone(), rule_idx);
            regions_ordered.push(name);
        }
    }
    let mut aliases = HashMap::new();
    for _ in 0..version_count {
        let alias_count = c.u16()? as usize;
        aliases.clear();
        for _ in 0..alias_count {
            let alias_idx = c.u16()? as usize;
            let region_idx = c.u16()? as usize;
            let alias = region_array.get(alias_idx)?.clone();
            let region = region_array.get(region_idx)?.clone();
            aliases.insert(alias, region);
        }
    }
    for (short, full) in SHORT_IDS.iter() {
        aliases.insert((*short).to_string(), (*full).to_string());
    }
    Some(TzdbCatalog {
        rule_bytes,
        region_to_rule_idx,
        regions_ordered,
        aliases,
    })
}

static CATALOG: OnceLock<Option<Arc<TzdbCatalog>>> = OnceLock::new();

fn catalog(ctx: &mut dyn NativeContext) -> Option<Arc<TzdbCatalog>> {
    CATALOG
        .get_or_init(|| {
            let java_home = ctx.get_system_property("java.home")?;
            let path = std::path::Path::new(&java_home)
                .join("lib")
                .join("tzdb.dat");
            let data = std::fs::read(path).ok()?;
            parse_catalog(&data).map(Arc::new)
        })
        .clone()
}

static RULES_CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<ZoneRulesData>>>>> = OnceLock::new();

fn rules_cache() -> &'static Mutex<HashMap<String, Option<Arc<ZoneRulesData>>>> {
    RULES_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parses a synthetic fixed-offset zone id — `"GMT+HH:MM"`, `"GMT-HH:MM"`,
/// `"UTC+HH:MM"`, or the un-normalised short forms `TimeZone.getTimeZone`
/// also accepts before canonicalising (`"GMT+2"`, `"GMT+0800"`, ...) — into
/// a whole-zone offset in seconds. Returns `None` for anything else (bare
/// `"GMT"`/`"UTC"` are real zero-offset named zones handled by the tzdb
/// catalog itself, and real IANA ids never start with `GMT`/`UTC` followed
/// by a sign).
///
/// TESTDATE8-1H (2026-07-22): `TimeZone.getTimeZone("GMT+01")` and friends
/// canonicalise to `"GMT+01:00"` fine (`normalize_gmt_custom_id` in
/// lib.rs), but every *offset query* against the resulting `ZoneInfo`
/// (`getOffset`, `getOffsets`, `getOffsetsByWall`, `getRawOffset`) is wired
/// through `get_zone_rules`/this module's wrapper functions, which only
/// know about zones present in the real `tzdb.dat` catalog — a synthetic
/// "GMT+01:00" id has no catalog entry, so every one of those queries
/// silently returned `None` -> the caller's `.unwrap_or(0)` -> a 0 offset
/// instead of the requested +1h. Confirmed against real HotSpot JDK 25:
/// `TimeZone.getTimeZone("GMT+01").getRawOffset()` is `3600000` there, `0`
/// here pre-fix, for every custom "GMT±HH:MM" id (not date-dependent, not
/// specific to `TestPreparedStatement.testDate8`'s 1582 date — that test
/// just happened to be the one that surfaced it, via
/// `TimeZone.setDefault(TimeZone.getTimeZone("GMT+01"))` feeding
/// `java.util.Date`'s deprecated field constructors).
fn parse_fixed_gmt_offset_seconds(zone_id: &str) -> Option<i32> {
    let body = zone_id
        .strip_prefix("GMT")
        .or_else(|| zone_id.strip_prefix("UTC"))?;
    let mut chars = body.chars();
    let sign = match chars.next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let rest = chars.as_str();
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.splitn(2, ':');
    let first = parts.next()?;
    let (h, m) = match parts.next() {
        Some(minute_part) => (first.parse::<i32>().ok()?, minute_part.parse::<i32>().ok()?),
        None if first.len() > 2 => {
            // Un-normalised "HHMM" form ("GMT+0800").
            let h: i32 = first[..first.len() - 2].parse().ok()?;
            let m: i32 = first[first.len() - 2..].parse().ok()?;
            (h, m)
        }
        None => (first.parse::<i32>().ok()?, 0),
    };
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

/// A degenerate, no-transitions rule set representing a fixed-offset zone
/// (never observes DST) — used as the [`get_zone_rules`] fallback for
/// synthetic "GMT±HH:MM" ids that the tzdb catalog itself has no entry
/// for. `offset_at_instant`/`offset_at_local`/`standard_offset_at_instant`/
/// `raw_offset` all treat empty transition vectors as "constant offset,
/// taken from the first/last element" — see their bodies below.
fn fixed_offset_rules(offset_seconds: i32) -> ZoneRulesData {
    ZoneRulesData {
        standard_transitions: Vec::new(),
        standard_offsets: vec![offset_seconds],
        savings_instant_transitions: Vec::new(),
        wall_offsets: vec![offset_seconds],
        last_rules: Vec::new(),
    }
}

/// Looks up (parsing + caching on first use) the full rule set for a zone
/// id, resolving tzdb aliases and the legacy `ZoneId.SHORT_IDS` first.
thread_local! {
    static TLS_ZONE_RULES_CACHE: std::cell::RefCell<Vec<(usize, String, Option<Arc<ZoneRulesData>>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub fn get_zone_rules(ctx: &mut dyn NativeContext, zone_id: &str) -> Option<Arc<ZoneRulesData>> {
    let vm = ctx.vm_identity();
    if let Some(hit) = TLS_ZONE_RULES_CACHE.with(|c| {
        c.borrow()
            .iter()
            .find(|(v, id, _)| *v == vm && id == zone_id)
            .map(|(_, _, r)| r.clone())
    }) {
        return hit;
    }
    let cached = rules_cache().lock().unwrap().get(zone_id).cloned();
    let result = if let Some(hit) = cached {
        hit
    } else {
        let cat = catalog(ctx);
        let mut computed = cat.and_then(|cat| {
            let resolved = cat
                .aliases
                .get(zone_id)
                .cloned()
                .unwrap_or_else(|| zone_id.to_string());
            cat.region_to_rule_idx
                .get(&resolved)
                .and_then(|&idx| cat.rule_bytes.get(idx))
                .and_then(|bytes| parse_zone_rules(bytes))
                .map(Arc::new)
        });
        if computed.is_none() {
            if let Some(offset_seconds) = parse_fixed_gmt_offset_seconds(zone_id) {
                computed = Some(Arc::new(fixed_offset_rules(offset_seconds)));
            }
        }
        rules_cache()
            .lock()
            .unwrap()
            .insert(zone_id.to_string(), computed.clone());
        computed
    };
    TLS_ZONE_RULES_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= 32 {
            cache.clear();
        }
        cache.push((vm, zone_id.to_string(), result.clone()));
    });
    result
}

/// Resolves a zone id (including tzdb aliases and the legacy 3-letter
/// `ZoneId.SHORT_IDS`, e.g. "CTT" -> "Asia/Shanghai") to the canonical
/// IANA region name `java.time.ZoneId.of(String)`'s single-arg overload
/// understands natively — i.e. without needing to pass `ZoneId.SHORT_IDS`
/// as a second argument. Returns `None` for an id this catalog doesn't
/// recognize at all.
pub fn canonical_zone_id(ctx: &mut dyn NativeContext, zone_id: &str) -> Option<String> {
    let cat = catalog(ctx)?;
    let resolved = cat
        .aliases
        .get(zone_id)
        .cloned()
        .unwrap_or_else(|| zone_id.to_string());
    if cat.region_to_rule_idx.contains_key(&resolved) {
        Some(resolved)
    } else {
        None
    }
}

/// Every zone id the JDK's own tzdb.dat knows about, plus the legacy
/// 3-letter `ZoneId.SHORT_IDS` — matches `TimeZone.getAvailableIDs()` /
/// `ZoneId.getAvailableZoneIds()`.
pub fn available_ids(ctx: &mut dyn NativeContext) -> Vec<String> {
    match catalog(ctx) {
        Some(cat) => {
            let mut ids = cat.regions_ordered.clone();
            ids.extend(SHORT_IDS.iter().map(|(s, _)| s.to_string()));
            ids
        }
        None => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Offset computation — mirrors `java.time.zone.ZoneRules`'s algorithms
// exactly (verified 7248/7248 against real HotSpot JDK 25 across all 604
// zones and a dozen instants spanning 1900-2100).
// ---------------------------------------------------------------------------

const DAYS_PER_CYCLE: i64 = 146_097;
const DAYS_0000_TO_1970: i64 = DAYS_PER_CYCLE * 5 - (30 * 365 + 7);

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0) && (year % 100 != 0 || year % 400 == 0)
}

fn length_of_month(year: i32, month: i32) -> i32 {
    match month {
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Proleptic-Gregorian epoch day, matching
/// `ZoneOffsetTransitionRule.toEpochDay` (which itself matches
/// `java.time.LocalDate.toEpochDay`).
fn to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
    let y = year as i64;
    let m = month as i64;
    let mut total = 365 * y;
    if y >= 0 {
        total += (y + 3) / 4 - (y + 99) / 100 + (y + 399) / 400;
    } else {
        total -= y / -4 - y / -100 + y / -400;
    }
    total += (367 * m - 362) / 12;
    total += day as i64 - 1;
    if m > 2 {
        total -= 1;
        if !is_leap_year(year) {
            total -= 1;
        }
    }
    total - DAYS_0000_TO_1970
}

/// ISO day-of-week for an epoch day, 1(Monday)..7(Sunday).
fn day_of_week_1_7(epoch_day: i64) -> i64 {
    (epoch_day + 3).rem_euclid(7) + 1
}

/// Matches `ZoneOffsetTransitionRule.adjust` (`relative`: 0 = next-or-same,
/// 1 = previous-or-same).
fn adjust_dow(epoch_day: i64, dow: i64, relative: i64) -> i64 {
    let cal_dow = day_of_week_1_7(epoch_day);
    if relative < 2 && cal_dow == dow {
        return epoch_day;
    }
    if relative & 1 == 0 {
        let diff = cal_dow - dow;
        epoch_day + if diff >= 0 { 7 - diff } else { -diff }
    } else {
        let diff = dow - cal_dow;
        epoch_day - if diff >= 0 { 7 - diff } else { -diff }
    }
}

/// The epoch second of this rule's transition in the given year — matches
/// `ZoneOffsetTransitionRule.getTransitionEpochSecond` (equivalently,
/// `ZoneOffsetTransitionRule.createTransition(year).toEpochSecond()` in the
/// full `java.time` API — both reduce to this same computation).
fn transition_epoch_second(rule: &TransitionRule, year: i32) -> i64 {
    let ed = if rule.dom < 0 {
        let d = to_epoch_day(
            year,
            rule.month as i32,
            length_of_month(year, rule.month as i32) + 1 + rule.dom as i32,
        );
        if rule.dow != -1 {
            adjust_dow(d, rule.dow as i64, 1)
        } else {
            d
        }
    } else {
        let d = to_epoch_day(year, rule.month as i32, rule.dom as i32);
        if rule.dow != -1 {
            adjust_dow(d, rule.dow as i64, 0)
        } else {
            d
        }
    };
    let difference = match rule.time_definition {
        0 => 0,                     // UTC
        1 => -rule.offset_before,   // WALL
        2 => -rule.standard_offset, // STANDARD
        _ => 0,
    };
    ed * 86400 + rule.second_of_day as i64 + difference as i64
}

/// Matches `ZoneRules.findYear` — the proleptic-Gregorian year containing
/// a given instant at a given offset.
fn find_year(epoch_second: i64, offset_seconds: i32) -> i32 {
    let local_second = epoch_second + offset_seconds as i64;
    let mut zero_day = local_second.div_euclid(86400) + DAYS_0000_TO_1970;
    zero_day -= 60;
    let mut adjust_applied = 0i64;
    if zero_day < 0 {
        let adjust_cycles = (zero_day + 1) / DAYS_PER_CYCLE - 1;
        adjust_applied = adjust_cycles * 400;
        zero_day += -adjust_cycles * DAYS_PER_CYCLE;
    }
    let mut year_est = (400 * zero_day + 591) / DAYS_PER_CYCLE;
    let mut doy_est = zero_day - (365 * year_est + year_est / 4 - year_est / 100 + year_est / 400);
    if doy_est < 0 {
        year_est -= 1;
        doy_est = zero_day - (365 * year_est + year_est / 4 - year_est / 100 + year_est / 400);
    }
    year_est += adjust_applied;
    let march_doy0 = doy_est as i32;
    let march_month0 = (march_doy0 * 5 + 2) / 153;
    year_est += (march_month0 / 10) as i64;
    year_est as i32
}

/// Matches `ZoneRules.getOffset(Instant)` exactly (binary search over the
/// historic transition table, falling back to the recurring `lastRules`
/// for instants beyond the table's end).
pub fn offset_at_instant(rules: &ZoneRulesData, epoch_sec: i64) -> i32 {
    if rules.savings_instant_transitions.is_empty() {
        return rules.wall_offsets.first().copied().unwrap_or(0);
    }
    let last_sav = *rules.savings_instant_transitions.last().unwrap();
    if !rules.last_rules.is_empty() && epoch_sec > last_sav {
        let year = find_year(epoch_sec, *rules.wall_offsets.last().unwrap());
        for rule in &rules.last_rules {
            let trans_epoch = transition_epoch_second(rule, year);
            if epoch_sec < trans_epoch {
                return rule.offset_before;
            }
        }
        return rules.last_rules.last().unwrap().offset_after;
    }
    match rules.savings_instant_transitions.binary_search(&epoch_sec) {
        Ok(i) => rules.wall_offsets[i + 1],
        Err(ip) => rules.wall_offsets[ip],
    }
}

/// Best-effort offset for a local (wall-clock) date-time, expressed as
/// "epoch seconds as if the wall-clock reading were UTC". Uses the
/// standard guess-and-refine resolution (compute assuming the local
/// reading is already an instant, then re-resolve using that offset) —
/// exact in the (overwhelmingly common) Normal case; in a DST Gap/Overlap
/// it lands on one of the two/zero valid offsets rather than always
/// matching `ZoneRules.getOffset(LocalDateTime)`'s specific "offset
/// before the transition" tie-break, since that requires the full
/// `LocalDateTime`-typed transition-array search this crate's callers
/// don't need for instant-based (epoch-second/millis) queries.
pub fn offset_at_local(rules: &ZoneRulesData, local_sec: i64) -> i32 {
    let guess = offset_at_instant(rules, local_sec);
    offset_at_instant(rules, local_sec - guess as i64)
}

/// The zone's standard (non-DST) offset for a specific instant — matches
/// `ZoneRules.getStandardOffset(Instant)`. Used to split a total offset
/// into raw/DST-savings components (`sun.util.calendar.ZoneInfo`'s
/// `offsets[0]`/`offsets[1]` out-params).
pub fn standard_offset_at_instant(rules: &ZoneRulesData, epoch_sec: i64) -> i32 {
    if rules.standard_transitions.is_empty() {
        return rules.standard_offsets[0];
    }
    match rules.standard_transitions.binary_search(&epoch_sec) {
        Ok(i) => rules.standard_offsets[i + 1],
        Err(ip) => rules.standard_offsets[ip],
    }
}

/// The zone's current standard (non-DST) offset — matches the `rawOffset`
/// legacy `java.util.TimeZone` reports (`getRawOffset()`).
pub fn raw_offset(rules: &ZoneRulesData) -> i32 {
    if !rules.standard_transitions.is_empty() {
        *rules.standard_offsets.last().unwrap()
    } else {
        rules.standard_offsets.first().copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Convenience wrappers taking a zone id string directly (what native
// callers actually have on hand).
// ---------------------------------------------------------------------------

pub fn offset_seconds_at_instant(
    ctx: &mut dyn NativeContext,
    zone_id: &str,
    epoch_sec: i64,
) -> Option<i32> {
    get_zone_rules(ctx, zone_id).map(|r| offset_at_instant(&r, epoch_sec))
}

pub fn offset_seconds_at_local(
    ctx: &mut dyn NativeContext,
    zone_id: &str,
    local_epoch_sec: i64,
) -> Option<i32> {
    get_zone_rules(ctx, zone_id).map(|r| offset_at_local(&r, local_epoch_sec))
}

/// `sun.util.calendar.ZoneInfoFile`'s conversion from tzdb rules to the legacy
/// `ZoneInfo` representation only models transitions from 1900 onward
/// (`UTC1900` in that class); a query before that floor falls through to the
/// zone's CURRENT standard offset with `dstSavings = 0`, not the deep
/// historical offset `java.time.zone.ZoneRules` would report. Mirrored so the
/// legacy `TimeZone`/`GregorianCalendar` path matches real HotSpot's legacy
/// behaviour bug-for-bug — see
/// `bug-h2-timezone-zonerules-offset-miscalculation-FIXED.md`.
pub const ZONEINFO_LEGACY_FLOOR_EPOCH_SEC: i64 = -2_208_988_800; // 1900-01-01T00:00:00Z

/// `(total_offset_ms, dst_offset_ms)` for `zone_id` at `date_millis`, under the
/// legacy `ZoneInfo` rules described on [`ZONEINFO_LEGACY_FLOOR_EPOCH_SEC`].
///
/// This is the body behind the registered `ZoneInfo`/`SimpleTimeZone`/
/// `TimeZone` `getOffset`/`getOffsets` natives. It lives here, rather than
/// inside those registrations, because `date_format_fast` needs the same
/// answer WITHOUT paying for a Java dispatch plus a native-funnel entry per
/// format — and two copies of this rule would be two things to keep in step.
pub fn legacy_offsets_ms(
    ctx: &mut dyn NativeContext,
    zone_id: &str,
    date_millis: i64,
) -> (i32, i32) {
    match get_zone_rules(ctx, zone_id) {
        Some(rules) => legacy_offsets_ms_of(&rules, date_millis),
        // An id this catalog cannot resolve keeps the answer it always had:
        // UTC, with no daylight saving. Unchanged from the pre-G28-1 spelling,
        // whose two `unwrap_or(0)`s reduced to exactly this.
        None => (0, 0),
    }
}

/// [`legacy_offsets_ms`] with the rules already in hand — the pure half, split
/// out by G28-1 so the whole legacy rule is unit-testable without a
/// `NativeContext`. Behaviour is unchanged.
pub fn legacy_offsets_ms_of(rules: &ZoneRulesData, date_millis: i64) -> (i32, i32) {
    let epoch_sec = date_millis.div_euclid(1000);
    let (total_sec, standard_sec) = if epoch_sec < ZONEINFO_LEGACY_FLOOR_EPOCH_SEC {
        let raw = raw_offset(rules);
        (raw, raw)
    } else {
        (
            offset_at_instant(rules, epoch_sec),
            standard_offset_at_instant(rules, epoch_sec),
        )
    };
    (
        total_sec.saturating_mul(1000),
        (total_sec - standard_sec).saturating_mul(1000),
    )
}

/// Is `local_sec` (WALL-clock epoch seconds) before `ZoneInfo`'s first
/// transition? That transition sits at UTC1900 but `getOffsetsByWall` searches
/// it in wall time, i.e. at UTC1900 plus the offset in force right after it
/// (Paris +0:09:21, Abidjan -0:16:08). Such a wall time is answered with the
/// zone's CURRENT standard offset and no saving, not the 1900 offset.
pub fn wall_before_first_legacy_transition(rules: &ZoneRulesData, local_sec: i64) -> bool {
    let after_floor = offset_at_instant(rules, ZONEINFO_LEGACY_FLOOR_EPOCH_SEC) as i64;
    local_sec < ZONEINFO_LEGACY_FLOOR_EPOCH_SEC + after_floor
}

/// `(total_offset_ms, dst_offset_ms)` for `ZoneInfo.getOffsetsByWall(wall, offsets)`,
/// given `wall_ms` already adjusted by `-rawOffsetDiff`.
///
/// Mirrors OpenJDK's `ZoneInfo.getOffsets(date, offsets, WALL_TIME)`:
/// - Before `UTC1900 + offset_at_instant(UTC1900)` -> `(raw_offset, 0)`
/// - Through `ZONEINFO_UTC2100_EPOCH_SEC` -> transition keys are searched in wall time
///   using `trans_utc + offset_after` (`ZoneInfo.getTransitionIndex`), so gaps select the
///   pre-transition offset and overlaps select the post-transition offset.
/// - Beyond the final 2100 transition -> mirrors `ZoneInfo.getLastRule()` (`SimpleTimeZone`).
pub fn legacy_wall_offsets_ms_of(rules: &ZoneRulesData, wall_adj_ms: i64) -> (i32, i32) {
    if rules.savings_instant_transitions.is_empty() {
        return (raw_offset(rules).saturating_mul(1000), 0);
    }
    let after_floor = offset_at_instant(rules, ZONEINFO_LEGACY_FLOOR_EPOCH_SEC);
    let first_wall_ms = (ZONEINFO_LEGACY_FLOOR_EPOCH_SEC + after_floor as i64) * 1000;
    if wall_adj_ms < first_wall_ms {
        return (raw_offset(rules).saturating_mul(1000), 0);
    }

    let wall_sec = wall_adj_ms.div_euclid(1000);
    let explicit_end = rules
        .savings_instant_transitions
        .partition_point(|&t| t <= ZONEINFO_UTC2100_EPOCH_SEC);
    let start_idx = rules
        .savings_instant_transitions[..explicit_end]
        .partition_point(|&t| t <= ZONEINFO_LEGACY_FLOOR_EPOCH_SEC);
    let explicit = &rules.savings_instant_transitions[start_idx..explicit_end];

    let mut lo = start_idx;
    let mut hi = explicit_end;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let t = rules.savings_instant_transitions[mid];
        let off_after = rules.wall_offsets[mid + 1] as i64;
        if (t + off_after) * 1000 <= wall_adj_ms {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let rel_pos = lo - start_idx;

    if rel_pos == 0 && (!explicit.is_empty() || rules.last_rules.is_empty()) {
        let total = after_floor;
        let standard = standard_offset_at_instant(rules, ZONEINFO_LEGACY_FLOOR_EPOCH_SEC);
        return (
            total.saturating_mul(1000),
            (total - standard).saturating_mul(1000),
        );
    }

    if rel_pos < explicit.len() {
        let idx = start_idx + rel_pos - 1;
        let total = rules.wall_offsets[idx + 1];
        let standard = standard_offset_at_instant(rules, rules.savings_instant_transitions[idx]);
        return (
            total.saturating_mul(1000),
            (total - standard).saturating_mul(1000),
        );
    }

    // Either past all explicit transitions (`rel_pos == explicit.len()`) with or without `last_rules`.
    let (fallback_total, fallback_trans) = if let Some(&last_t) = explicit.last() {
        (rules.wall_offsets[explicit_end], last_t)
    } else {
        (after_floor, ZONEINFO_LEGACY_FLOOR_EPOCH_SEC)
    };
    if rules.last_rules.is_empty() {
        let standard = standard_offset_at_instant(rules, fallback_trans);
        return (
            fallback_total.saturating_mul(1000),
            (fallback_total - standard).saturating_mul(1000),
        );
    }

    // Find the final `ZoneInfo.transitions[]` entry (from `last_rules` up to `UTC2100`).
    let mut last_2100: Option<(i64, i32)> = None;
    for y in [ZONEINFO_LASTYEAR, ZONEINFO_LASTYEAR - 1] {
        for rule in &rules.last_rules {
            let t = transition_epoch_second(rule, y);
            if t > fallback_trans && t <= ZONEINFO_UTC2100_EPOCH_SEC {
                if last_2100.is_none_or(|(best_t, _)| t > best_t) {
                    last_2100 = Some((t, rule.offset_after));
                }
            }
        }
        if last_2100.is_some() {
            break;
        }
    }
    if let Some((t2100, off2100)) = last_2100 {
        if wall_adj_ms >= (t2100 + off2100 as i64) * 1000 {
            // Post-2100 tail mirrors `ZoneInfo.getLastRule()` (`SimpleTimeZone`).
            let raw = raw_offset(rules);
            let std_sec = wall_sec - raw as i64;
            let tot = offset_at_instant(rules, std_sec);
            let mut dst = tot - raw;
            if dst > 0 && offset_at_instant(rules, std_sec - dst as i64) == raw {
                dst = 0;
            }
            return (
                (raw + dst).saturating_mul(1000),
                dst.saturating_mul(1000),
            );
        }
    }

    let approx_year = find_year(wall_sec, *rules.wall_offsets.last().unwrap_or(&fallback_total));
    let mut chosen: Option<(i64, i32, i32)> = None;
    for y in [approx_year - 1, approx_year, approx_year + 1] {
        for rule in &rules.last_rules {
            let t = transition_epoch_second(rule, y);
            if t <= fallback_trans || t > ZONEINFO_UTC2100_EPOCH_SEC {
                continue;
            }
            let wk_ms = (t + rule.offset_after as i64) * 1000;
            if wk_ms <= wall_adj_ms && chosen.is_none_or(|(best_t, _, _)| t > best_t) {
                chosen = Some((t, rule.offset_after, rule.standard_offset));
            }
        }
    }
    if let Some((_t, total, standard)) = chosen {
        return (
            total.saturating_mul(1000),
            (total - standard).saturating_mul(1000),
        );
    }
    let standard = standard_offset_at_instant(rules, fallback_trans);
    (
        fallback_total.saturating_mul(1000),
        (fallback_total - standard).saturating_mul(1000),
    )
}

pub fn raw_offset_seconds(ctx: &mut dyn NativeContext, zone_id: &str) -> Option<i32> {
    get_zone_rules(ctx, zone_id).map(|r| raw_offset(&r))
}

pub fn standard_offset_seconds_at_instant(
    ctx: &mut dyn NativeContext,
    zone_id: &str,
    epoch_sec: i64,
) -> Option<i32> {
    get_zone_rules(ctx, zone_id).map(|r| standard_offset_at_instant(&r, epoch_sec))
}

// ---------------------------------------------------------------------------
// G28-1 — the DAYLIGHT-SAVING rule layer, and the five natives that publish it.
// ---------------------------------------------------------------------------
//
// Everything above answers "what is the offset". Everything below answers the
// other five questions `java.util.TimeZone` asks about a zone —
// `getDSTSavings()`, `useDaylightTime()`, `observesDaylightTime()`,
// `inDaylightTime(Date)` and the six-argument `getOffset(era, y, m, d, dow,
// ms)` — which were answered by real `sun.util.calendar.ZoneInfo` bytecode
// against a FABRICATED receiver whose `transitions` array is null and whose
// `simpleTimeZoneParams` is null. That receiver says "no zone on earth has ever
// observed daylight saving": MEASURED wrong on 542 of the 632 ids
// `TimeZone.getAvailableIDs()` returns. See
// `docs/feature-designs/concurrent-gc-maturation.md`
// (the measurement and the design) and `G28-1-the-dst-rule-layer-rebuilt-20260817.md`
// (this implementation).
//
// The rules below are not invented. Each is a transcription of the arithmetic
// real `sun.util.calendar.ZoneInfoFile.getZoneInfo(...)` performs on THESE SAME
// five arrays when it builds a `ZoneInfo`, plus the accessor `ZoneInfo` then
// runs against the result — read out of `$JAVA_HOME/lib/src.zip` and validated,
// as a rule, on 632 zones x 15 accessors = 9,480 oracle rows with 0 mismatches
// (`scratchpad/g28/G28Model.java`, which computes every quantity here from
// `java.time.zone.ZoneRules` alone and compares against `java.util.TimeZone`).

/// `sun.util.calendar.ZoneInfoFile.UTC2100` — the last instant its generated
/// transition table can describe. A rule beyond it is not written into the
/// table at all, which is why the scan in [`observes_daylight_time_of`] stops
/// there and why `last_savings_transition_year` pins to `LASTYEAR`.
pub const ZONEINFO_UTC2100_EPOCH_SEC: i64 = 4_133_980_799;

/// `sun.util.calendar.ZoneInfoFile.LASTYEAR`.
pub const ZONEINFO_LASTYEAR: i32 = 2100;

/// `ZoneInfoFile`'s `lastyear`: the proleptic-Gregorian year of the FINAL
/// savings transition, read at the offset in force BEFORE it (`wallOffsets[n-1]`
/// — that index is `ZoneInfoFile`'s, and it is the before-offset, not the
/// after-offset); or `LASTYEAR` outright as soon as any transition lies beyond
/// `UTC2100`, because that is where `ZoneInfoFile`'s own loop breaks out.
///
/// Returns `None` for a zone with no savings transitions at all, where
/// `ZoneInfoFile` never enters the block that computes it.
fn last_savings_transition_year(rules: &ZoneRulesData) -> Option<i32> {
    let n = rules.savings_instant_transitions.len();
    if n == 0 {
        return None;
    }
    if rules
        .savings_instant_transitions
        .iter()
        .any(|&t| t > ZONEINFO_UTC2100_EPOCH_SEC)
    {
        return Some(ZONEINFO_LASTYEAR);
    }
    Some(find_year(
        rules.savings_instant_transitions[n - 1],
        rules.wall_offsets[n - 1],
    ))
}

/// The RECURRING daylight saving, in seconds — the quantity real
/// `ZoneInfoFile` stores in `ZoneInfo.dstSavings` and `getDSTSavings()`
/// returns.
///
/// It is a property of the zone's LAST RULES, never of its transition history:
/// `America/Sao_Paulo` has a dense DST past and answers `0`, because Brazil
/// abolished daylight saving in 2019. The three branches are `ZoneInfoFile`'s
/// three, in its order:
///
///   * no savings transitions at all -> `ZoneInfo.transitions` is null, nothing
///     is computed, `0`;
///   * two or more `lastRules` -> the saving of the START rule, where "start"
///     is `lastRules[n-2]` and `lastRules[n-1]` SWAPPED when the first of the
///     pair steps the clock back and the second steps it forward. That swap is
///     what makes the southern hemisphere answer `+1800000` for
///     `Australia/Lord_Howe` rather than `-1800000`;
///   * otherwise `ZoneInfoFile`'s "Israel/Iran workaround": a zone with no
///     recurring rule but an explicit table running all the way to `LASTYEAR`
///     gets a saving synthesised from its final pair of transitions.
///
/// **The third branch is UNEXERCISED by the tzdb this JDK ships** — measured:
/// deleting it from `G28Model` leaves the 9,480-row comparison at 0 mismatches,
/// because every zone whose table reaches 2100 also has `lastRules`. It is
/// transcribed anyway because it is `ZoneInfoFile`'s own arithmetic and a later
/// tzdb can reach it; it is NOT a guess, and the record says plainly that it is
/// unmeasured.
pub fn dst_savings_seconds(rules: &ZoneRulesData) -> i32 {
    if rules.savings_instant_transitions.is_empty() {
        return 0;
    }
    let n = rules.last_rules.len();
    if n > 1 {
        let first = &rules.last_rules[n - 2];
        let second = &rules.last_rules[n - 1];
        let start = if first.offset_after - first.offset_before < 0
            && second.offset_after - second.offset_before > 0
        {
            second
        } else {
            first
        };
        return start.offset_after - start.offset_before;
    }
    if rules.savings_instant_transitions.len() > 2
        && last_savings_transition_year(rules).unwrap_or(i32::MIN) >= ZONEINFO_LASTYEAR
    {
        let m = rules.savings_instant_transitions.len();
        let start_trans = rules.savings_instant_transitions[m - 2];
        let start_offset = rules.wall_offsets[m - 1];
        let start_standard = standard_offset_at_instant(rules, start_trans);
        let end_trans = rules.savings_instant_transitions[m - 1];
        let end_offset = rules.wall_offsets[m];
        let end_standard = standard_offset_at_instant(rules, end_trans);
        if start_offset > start_standard && end_offset == end_standard {
            return start_offset - start_standard;
        }
    }
    0
}

/// `ZoneInfo.useDaylightTime()`, whose whole body is
/// `return (simpleTimeZoneParams != null);`.
///
/// `simpleTimeZoneParams` and `dstSavings` are written by the SAME two branches
/// of `ZoneInfoFile` — neither is ever set without the other — so
/// "`dstSavings != 0`" is that field test, expressed in the only state this
/// module keeps. Validated on all 632 ids.
pub fn uses_daylight_time(rules: &ZoneRulesData) -> bool {
    dst_savings_seconds(rules) != 0
}

/// `ZoneInfo.inDaylightTime(Date)`: is the offset in force at this instant
/// something other than the zone's STANDARD offset?
///
/// `ZoneInfo` reads a DST bit that `ZoneInfoFile.addTrans` sets to exactly
/// `offset - standardOffset` per table entry, and answers `false` outright for
/// any instant before the table starts — which is the 1900 floor
/// [`ZONEINFO_LEGACY_FLOOR_EPOCH_SEC`] already models. So the whole method is
/// the DST component of [`legacy_offsets_ms_of`], which is not a coincidence
/// but the same rule reached twice.
pub fn in_daylight_time_of(rules: &ZoneRulesData, date_millis: i64) -> bool {
    legacy_offsets_ms_of(rules, date_millis).1 != 0
}

/// `ZoneInfo.observesDaylightTime()` — the one member of this family whose
/// answer legitimately depends on WHEN YOU ASK.
///
/// `ZoneInfo`'s body is: `simpleTimeZoneParams != null` short-circuits to
/// `true`; otherwise walk the transition table from the entry containing NOW to
/// its end and answer `true` if any of them carries daylight saving. The walk
/// starts AT the current entry, so "in daylight time right now" counts, and the
/// table stops at `UTC2100`.
///
/// That distinction is not academic: `Africa/Casablanca`, `Africa/El_Aaiun` and
/// `Africa/Windhoek` model a standing daylight offset with no recurring rule
/// and are the three ids for which `useDaylightTime()` is `false` while
/// `observesDaylightTime()` is `true`. A rule keyed only on `lastRules` gets
/// them wrong.
pub fn observes_daylight_time_of(rules: &ZoneRulesData, now_millis: i64) -> bool {
    if uses_daylight_time(rules) {
        return true;
    }
    if in_daylight_time_of(rules, now_millis) {
        return true;
    }
    let now_sec = now_millis.div_euclid(1000);
    for (i, &trans) in rules.savings_instant_transitions.iter().enumerate() {
        if trans <= now_sec || trans > ZONEINFO_UTC2100_EPOCH_SEC {
            continue;
        }
        if rules.wall_offsets[i + 1] != standard_offset_at_instant(rules, trans) {
            return true;
        }
    }
    false
}

/// The six-argument `ZoneInfo.getOffset(era, year, month, day, dayOfWeek,
/// milliseconds)`, whose `milliseconds` argument is **standard local time** and
/// whose return is the TOTAL offset.
///
/// `ZoneInfo` converts by subtracting the zone's fixed `rawOffset` — its
/// modern standard offset, not the standard offset in force at the resulting
/// instant — and then reads the total offset off that instant. Subtracting the
/// modern raw offset is wrong for a pre-modern date whose standard offset
/// differed, and it is reproduced here deliberately: the 1900 floor swallows
/// almost every such case, and where it does not, HotSpot is the oracle.
///
/// The `dayOfWeek` argument is range-checked and then IGNORED, exactly as the
/// real method ignores it.
pub fn offset_ms_at_local_standard_of(rules: &ZoneRulesData, local_standard_ms: i64) -> i32 {
    let raw_ms = (raw_offset(rules) as i64).saturating_mul(1000);
    legacy_offsets_ms_of(rules, local_standard_ms.saturating_sub(raw_ms)).0
}

/// `sun.util.calendar.Gregorian.validate` for the three fields the six-arg
/// `getOffset` actually sets — the time-of-day fields are left at zero by
/// `newCalendarDate(null)` and cannot fail. `month` is 1-based here.
fn gregorian_date_is_valid(year: i32, month: i32, day: i32) -> bool {
    (1..=12).contains(&month) && day >= 1 && day <= length_of_month(year, month)
}

/// The RECURRING daylight saving in milliseconds for a zone id — the value
/// `ZoneInfo.dstSavings` is supposed to hold.
///
/// Exists so `alloc_synth_timezone` can seed that field from the same producer
/// the accessors read. It could not before: the value lives in
/// `ZoneRulesData.last_rules`, which is private to this module, and a sampler
/// built outside it was MEASURED wrong on `Africa/Casablanca`,
/// `Africa/El_Aaiun` and `Africa/Windhoek` (601/604). Exposing it from in here
/// is the fix for that; see N-TZ-1 in
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub fn dst_savings_ms(ctx: &mut dyn NativeContext, zone_id: &str) -> Option<i32> {
    get_zone_rules(ctx, zone_id).map(|r| dst_savings_seconds(&r).saturating_mul(1000))
}

#[derive(Clone, Copy)]
struct ZoneInfoFieldSlots {
    vm: usize,
    cid: cratonvm_types::ClassId,
    id: Option<usize>,
    raw_offset: Option<usize>,
    raw_offset_diff: Option<usize>,
}

fn zone_info_field_slots(ctx: &mut dyn NativeContext, this: ObjectRef) -> ZoneInfoFieldSlots {
    thread_local! {
        static LAST_SLOTS: std::cell::Cell<Option<ZoneInfoFieldSlots>> =
            const { std::cell::Cell::new(None) };
        static SLOTS: std::cell::RefCell<Vec<ZoneInfoFieldSlots>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let vm = ctx.vm_identity();
    let cid = ctx.class_id_of_object(this);
    if let Some(last) = LAST_SLOTS.with(std::cell::Cell::get) {
        if last.vm == vm && last.cid == cid {
            return last;
        }
    }
    if let Some(hit) = SLOTS.with(|c| {
        c.borrow()
            .iter()
            .find(|s| s.vm == vm && s.cid == cid)
            .copied()
    }) {
        LAST_SLOTS.with(|last| last.set(Some(hit)));
        return hit;
    }
    let resolved = ZoneInfoFieldSlots {
        vm,
        cid,
        id: ctx.resolve_field_index_by_class_id(cid, "ID"),
        raw_offset: ctx.resolve_field_index_by_class_id(cid, "rawOffset"),
        raw_offset_diff: ctx.resolve_field_index_by_class_id(cid, "rawOffsetDiff"),
    };
    SLOTS.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= 16 {
            cache.clear();
        }
        cache.push(resolved);
    });
    LAST_SLOTS.with(|last| last.set(Some(resolved)));
    resolved
}

fn zone_slot_int(ctx: &mut dyn NativeContext, this: ObjectRef, slot: Option<usize>) -> i32 {
    match slot.map(|s| ctx.get_field(this, s)) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

/// The receiver's `ID` field, read through cached slot indices.
fn zone_id_of(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let slots = zone_info_field_slots(ctx, this);
    match slots.id.map(|s| ctx.get_field(this, s)) {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

struct ZoneObjRulesCacheEntry {
    vm: usize,
    epoch: u64,
    id_addr: usize,
    id: String,
    rules: Option<Arc<ZoneRulesData>>,
}

thread_local! {
    static TLS_ZONE_OBJ_RULES: std::cell::RefCell<Vec<ZoneObjRulesCacheEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn zone_slot_typed_int(ctx: &mut dyn NativeContext, this: ObjectRef, slot: Option<usize>) -> i32 {
    match slot.map(|s| ctx.get_field_typed(this, s, b'I')) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

/// Read `(ID, rawOffset, rawOffsetDiff)` from a `TimeZone`/`ZoneInfo` receiver
/// using thread-local cached field slots.
pub(crate) fn zone_id_and_offset_fields(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> (String, i32, i32) {
    let slots = zone_info_field_slots(ctx, this);
    let id = match slots.id.map(|s| ctx.get_field_typed(this, s, b'L')) {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let raw = zone_slot_typed_int(ctx, this, slots.raw_offset);
    let diff = zone_slot_typed_int(ctx, this, slots.raw_offset_diff);
    (id, raw, diff)
}

/// Zero-allocation fast path for `ZoneInfo`/`TimeZone` intrinsics: resolves
/// `(rawOffset, rawOffsetDiff)` and borrows cached `Option<Arc<ZoneRulesData>>`
/// keyed by `(vm, gc_address_epoch, id_obj_addr)` so steady-state calls skip
/// `read_string`, UTF-16 decode, `String` alloc, `Arc::clone`, and `Mutex`.
/// Shared `(total_ms, dst_ms)` computation for `ZoneInfo.getOffset` /
/// `getOffsets` / `getOffsetsByWall`.
pub fn zone_info_offsets_words(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    millis: i64,
    by_wall: bool,
) -> (i32, i32) {
    with_zone_rules_and_offsets(ctx, this, |rules_opt, raw_field, diff| {
        let (total, dst) = match rules_opt {
            Some(rules) if by_wall => {
                legacy_wall_offsets_ms_of(rules, millis.saturating_sub(diff as i64))
            }
            Some(rules) => legacy_offsets_ms_of(rules, millis),
            None => (raw_field, 0),
        };
        (total.saturating_add(diff), dst)
    })
}

/// Zero-wrapper JIT leaf fast path for `ZoneInfo.getOffsets(J[I)I` and
/// `ZoneInfo.getOffsetsByWall(J[I)I`. Declines (`None`) when a non-null array is
/// shorter than 2 elements so the callback path raises the standard error.
pub fn jit_zone_info_get_offsets(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    millis: i64,
    offsets: Option<ObjectRef>,
    by_wall: bool,
) -> Option<i32> {
    let (total_ms, dst_ms) = zone_info_offsets_words(ctx, this, millis, by_wall);
    if let Some(arr) = offsets {
        let words = [total_ms - dst_ms, dst_ms];
        if !ctx.write_int_array_from(arr, 0, &words) {
            return None;
        }
    }
    Some(total_ms)
}

fn native_zone_info_get_offsets_shared(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    by_wall: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let millis = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let offsets_arr = args.get(2).cloned().unwrap_or(Value::Object(None));
    if let Value::Object(Some(arr)) = offsets_arr {
        if ctx.array_length(arr) < 2 && ctx.decline_leaf_call() {
            return Ok(Some(Value::Int(0)));
        }
    }
    let (total_ms, dst_ms) = zone_info_offsets_words(ctx, this, millis, by_wall);
    if let Some(Value::Object(Some(arr))) = Some(&offsets_arr) {
        ctx.set_array_element(*arr, 0, Value::Int(total_ms - dst_ms));
        ctx.set_array_element(*arr, 1, Value::Int(dst_ms));
    }
    Ok(Some(Value::Int(total_ms)))
}

pub fn native_zone_info_get_offsets(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_zone_info_get_offsets_shared(ctx, args, false)
}

pub fn native_zone_info_get_offsets_by_wall(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_zone_info_get_offsets_shared(ctx, args, true)
}

pub fn is_zone_info_get_offsets_native(cb: NativeCallback) -> bool {
    cb as usize == native_zone_info_get_offsets as usize
}

pub fn is_zone_info_get_offsets_by_wall_native(cb: NativeCallback) -> bool {
    cb as usize == native_zone_info_get_offsets_by_wall as usize
}

pub(crate) fn with_zone_rules_and_offsets<R>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(Option<&ZoneRulesData>, i32, i32) -> R,
) -> R {
    let slots = zone_info_field_slots(ctx, this);
    let (id_obj, raw, diff) = match (slots.id, slots.raw_offset, slots.raw_offset_diff) {
        (Some(id_s), Some(raw_s), Some(diff_s)) => {
            let mut vals = [Value::Int(0), Value::Int(0), Value::Int(0)];
            ctx.get_fields_typed(this, &[(id_s, b'L'), (raw_s, b'I'), (diff_s, b'I')], &mut vals);
            let id = match vals[0] {
                Value::Object(Some(s)) => Some(s),
                _ => None,
            };
            let raw = match vals[1] {
                Value::Int(v) => v,
                _ => 0,
            };
            let diff = match vals[2] {
                Value::Int(v) => v,
                _ => 0,
            };
            (id, raw, diff)
        }
        _ => {
            let id = match slots.id.map(|s| ctx.get_field_typed(this, s, b'L')) {
                Some(Value::Object(Some(s))) => Some(s),
                _ => None,
            };
            let raw = zone_slot_typed_int(ctx, this, slots.raw_offset);
            let diff = zone_slot_typed_int(ctx, this, slots.raw_offset_diff);
            (id, raw, diff)
        }
    };
    let Some(id_ref) = id_obj else {
        return f(None, raw, diff);
    };
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let id_addr = id_ref.as_ptr() as usize;
    let mut f_opt = Some(f);
    let fast = TLS_ZONE_OBJ_RULES.with(|c| {
        let b = c.borrow();
        b.iter()
            .find(|e| e.vm == vm && e.epoch == epoch && e.id_addr == id_addr)
            .and_then(|e| f_opt.take().map(|cb| cb(e.rules.as_deref(), raw, diff)))
    });
    if let Some(res) = fast {
        return res;
    }
    let id = ctx.read_string(id_ref).unwrap_or_default();
    let rules = if id.is_empty() {
        None
    } else {
        get_zone_rules(ctx, &id)
    };
    let res = f_opt
        .take()
        .expect("closure not consumed on cache miss")(rules.as_deref(), raw, diff);
    TLS_ZONE_OBJ_RULES.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= 16 {
            cache.clear();
        }
        if !cache
            .iter()
            .any(|e| e.vm == vm && e.epoch == epoch && e.id_addr == id_addr)
        {
            cache.push(ZoneObjRulesCacheEntry {
                vm,
                epoch,
                id_addr,
                id,
                rules,
            });
        }
    });
    res
}

/// The receiver's own offset state that a lookup by `ID` cannot see (nio20
/// wave 5, L4 fix-back; `Nio20LocaleProviders tz`): `(rawOffset,
/// rawOffsetDiff)` in ms. `ZoneInfo.setRawOffset(ms)` records
/// `rawOffsetDiff = ms - rawOffset`, and every offset the JDK's `ZoneInfo`
/// answers adds it (`getLastRawOffset()`, the `offsets[..] + rawOffsetDiff`
/// arm of `getOffsets`). The offset intrinsics answer by tzdb lookup of `ID`,
/// so they dropped the diff: `TimeZone.getDefault().setRawOffset(3600000)`
/// then `getRawOffset()` read 0 in both modes where HotSpot reads 3600000.
/// `rawOffset` is the fallback for an `ID` the catalog does not resolve --
/// a `setID` relabel, or a custom `GMT+hh:mm` zone -- where the JDK's
/// `ZoneInfo` keeps answering its own fields.
pub fn zone_offset_fields_ms(ctx: &mut dyn NativeContext, this: ObjectRef) -> (i32, i32) {
    let slots = zone_info_field_slots(ctx, this);
    (
        zone_slot_int(ctx, this, slots.raw_offset),
        zone_slot_int(ctx, this, slots.raw_offset_diff),
    )
}

/// Registers the five daylight-saving members on `sun/util/calendar/ZoneInfo`
/// **and on nothing else**.
///
/// WHY NOT ALSO ON `java/util/TimeZone`, when its sibling offset family in
/// `lib.rs` IS registered on the base. `TimeZone.getDSTSavings()` and
/// `TimeZone.observesDaylightTime()` are CONCRETE on the base class. An
/// application's own `extends TimeZone` that does not override them has no
/// bytecode of its own for the superclass climb to stop at, so a base
/// registration would answer that subclass from a tzdb lookup of its `ID` —
/// which is precisely the `C6-2` defect (an id treated as a zone when it is an
/// opaque label) recreated in a new place. MEASURED: the base class's rows for
/// the already-registered offset family read `invocations=0` under `--real-jdk`,
/// so the narrow registration gives up nothing that runs.
///
/// `java.util.SimpleTimeZone` declares all five itself and could not have been
/// captured through the base in any case.
///
/// KIND: `Intrinsic`, explicitly, on all five -- see the full reasoning on
/// `register_tzdb_offset_natives_for` in `lib.rs`, which this family shares
/// it with (same `alloc_synth_timezone` allocation, same unpopulated
/// `transitions`/`offsets`/`simpleTimeZoneParams`, same
/// `bridge.stated_shadows_bytecode` ratchet failure when first tried as
/// `Bridge`). Investigated and stated 2026-09-21 while re-freezing the
/// kind-map baseline.
pub fn register_zoneinfo_dst_natives(registry: &mut NativeMethodRegistry) {
    const ZONE_INFO: &str = "sun/util/calendar/ZoneInfo";

    registry.with_leaf(true, |registry| {
        registry.register(
            ZONE_INFO,
            "getDSTSavings",
            "()I",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let sav = with_zone_rules_and_offsets(ctx, this, |rules, _, _| {
                    rules.map(|r| dst_savings_seconds(r).saturating_mul(1000)).unwrap_or(0)
                });
                Ok(Some(Value::Int(sav)))
            },
            cratonvm_native_api::NativeKind::Intrinsic,
        );

        registry.register(
            ZONE_INFO,
            "useDaylightTime",
            "()Z",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let uses = with_zone_rules_and_offsets(ctx, this, |rules, _, _| {
                    rules.map(uses_daylight_time).unwrap_or(false)
                });
                Ok(Some(Value::Int(i32::from(uses))))
            },
            cratonvm_native_api::NativeKind::Intrinsic,
        );

        registry.register(
            ZONE_INFO,
            "observesDaylightTime",
            "()Z",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let now_millis = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let observes = with_zone_rules_and_offsets(ctx, this, |rules, _, _| {
                    rules.map(|r| observes_daylight_time_of(r, now_millis)).unwrap_or(false)
                });
                Ok(Some(Value::Int(i32::from(observes))))
            },
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    });

    registry.register(
        ZONE_INFO,
        "inDaylightTime",
        "(Ljava/util/Date;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let date = match args.get(1) {
                Some(Value::Object(Some(d))) => *d,
                // `ZoneInfo.inDaylightTime(null)` is a bare
                // `throw new NullPointerException()` -- getMessage() is NULL,
                // unlike `SimpleTimeZone`'s helpful helper-generated one. A
                // `Some(String::new())` here would build a non-null empty
                // message and diverge; `None` is the only spelling that
                // reproduces it.
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            // `Date.getTime()` is a real virtual call that can allocate and
            // safepoint -- a deprecated `Date` setter leaves `cdate` dirty and
            // forces a normalise -- so both references are pinned across it.
            // Same reason `ssl_security.rs` does it this way for
            // `checkValidity(Date)`.
            let this_pin = ctx.pin_native_root(this);
            let date_pin = ctx.pin_native_root(date);
            let called = ctx.invoke_virtual(date, "getTime", "()J", &[]);
            let this = ctx.read_native_pin(this_pin, this);
            let date = ctx.read_native_pin(date_pin, date);
            ctx.unpin_native_roots(this_pin);
            let millis = match called? {
                Some(Value::Long(ms)) => ms,
                // A synthetic `Date` with no bytecode keeps its instant in
                // slot 0.
                _ => match ctx.get_field(date, 0) {
                    Value::Long(ms) => ms,
                    _ => return Ok(Some(Value::Int(0))),
                },
            };
            let id = zone_id_of(ctx, this);
            let in_dst = get_zone_rules(ctx, &id)
                .map(|r| in_daylight_time_of(&r, millis))
                .unwrap_or(false);
            Ok(Some(Value::Int(i32::from(in_dst))))
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );

    registry.with_leaf(true, |registry| {
        registry.register(
            ZONE_INFO,
            "getOffset",
            "(IIIIII)I",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let int_at = |i: usize| args.get(i).and_then(|v| v.as_int()).unwrap_or(0);
                let era = int_at(1);
                let year_arg = int_at(2);
                let month0 = int_at(3);
                let day = int_at(4);
                let day_of_week = int_at(5);
                let millis_in_day = int_at(6);

                const DAY_IN_MILLIS: i32 = 86_400_000;
                if !(0..DAY_IN_MILLIS).contains(&millis_in_day) {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: String::new(),
                    }
                    .into());
                }
                let year = match era {
                    0 => 1i32.wrapping_sub(year_arg),
                    1 => year_arg,
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: String::new(),
                        }
                        .into())
                    }
                };
                if !gregorian_date_is_valid(year, month0.saturating_add(1), day) {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: String::new(),
                    }
                    .into());
                }
                if !(1..=7).contains(&day_of_week) {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: String::new(),
                    }
                    .into());
                }

                let local_standard_ms = to_epoch_day(year, month0.saturating_add(1), day)
                    .saturating_mul(86_400_000)
                    .saturating_add(millis_in_day as i64);
                let total = with_zone_rules_and_offsets(ctx, this, |rules, raw_field, diff| {
                    rules
                        .map(|r| offset_ms_at_local_standard_of(r, local_standard_ms))
                        .unwrap_or(raw_field)
                        .saturating_add(diff)
                });
                Ok(Some(Value::Int(total)))
            },
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    });
}

// ---------------------------------------------------------------------------
// W7-92 — the HOST's own zone, as an id `TimeZone`/`ZoneId` can resolve.
// ---------------------------------------------------------------------------
//
// This is the missing input, not a missing table: everything above computes
// offsets correctly *for a zone id it is given*, and both producers of the
// default zone handed it `"UTC"` on every host. See
// docs/feature-designs/native-dispatch-architecture.md.
//
// Two rules govern the code below, both earned by this campaign:
//
//   * **Never fabricate a zone.** Every id returned here has been resolved
//     against the same `tzdb.dat` catalog the offset natives read; anything
//     the platform does not state, or that the catalog does not know, answers
//     `None` so the caller keeps its existing UTC/GMT answer. A wrong non-UTC
//     zone is worse than UTC, which is at least recognisable as a stub.
//   * **Never hard-code *this* host's zone.** Nothing below names a zone; the
//     names all come from the OS and from `<java.home>/lib/tzmappings`.

/// `DYNAMIC_TIME_ZONE_INFORMATION` (winbase.h). `SYSTEMTIME` is eight
/// `WORD`s, so `[u16; 8]` is layout-identical to it and saves declaring a
/// second struct nothing reads the fields of. Offsets: bias 0, standardName
/// 4, standardDate 68, standardBias 84, daylightName 88, daylightDate 152,
/// daylightBias 168, timeZoneKeyName 172, dynamicDaylightTimeDisabled 428;
/// size 432 with `align(4)` from the `LONG`s.
#[cfg(windows)]
#[repr(C)]
struct DynamicTimeZoneInformation {
    bias: i32,
    standard_name: [u16; 32],
    standard_date: [u16; 8],
    standard_bias: i32,
    daylight_name: [u16; 32],
    daylight_date: [u16; 8],
    daylight_bias: i32,
    time_zone_key_name: [u16; 128],
    dynamic_daylight_time_disabled: u8,
}

/// `TIME_ZONE_ID_INVALID` — the only failure code
/// `GetDynamicTimeZoneInformation` returns (0/1/2 all mean success, and say
/// which of standard/daylight time is in force).
#[cfg(windows)]
const TIME_ZONE_ID_INVALID: u32 = 0xFFFF_FFFF;

/// The host's time-zone information, straight from Win32.
///
/// Real HotSpot's `TimeZone_md.c` reads
/// `HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation\TimeZoneKeyName`
/// out of the registry. `GetDynamicTimeZoneInformation` returns that exact
/// value in its `TimeZoneKeyName` field — it is the documented API over the
/// same data — so no registry API is needed and therefore no new crate
/// dependency. `kernel32` is linked by the Rust standard library on every
/// `*-pc-windows-*` target, and this entry point exists on Vista and later,
/// so the import cannot fail to bind on any host that can run the VM at all.
#[cfg(windows)]
fn dynamic_time_zone_information() -> Option<DynamicTimeZoneInformation> {
    unsafe {
        extern "system" {
            fn GetDynamicTimeZoneInformation(
                lpTimeZoneInformation: *mut DynamicTimeZoneInformation,
            ) -> u32;
        }
        let mut tzi: DynamicTimeZoneInformation = std::mem::zeroed();
        if GetDynamicTimeZoneInformation(&mut tzi) == TIME_ZONE_ID_INVALID {
            return None;
        }
        Some(tzi)
    }
}

/// The platform's own name for its zone, in the platform's own namespace —
/// on Windows a time-zone *key* name ("Argentina Standard Time"), which is
/// not an IANA id and must go through [`map_windows_zone_key`].
///
/// A blank key is a real state, not a read failure: a machine whose zone was
/// installed with `SetTimeZoneInformation` rather than picked from the
/// registry's list has no key name, and HotSpot's `getWinTimeZone` gives up
/// on it too (falling through to its GMT-offset id). `None` here, never a
/// guess.
#[cfg(windows)]
fn platform_zone_key() -> Option<String> {
    let tzi = dynamic_time_zone_information()?;
    let key = &tzi.time_zone_key_name;
    let end = key.iter().position(|&u| u == 0).unwrap_or(key.len());
    let name = String::from_utf16_lossy(&key[..end]).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(not(windows))]
fn platform_zone_key() -> Option<String> {
    None
}

/// `TimeZone_md.c`'s `customZoneName`: a Win32 `Bias` is the number of
/// minutes to ADD to local time to get UTC, so a *positive* bias is a zone
/// WEST of Greenwich and the rendered sign is the opposite of the bias's.
/// The bias used is the zone's STANDARD one, exactly as HotSpot's
/// `getGMTOffsetID` does — a custom id carries no DST rule, so folding the
/// current daylight saving into it would misreport the zone for half the
/// year.
#[cfg(windows)]
fn custom_zone_name(bias_minutes: i32) -> String {
    let sign = if bias_minutes > 0 { '-' } else { '+' };
    // A hostile/absurd bias must not panic an arithmetic-overflow check in a
    // debug build; real values are within ±14h.
    let mins = bias_minutes.unsigned_abs().min(24 * 60);
    format!("GMT{sign}{:02}:{:02}", mins / 60, mins % 60)
}

/// The host's standard UTC offset as a custom `"GMT±HH:MM"` id — the body
/// behind `TimeZone.getSystemGMTOffsetID`.
///
/// This is the JDK's own second chance, not a hedge:
/// `TimeZone.setDefaultZone()` calls it precisely when the named id it just
/// obtained from `getSystemTimeZoneID` is one `TimeZone.getTimeZone(id,
/// false)` could not resolve, and uses the result instead. It is what keeps
/// the host's OFFSET right even where the host's zone NAME cannot be
/// resolved, and it is the reason this fix does not depend on the Java-side
/// `ZoneInfoFile`/`tzdb.dat` path being healthy.
#[cfg(windows)]
pub fn platform_gmt_offset_id() -> Option<String> {
    dynamic_time_zone_information().map(|tzi| custom_zone_name(tzi.bias))
}

#[cfg(not(windows))]
pub fn platform_gmt_offset_id() -> Option<String> {
    None
}

/// Map a Windows time-zone key name to an IANA id through
/// `<java.home>/lib/tzmappings` — the file real HotSpot's `matchJavaTZ`
/// reads, and the reason the JDK hands `getSystemTimeZoneID` a `javaHome`
/// argument at all.
///
/// One row per line, no header and no comment lines in the JDK 25 file
/// (CRLF-terminated, which `str::lines` strips):
/// `<Windows key>:<region>:<IANA id>:`. `matchJavaTZ` prefers the row whose
/// region equals the host's ISO-3166 country code and falls back to the row
/// whose region is `001` — CLDR's "world" default.
///
/// **Only the `001` row is consulted here.** Reading the host's country would
/// mean a second Win32 import (`GetUserDefaultGeoName`, Windows 10 1709+ —
/// an import that would fail to BIND, killing the whole process, on an older
/// host), and the rows it would select cannot differ in offset: every row
/// under one Windows key describes the same Windows zone, i.e. the same
/// standard offset and the same DST rule, and differs only in which IANA
/// name (and therefore which pre-modern history) it points at. So this
/// narrowing can change the id STRING relative to HotSpot in a
/// country-specific case, never the offset the id computes.
fn map_windows_zone_key(java_home: &str, key: &str) -> Option<String> {
    let path = std::path::Path::new(java_home)
        .join("lib")
        .join("tzmappings");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `matchJavaTZ` skips any row with fewer than three fields rather than
        // giving up on the file, and so does this.
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 3 {
            continue;
        }
        if fields[0] == key && fields[1] == "001" && !fields[2].is_empty() {
            return Some(fields[2].to_string());
        }
    }
    None
}

/// `/etc/timezone` holds the plain IANA id on Debian/Ubuntu/Arch and most
/// modern distros. `/etc/localtime` — the symlink RHEL-family and Alpine use
/// instead — is deliberately not followed, the same choice
/// `util_time.rs::os_default_zone_id` documents.
///
/// `$TZ` is deliberately NOT read here even though it is the POSIX override:
/// `vm_init` already turns it into the `user.timezone` system property, and
/// both callers of [`system_zone_id`] consult `user.timezone` FIRST — real
/// `TimeZone.setDefaultZone()` by its own bytecode, the `getDefault` native
/// explicitly. Reading it a third time here could only ever disagree with
/// those two.
#[cfg(unix)]
fn unix_zone_id() -> Option<String> {
    let contents = std::fs::read_to_string("/etc/timezone").ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The host's own zone as an id `java.util.TimeZone.getTimeZone` and
/// `java.time.ZoneId.of` both resolve, or `None` when the platform does not
/// say — in which case the caller keeps whatever it answered before.
///
/// `java_home` is the argument `TimeZone.getSystemTimeZoneID(String)` was
/// handed; pass `None` to take it from the `java.home` system property.
pub fn system_zone_id(ctx: &mut dyn NativeContext, java_home: Option<&str>) -> Option<String> {
    // `<java.home>/lib` holds both files this needs — `tzmappings` to map the
    // platform's key, `tzdb.dat` to check the answer — so a VM with no
    // `java.home` yet cannot resolve anything. It must not TRY, either:
    // `catalog`'s `OnceLock` latches its first result for the process, so one
    // premature call would leave every tzdb-backed offset native answering
    // from an empty catalog for the rest of the run.
    let java_home = match java_home {
        Some(home) if !home.is_empty() => home.to_string(),
        _ => ctx
            .get_system_property("java.home")
            .filter(|home| !home.is_empty())?,
    };

    let mut candidate = platform_zone_key().and_then(|key| map_windows_zone_key(&java_home, &key));
    #[cfg(unix)]
    {
        if candidate.is_none() {
            candidate = unix_zone_id();
        }
    }
    let id = candidate?;

    // The gate against answering a name nothing downstream can resolve.
    // `get_zone_rules` follows `tzdb.dat`'s own alias table — which is how
    // `tzmappings`' "America/Buenos_Aires" reaches the
    // "America/Argentina/Buenos_Aires" rules — so a hit here means the real
    // `ZoneId.of(id)` and `TimeZone.getTimeZone(id)` resolve it as well.
    // The id is returned VERBATIM rather than canonicalised, because the
    // verbatim `tzmappings` value is what real HotSpot answers and HotSpot is
    // the oracle.
    if get_zone_rules(ctx, &id).is_some() {
        Some(id)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
