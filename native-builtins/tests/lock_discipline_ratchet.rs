// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! LOCK-DISCIPLINE RATCHET — a one-way ratchet on raw lock constructions in
//! `cratonvm-native-builtins`.
//!
//! ## The finding (ARCH-2026-08-04 A6)
//!
//! The workspace has a designed lock hierarchy: [`LockLevel`] L0..L10 in
//! `types/src/lock_order.rs`, with `OrderedPlMutex` / `OrderedPlRwLock` wrappers
//! that assert acquisition order (strictly decreasing level) and name the
//! violation. Adoption on 2026-08-04:
//!
//! | Crate | Ordered | Raw `Mutex::new` / `RwLock::new` |
//! |-------|--------:|---------------------------------:|
//! | `vm` | 116 | 319 |
//! | `native-builtins` | **0** | **440** |
//!
//! (A plain `grep` over the crate reports 481. This gate counts 440 because it
//! excludes `#[cfg(test)]` regions and comment lines — a lock in a test fixture
//! is not part of the runtime's discipline. 440 is the production figure and is
//! the one to quote.)
//!
//! `native-builtins` is the largest crate in the workspace *and* the one that
//! re-enters the VM: a native callback calls back into Java, which takes heap
//! locks and the L10 class-manager lock. That is precisely the shape a lock
//! hierarchy exists to police, and it had no coverage at all — 440
//! independently-locked pieces of global state with no ordering relation to
//! each other or to the VM's levels. The `ordered` column of this gate's own
//! output is the live proof: it read 0 before this change.
//!
//! Two locks have since been converted (`AOT_CACHE_INPUT_PATH` /
//! `AOT_CACHE_OUTPUT_PATH` in `src/aot.rs`, at `LockLevel::Scratch`), bringing
//! the baseline to 438. They were picked because the claim is *checkable*:
//! every one of their six acquisition sites clones the `Option<String>` and
//! drops the guard before touching `ctx`, so none is held across a re-entry
//! into the VM. That is the standard the rest of the backlog has to meet.
//!
//! Four more on 2026-08-10 — `ds_side_table`, `ds_peer_table` and
//! `ssc_side_table` in `src/net_phase_e.rs`, and `pending_connect_sockets` in
//! `src/phases_late/ssl_security.rs`, all at `LockLevel::Scratch` — paying back
//! the four raw locks that had landed since the last freeze. Each met the same
//! standard: every acquisition site read, no `ctx` call under the guard.
//!
//! Twenty-four more on 2026-08-17, all at `LockLevel::Scratch`, paying back the
//! twenty-four raw locks that had landed since the 2026-08-11 freeze (`dev` was
//! red at 456). They were found by blaming every raw-lock site and taking the
//! ones newer than that freeze, which is also why they cluster: five JFR tables
//! in `src/jfr.rs`, eleven TLS side-tables in `src/t27_tls.rs`, and one each in
//! `src/tls.rs`, `src/jca/signature.rs` (two), `src/http_url_connection.rs`,
//! `src/phases_late/ssl_security.rs`, `src/locale_bootstrap.rs`,
//! `src/locale_resources.rs` and `src/phases_early.rs`.
//!
//! Sixteen already met the standard as written. The other eight did not, and
//! were made to — which is the part worth reading, because each was a real
//! lock-held-across-a-VM-re-entry, not a bookkeeping detail:
//!
//! * Five evaluated `ctx.identity_hash_code` (via `gc_stable_objref_key` /
//!   `engine_objref_key` / `sig_key`) *inside* the lock expression, e.g.
//!   `table().lock().insert(gc_stable_objref_key(ctx, ses), sid)`. The key is
//!   idempotent, so hoisting it to a `let` above the acquisition is
//!   behaviour-preserving and takes the `ctx` call out from under the guard.
//! * Three held a guard across a body that re-enters the VM, because in edition
//!   2021 an `if let` / `match` scrutinee's temporaries live for the whole
//!   construct: `client_session_cache` across `touch_session_access_time`,
//!   `kmf_live_km_id_by_identity` across a field walk, and `https_peer_info`
//!   across `throw_jca_exc`. Each now binds the (copied or cloned) row to a
//!   local first, so the guard drops before the body runs.
//!
//! Twenty-one more on 2026-08-19, all at `LockLevel::Scratch`, paying back the
//! twenty raw locks that had landed since the 2026-08-17 freeze (`dev` was red
//! at 452) and retiring one more besides. Found the same way — census the
//! crate at the freeze commit and at `HEAD`, take the per-file delta, then
//! blame the sites in the files that grew:
//!
//! * blame needs `-w --ignore-rev` here. A line-ending normalisation commit
//!   ("chore: normalise nine files back to LF before merging dev") re-blames
//!   every line of nine files, and reported **39** new sites where the census
//!   delta is 20. A date is not an attribution.
//!
//! The twenty-one: `CACHE` (the NIST curve parameters, `src/crypto_impl.rs`),
//! `https_response_streams` (`src/http_url_connection.rs`),
//! `BOOLEAN_COMPONENTS` (`src/intrinsics/record.rs`), `FILE_PROPS`
//! (`src/jca/provider_chain.rs`), `x500_der_table` (`src/jca/x500.rs`), the
//! three box caches `CHARACTER_CACHE` / `BYTE_CACHE` / `SHORT_CACHE`
//! (`src/lang_math.rs`), `surrogate_intern_pool` (`src/lang_string.rs`),
//! `ssc_carrier_roots` and `sss_option_delegates` (`src/net_phase_e.rs`),
//! `BC_SHA256_SLOTS` (`src/phases_late/bouncycastle.rs`), `TlsEntry::stream`
//! (`src/servlet.rs`, four construction sites for one field), `sss_mode_states`,
//! `sss_enabled_suites_table`, `session_peer_endpoint_table` and
//! `session_invalidated_table` (`src/t27_tls.rs`), and `ArenaStore::translated`
//! (`src/unsafe_natives_ext.rs`).
//!
//! Sixteen met the standard as written. Five did not:
//!
//! * `session_peer_endpoint_table`, `session_invalidated_table` and
//!   `https_response_streams` evaluated the key — `gc_stable_objref_key` /
//!   `ctx.identity_hash_code` — INSIDE the lock expression. Same hoist as the
//!   five the 2026-08-17 round fixed, and behaviour-preserving for the same
//!   reason: the key is idempotent.
//! * `FILE_PROPS` held its guard across `ctx.get_system_property` AND a
//!   `read_to_string` of `java.security`. The parse now runs outside the lock
//!   and only the publish is taken under it; two threads that miss together
//!   both parse and `get_or_insert` keeps the first, which is the same map.
//! * `session_is_valid` had the guard inside a `&&` whose left operand calls
//!   back into the VM; it is now an early `return` with the key hoisted.
//!
//! `TlsEntry::stream` is the one that lowers the baseline. It is a single
//! FIELD with four construction sites, three of them new; converting the field
//! necessarily converts the fourth (`TlsClientStream::Native`), which predates
//! the freeze. That is a lock retired below the frozen figure, so
//! [`BASELINE_RAW_LOCKS`] drops 432 -> 431 in this same change — the
//! bookkeeping the 2026-08-11 and 2026-08-17 entries declined to do because
//! they had retired nothing.
//!
//! `ArenaStore::translated` is `Scratch` only because its enclosing lock is
//! unordered: `real_ptr` holds `ArenaStore::inner`'s `RwLock` across it. Same
//! caveat as the two JFR tables — if `inner` is ever given a level it must be
//! a HIGHER one, never an equal one.
//!
//! Not converted, and worth naming so the next person does not re-derive it:
//! `boot_layer_memo` (`src/jboss_jdkspecific.rs`) and `p60_current_handle_memo`
//! (`src/phases_late.rs`) both hold their guard across `ctx.add_global_root`,
//! which is exactly the re-entrant shape a level is supposed to forbid. They
//! need the publish restructured before a level can honestly be stamped on
//! them, so they stay in the §A6 backlog rather than being given one.
//!
//! Joined on 2026-08-17 by `java_recordings` and `java_event_streams`
//! (`src/jfr.rs`), for a harder version of the same reason: both hold their
//! guard across a `ctx.resolve_global_root` that runs *per element*, inside an
//! `iter().position(..)` predicate over the table itself. That cannot be
//! hoisted the way the eight above were — it needs the lookup restructured — so
//! they keep no level, and the two JFR tables that ARE taken under their guard
//! (`java_events_admitted`, `java_events_known_disabled`) say so in their own
//! comments. Those two are only safely `Scratch` *because* the enclosing lock
//! is unordered; if `java_recordings` is ever given a level, it must be a
//! higher one than theirs, not an equal one.
//!
//! Joined on 2026-08-19 by `INTEGER_CACHE_HIGH` (`src/lang_math.rs`), which is
//! the interesting one: it is the OUTER lock of a two-lock nest over the
//! unordered `INTEGER_CACHE`. A level claims nothing at or below it is held on
//! acquisition, and the ordering that implies runs the wrong way here — the
//! INNER lock would have to sit below this one, and `Scratch` is the floor.
//! Any higher level would be this cache asserting a place in the VM's own
//! hierarchy. The nesting is load-bearing (`high` and the matching `entries`
//! must be published atomically or a racing thread can pair a `high` with a
//! wrong-length cache), so the publish has to be restructured before a level
//! can be stamped. Its three siblings in that file — `CHARACTER_CACHE`,
//! `BYTE_CACHE`, `SHORT_CACHE` — take no nested lock and were converted.
//!
//! ## Why this gate ratchets instead of converting
//!
//! Converting all 440 mechanically would be worse engineering than this gate,
//! not better. `OrderedPlMutex::new` takes a [`LockLevel`], and the level is a
//! *claim*: "no lock at or below this level is ever held when this one is
//! taken". Stamping 440 locks with a level nobody reasoned about makes the
//! checker assert something unverified — it would either fire constantly on
//! correct code or, worse, pass while encoding a false hierarchy. A wrong level
//! is more dangerous than no level, because it reads as audited.
//!
//! The honest unit of work is per-lock: decide whether that lock can be held
//! across a re-entry into the VM, and pick the level from the answer. Most of
//! these are leaf caches that belong at `LockLevel::Scratch`; the interesting
//! minority are the ones held across a `NativeContext` callback, and those are
//! the actual latent deadlocks this program should surface. That audit is
//! tracked in `architecture-review-a1-a9.md` §A6.
//!
//! What this gate does is stop the number growing while that work happens. A
//! new raw lock fails CI; converting one to an ordered wrapper requires
//! lowering [`BASELINE_RAW_LOCKS`] in the same change, which locks in the win.
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test lock_discipline_ratchet -- --nocapture
//! ```
//!
//! [`LockLevel`]: cratonvm_types::lock_order::LockLevel

use std::path::{Path, PathBuf};

/// Frozen upper bound on raw lock constructions in `native-builtins/src`.
///
/// Measured 2026-08-04. Zero slack, matching `stub_ratchet`'s contract.
///
/// Lowered 438 → 432 on 2026-08-05. The six conversions are NOT from the change
/// that lowered it: they arrived on `dev` without the same-change re-freeze this
/// ratchet asks for, so the test was already red on `dev`. Verified by re-running
/// the scan with that change's own source edits checked out — it also counts 432,
/// and the change removes no lock construction at all. Recorded here rather than
/// silently adjusted, because a baseline moved by someone other than the author
/// of the improvement is the bookkeeping this ratchet exists to keep honest.
///
/// Lowered 432 -> 428 on 2026-08-19. `dev` was red at 452; twenty-four
/// conversions pay back the twenty that had landed AND retire four more. Unlike
/// the two entries below, this number moves — that is what "converting one to an
/// ordered wrapper requires lowering the baseline in the same change" means when
/// it actually happens.
///
/// The four beyond the payback are not a bonus, they are unavoidable: a lock is
/// converted by changing a TYPE, and two of these types are shared.
/// `TlsEntry::stream` is one field with four construction sites, of which only
/// three are new. The `valueOf` box caches share `cached_wrapper_box`,
/// `scan_one_cache`, `update_one_cache` and `canonical_wrapper_if_cached::read`,
/// all four of which take the mutex by type — so `CHARACTER_CACHE`,
/// `BYTE_CACHE` and `SHORT_CACHE` could not convert without `INTEGER_CACHE`,
/// `BOOLEAN_CACHE` and `LONG_CACHE` coming with them. Verified safe together:
/// `gc_scan_value_of_cache_roots` and `canonical_wrapper_if_cached` take these
/// guards one at a time, never nested, so six locks at an equal level cannot
/// trip the checker.
///
/// Held at 432 again on 2026-08-17 while converting twenty-four locks, for the
/// same reason as the 2026-08-11 entry below: `dev` had gone red at 456, and
/// the twenty-four conversions pay that regression back exactly. No lock has
/// been retired below the frozen figure, so lowering the number would overstate
/// what was won.
///
/// Held at 432 on 2026-08-11 while converting one lock. `jar_manifest`'s
/// `mtime_memo` landed as a raw `Mutex` on 2026-08-10 (14f3eb6e0) and took the
/// count to 433, i.e. `dev` was red; converting it to `OrderedPlMutex` at
/// `LockLevel::Scratch` pays that back exactly. So this number is unchanged and
/// the ratchet is green again — a conversion that lowered it would have been
/// the wrong bookkeeping, because no lock has been retired below the frozen
/// figure.
const BASELINE_RAW_LOCKS: usize = 428;

/// Minimum number of source lines the scan must see before its count means
/// anything.
///
/// A source-scanning gate that stops finding its own source reports zero hits
/// and **passes** — indistinguishable from a clean tree. This crate is ~565k
/// lines; 100k is far below any plausible real shrink and far above anything a
/// broken walk would produce.
const MIN_LINES_SCANNED: usize = 100_000;

/// Sanity floor on the raw-lock count itself.
///
/// Belt-and-braces against a scan that walks the right files but stops matching
/// (a mangled needle, a CRLF split, an over-eager comment filter). A count of
/// zero here would sail past the `<=` assertion below while proving nothing.
const MIN_RAW_LOCKS_FOUND: usize = 400;

fn crate_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `vendor/` holds third-party sources this crate does not own.
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Count raw `Mutex::new` / `RwLock::new` constructions outside test code.
///
/// Returns `(raw_locks, ordered_locks, lines_scanned)`.
///
/// Line-oriented on purpose. `str::lines` splits on `\n` and strips a trailing
/// `\r`, so this reads identically on an LF and a CRLF checkout — a gate in
/// this repo has already been red on Windows and green on Linux for splitting
/// on a literal `"\n}\n"`.
fn census() -> (usize, usize, usize, Vec<String>) {
    let mut files = Vec::new();
    rust_files(&crate_src(), &mut files);
    files.sort();

    let mut raw = 0usize;
    let mut ordered = 0usize;
    let mut lines_scanned = 0usize;
    let mut sites = Vec::new();

    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        let mut pending_test = false;
        let mut depth: i32 = 0;

        for (n, line) in src.lines().enumerate() {
            lines_scanned += 1;
            let t = line.trim_start();

            // Skip `#[cfg(test)]`-gated items: a lock in a test fixture is not
            // part of the runtime's discipline.
            let is_comment = t.starts_with("//") || t.starts_with('*');
            if !is_comment && depth == 0 && !pending_test && t.starts_with("#[cfg(test)]") {
                pending_test = true;
                continue;
            }
            if pending_test {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                if line.contains('{') && depth <= 0 {
                    pending_test = false;
                    depth = 0;
                } else if depth > 0 {
                    pending_test = false;
                }
                continue;
            }
            if depth > 0 {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }

            // A mention in a comment is not a construction.
            if is_comment {
                continue;
            }

            if t.contains("OrderedPlMutex")
                || t.contains("OrderedPlRwLock")
                || t.contains("OrderedMutex")
            {
                ordered += 1;
                continue;
            }
            if line.contains("Mutex::new") || line.contains("RwLock::new") {
                raw += 1;
                if sites.len() < 40 {
                    sites.push(format!("{rel}:{}", n + 1));
                }
            }
        }
    }

    (raw, ordered, lines_scanned, sites)
}

#[test]
fn raw_lock_constructions_do_not_grow() {
    let (raw, ordered, lines, sites) = census();

    println!(
        "lock-discipline: {raw} raw lock constructions, {ordered} ordered, \
         across {lines} lines (baseline {BASELINE_RAW_LOCKS})"
    );
    if raw > BASELINE_RAW_LOCKS {
        println!("first sites:");
        for s in &sites {
            println!("  {s}");
        }
    }

    assert!(
        lines >= MIN_LINES_SCANNED,
        "only {lines} lines scanned (floor {MIN_LINES_SCANNED}). The walk broke \
         rather than the crate shrinking — fix the scan before touching the \
         baseline."
    );
    assert!(
        raw >= MIN_RAW_LOCKS_FOUND,
        "only {raw} raw locks found (floor {MIN_RAW_LOCKS_FOUND}). The needle \
         stopped matching; a zero here would pass the ceiling assertion below \
         while proving nothing."
    );

    assert!(
        raw <= BASELINE_RAW_LOCKS,
        "raw lock constructions in native-builtins rose to {raw} (baseline \
         {BASELINE_RAW_LOCKS}).\n\
         This crate re-enters the VM — a native callback calls back into Java, \
         which takes the heap and L10 class-manager locks — so a new global \
         lock here with no LockLevel is a new deadlock the checker cannot see.\n\
         Use OrderedPlMutex / OrderedPlRwLock with a level you can justify \
         (see types/src/lock_order.rs), or explain in review why this lock \
         cannot participate in a cycle. Do NOT raise the baseline."
    );

    // Ratchet: a conversion must lower the baseline in the same change,
    // otherwise the win leaks back the next time someone adds a lock.
    assert_eq!(
        raw, BASELINE_RAW_LOCKS,
        "raw lock constructions fell to {raw} (baseline {BASELINE_RAW_LOCKS}). \
         Good — now set BASELINE_RAW_LOCKS to {raw} in this same change."
    );
}
