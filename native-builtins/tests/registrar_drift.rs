// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! REGISTRAR MODE-DRIFT GATE — a `(class, name, descriptor)` triple may not
//! newly acquire two implementations, one per compatibility mode.
//!
//! # The species
//!
//! `NativeMethodRegistry::register` is last-write-wins with no unregister
//! API, and `register_builtins` runs `register_essential_natives` and *then*
//! `register_synthetic_overrides` — which is `#[cfg(feature = "synthetic-jdk")]`
//! and in no crate's default feature set. So when one triple is registered by
//! BOTH a pass that only `register_synthetic_overrides` can reach and a pass
//! the shipping binary reaches:
//!
//! * in synthetic-JDK mode the synthetic-only body is registered LAST and wins;
//! * in the shipping modes (`--jdk-only` included) the synthetic-only pass is
//!   not compiled in at all, so the shipping body is the only one there.
//!
//! **The two modes run different code for that triple**, and every test built
//! with `--features synthetic-jdk` measures the copy that does not ship. That
//! is the `register_pe_panama` / `structLayout` defect generalised: two files
//! carried two different layout objects behind one descriptor, and which one a
//! caller got was decided by which mode you booted. `phases_late/collections.rs`
//! and `native-collections/src/lib.rs` still carry that exact shape for
//! `TreeMap.ceilingEntry` today (see [`MUST_DRIFT`]).
//!
//! `registrar_reachability.rs` gates the *population* — which passes are
//! synthetic-only. It says nothing about which triples they share with the
//! shipping side. This file is that second gate, and F34-1 §8.1 is the request
//! for it.
//!
//! # What is new here, relative to `registrar_reachability.rs`
//!
//! 1. **Triple granularity.** Reachability is answered per pass; drift has to
//!    be answered per `(class, name, descriptor)`. F34-1 §8.2 records the gap
//!    this closes: a family whose classes are all also registered by a shipping
//!    pass, but where the shipping pass registers *fewer methods*, is invisible
//!    at class granularity.
//! 2. **Five crates, not one.** `native-builtins`, `native-collections`,
//!    `native-io`, `native-awt`, `vm`, plus the two `native-builtins-*`
//!    satellites. F34-1 §8.3 records that ~17 synthetic-only registrars live
//!    outside `native-builtins`, and — more importantly for drift — that most
//!    shipping TWINS do (`register_io_natives`, `register_tree_map_natives`,
//!    `channel_register_native` are all outside this crate). A one-crate scan
//!    manufactures false drift *and* misses real drift.
//! 3. **`for` loops are expanded.** F34-1 warned that four rows come out of a
//!    `for` loop so a `registry.register(` grep undercounts. It is not four:
//!    in this tree 695 register sites sit inside a `for x in ["a", "b"] { .. }`
//!    and expand to more than one triple each. A scan that cannot expand them
//!    silently drops whole classes.
//!
//! # What this scanner CANNOT see, stated up front
//!
//! Never trust the number without this list. It is deliberately identical to
//! the list in `docs/known-issues/jdk-only/G3-1-…-20260816.md` §3.
//!
//! * **Descriptors built with `format!`** — 18 sites. W7-5 §0 records a 60%
//!   over-statement that came from exactly this blind spot, which is why this
//!   file counts them (the `unresolved` map) instead of pretending they do
//!   not exist.
//! * **Registrars parameterised by class name** (`fn register_x(r, class:
//!   &str)`) — the class arrives from the call site, and this scanner does not
//!   propagate it. Their sites land in `unresolved`.
//! * **`for (name, desc) in [(..), (..)]`** — tuple-destructuring loops.
//! * **Iterating a named array const** (`for c in MAP_VIEW_CARRIERS`).
//! * **`NativeKind`.** This is the big one. `--jdk-only` REFUSES any
//!   registration whose kind is `SyntheticStub` (`native-api/src/registry.rs`
//!   `allowed_in`), so "the shipping copy wins in `--jdk-only`" is only true
//!   when the shipping copy is a `Bridge` or an `Intrinsic`. Kind is ambient
//!   registry state (`set_category` / `with_category`), threaded through call
//!   chains, and a source scan cannot resolve it. `java/time/Instant` is the
//!   worked counter-example: its shipping twin is tagged `SyntheticStub`, so in
//!   `--jdk-only` NEITHER copy is registered and the JDK's own bytecode runs.
//!   **A row in the baseline is a claim that two registrations exist, not a
//!   claim about which body a `--jdk-only` process ends up dispatching.** Only
//!   `--dump-native-registry` can answer that.
//!
//! # Why the numbers are a snapshot and not an invariant
//!
//! [`DRIFT_BASELINE`] is **measured**, at 2026-08-16, on the working tree — it
//! is not a specification and nothing derives it. It exists so this gate
//! ratchets instead of failing on day one against 1,266 pre-existing rows.
//! Every row is a debt, not a permission.
//!
//! It is a **ceiling**, not an equality, and that is a deliberate weakening:
//! see [`no_new_mode_drift`] for exactly what that does and does not catch.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ===========================================================================
// SECTION 1 — configuration and the measured baseline
// ===========================================================================

/// Crates scanned, relative to the workspace root. Every one of them either
/// defines a synthetic-only registrar or defines a shipping twin of one; a
/// census that drops any of them reports a wrong number in both directions.
const CRATES: &[&str] = &[
    "native-builtins",
    "native-collections",
    "native-io",
    "native-awt",
    "vm",
    "native-builtins-crypto",
    "native-builtins-security",
];

/// The population is keyed on the ARGUMENT, not on the name: a registration
/// pass need not be called `register_*`. If this type is ever renamed, every
/// floor in [`the_drift_scanner_is_not_vacuous`] fires at once — which is the
/// point, and is the hole F34-1 §M4a found in its own gate.
const KEY_TYPE: &str = "NativeMethodRegistry";

/// The pass every synthetic-only chain runs through.
const SYNTHETIC_OVERRIDES: &str = "register_synthetic_overrides";

/// The cargo feature that gates it.
const SYNTHETIC_FEATURE: &str = "synthetic-jdk";

// --- measured floors -------------------------------------------------------
// Every one is a MEASURED value at 2026-08-16 minus generous slack. A floor
// with no measurement behind it is decoration; a floor that tracks the
// measurement exactly is a maintenance tax that gets relaxed under pressure.

/// `.rs` files parsed across all seven crate `src` trees (measured 361).
const MIN_FILES: usize = 250;
/// `fn` definitions parsed (measured 33,710).
const MIN_FN_DEFS: usize = 20_000;
/// Definitions whose signature mentions `NativeMethodRegistry` (measured 872).
const MIN_PASSES: usize = 600;
/// `.register(` call sites found, of any arity (measured 14,063).
const MIN_REGISTER_SITES: usize = 9_000;
/// Sites whose first three arguments all resolved (measured 12,600).
const MIN_RESOLVED_SITES: usize = 8_000;
/// Distinct `(class, name, descriptor)` triples recovered (measured 11,422).
const MIN_TRIPLES: usize = 7_000;
/// Passes reachable from the shipping side (measured 521).
const MIN_SHIPPING: usize = 350;
/// Synthetic-only passes (measured 280; `registrar_reachability.rs` says 284
/// for `native-builtins` alone — this scan sees the other crates' shipping
/// call sites, so a few names it calls synthetic-only are not).
const MIN_SYNTHETIC_ONLY: usize = 200;
/// Direct synthetic-only children of `register_synthetic_overrides`
/// (measured 73, exactly `registrar_reachability.rs`'s allow-list length).
const MIN_DIRECT_SYNTHETIC_ONLY: usize = 60;
/// Bytes of `register_synthetic_overrides`' extracted body (measured 104,360).
/// F34-1 §M4b: when the locator broke, ONLY a body-size floor noticed.
const MIN_SYNTHETIC_OVERRIDES_BODY: usize = 60_000;
/// Drifting triples (measured 1,266). This is the anti-vacuity floor: a
/// scanner that has stopped resolving descriptors reports a small, clean,
/// entirely fictional number, and every other assertion here passes.
const MIN_TOTAL_DRIFT: usize = 900;
/// Sites inside an expanded `for` loop (measured 695). Without loop expansion
/// whole classes vanish from the census with no other symptom.
const MIN_LOOP_EXPANDED_SITES: usize = 400;

/// Ceiling on total drift. Measured 1,266 at 2026-08-16.
const BASELINE_TOTAL_DRIFT: usize = 1_266;

/// Two triples that pin BOTH answers.
///
/// A one-sided control is worthless here: a scanner that has stopped
/// discriminating answers "drift" for everything and sails past a positive-only
/// control, and one that has stopped resolving answers "no drift" for
/// everything and sails past a negative-only one.
///
/// **POSITIVE** — `TreeMap.ceilingEntry` MUST drift. `register_p62_navigable_expansion`
/// (`native-builtins/src/phases_late/collections.rs:374`, synthetic-only) binds it to
/// `p62_tm_ceiling_entry`, a linear scan over slots 0/1 using
/// `natural_compare_values`; `register_tree_map_natives`
/// (`native-collections/src/lib.rs`, shipping) binds it to
/// `native_tm_ceiling_entry`, which honours a user `Comparator`, calls
/// `tm_sync_native_state` first, has a `tm_fast_with` BTree path, and refreshes
/// `data` after a comparator call because that call can move the heap. The two
/// answer differently, and the one that is tested is the weaker one.
///
/// **NEGATIVE** — `MemoryLayout.structLayout` MUST NOT drift. It is F34-1's
/// worked example, and it was FIXED by F16: `panama.rs::register_pe2_struct_layouts`
/// deleted its whole group-layout family (the comment at `panama.rs:5829`
/// records why), leaving `phases_late/foreign_ffm.rs::register_p67_foreign_memory`
/// as the sole registrant. If this control ever reports drift, the twin came
/// back — which is the single most likely way this defect recurs.
const CONTROL_POSITIVE: (&str, &str, &str) = (
    "java/util/TreeMap",
    "ceilingEntry",
    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
);
const CONTROL_NEGATIVE: (&str, &str, &str) = (
    "java/lang/foreign/MemoryLayout",
    "structLayout",
    "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
);

/// Triples that must be present in the census at all, drifting or not.
///
/// Distinct from the controls: these guard the RESOLVER rather than the drift
/// verdict. Each needs a different resolution path to be recovered, so if any
/// one of them goes missing the corresponding path has broken silently.
const RESOLVER_WITNESSES: &[(&str, &str, &str, &str)] = &[
    (
        "java/io/ByteArrayOutputStream",
        "toByteArray",
        "()[B",
        "a `let cls = \"...\"` binding read out of the enclosing fn",
    ),
    (
        "java/util/concurrent/atomic/AtomicBoolean",
        "compareAndSet",
        "(ZZ)Z",
        "a `let c = \"...\"` binding inside a `with_category` closure",
    ),
    (
        "java/lang/foreign/MemoryLayout$PathElement",
        "groupElement",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout$PathElement;",
        "a plain three-literal call in panama.rs",
    ),
];

/// Per-pass drift, **measured 2026-08-16**, used as a per-pass CEILING.
///
/// Not an invariant, not a target, and emphatically not a permission: every row
/// is a triple whose behaviour depends on which mode you booted. The number
/// beside a name is what the scanner found on that day, nothing more.
///
/// To re-take it after a deliberate change, re-run the Python mirror described
/// in the record (`docs/known-issues/jdk-only/G3-1-…-20260816.md` §8) with
/// `GATE_MODE=1`; that mode restricts the mirror to exactly the resolution
/// rules implemented below, so the two do not drift apart.
///
/// A name absent from this table has a ceiling of ZERO — so a pass that starts
/// drifting fails even though nothing else about it changed.
const DRIFT_BASELINE: &[(&str, usize)] = &[
    ("register_aot_natives", 1),
    ("register_atomic_boolean_natives", 8),
    ("register_bigdecimal_natives", 18),
    ("register_biginteger_natives", 19),
    ("register_body_handlers", 3),
    ("register_body_publisher", 3),
    ("register_byte_array_output_stream", 12),
    ("register_classloader_define_class", 1),
    ("register_classloader_natives", 81),
    ("register_completable_future_natives", 3),
    ("register_core_stdlib_extras", 97),
    ("register_crypto_impl_natives", 2),
    ("register_enterprise_final_natives", 17),
    ("register_enum_map_natives", 1),
    ("register_enum_natives", 4),
    ("register_forkjoin_extras", 3),
    ("register_forkjoin_natives", 35),
    ("register_formatter_natives", 8),
    ("register_http_client", 9),
    ("register_http_client_builder", 3),
    ("register_http_headers", 1),
    ("register_http_request", 5),
    ("register_http_request_builder", 11),
    ("register_http_response", 3),
    ("register_java_lang_extras_natives", 27),
    ("register_key_manager_factory", 4),
    ("register_logging_natives", 20),
    ("register_m18_concurrent_fixes", 35),
    ("register_object_input_stream", 1),
    ("register_object_stream_class", 1),
    ("register_p58_completable_future", 16),
    ("register_p58_nio_channels", 4),
    ("register_p59_management", 31),
    ("register_p59_module", 18),
    ("register_p59_package", 1),
    ("register_p59_spliterator", 4),
    ("register_p59_varhandle", 3),
    ("register_p60_flow", 2),
    ("register_p60_http_client", 26),
    ("register_p61_charset", 5),
    ("register_p61_classloader", 8),
    ("register_p61_files_path", 7),
    ("register_p61_logging", 9),
    ("register_p61_net", 2),
    ("register_p61_reflect", 7),
    ("register_p61_text_formatting", 2),
    ("register_p62_abstract_map_entries", 3),
    ("register_p62_navigable_expansion", 24),
    ("register_p63_enumeration", 5),
    ("register_p63_resource_bundle", 9),
    ("register_p63_scheduled_executor", 18),
    ("register_p63_service_loader", 6),
    ("register_p64_collectors_teeing", 1),
    ("register_p64_sequenced_collections", 3),
    ("register_p64_stream_modern", 2),
    ("register_p65_checked_collections", 3),
    ("register_p65_priority_blocking_queue", 8),
    ("register_p65_stream_map_multi", 1),
    ("register_p67_misc", 16),
    ("register_p69_spliterator", 8),
    ("register_p70_atomic_accumulators", 5),
    ("register_p70_misc", 2),
    ("register_p71_files_bridge", 5),
    ("register_p71_logging_extras", 17),
    ("register_p71_wrapper_extras", 2),
    ("register_p72_datagram", 8),
    ("register_p72_naming", 9),
    ("register_p72_server_socket", 8),
    ("register_pe2_string_marshaling", 2),
    ("register_pe_arena", 7),
    ("register_pe_function_descriptor", 2),
    ("register_pe_linker", 2),
    ("register_phase52_byte_order", 5),
    ("register_phase52_function_extras", 20),
    ("register_phase52_objects_extras", 4),
    ("register_phase52_url_encoding", 6),
    ("register_phase53_crypto", 16),
    ("register_phase53_security", 29),
    ("register_phase54_atomics", 79),
    ("register_phase54_net_extras", 21),
    ("register_phase55_charset", 8),
    ("register_phase55_collection_extras", 12),
    ("register_phase55_executors", 13),
    ("register_phase56_collectors_extras", 16),
    ("register_phase56_stream_extras", 24),
    ("register_phase57_file_channel", 9),
    ("register_phase57_random_access_file", 18),
    ("register_s1_classloading", 17),
    ("register_s2_selector", 5),
    ("register_s2_server_socket_channel", 2),
    ("register_s2_socket_channel", 2),
    ("register_s3_http_client", 4),
    ("register_security_natives", 10),
    ("register_slf4j_natives", 45),
    ("register_ssl_context", 10),
    ("register_ssl_engine", 11),
    ("register_ssl_engine_result", 4),
    ("register_ssl_parameters", 2),
    ("register_ssl_session", 11),
    ("register_ssl_socket_factory", 4),
    ("register_synthetic_overrides", 173),
    ("register_t25_natives", 1),
    ("register_t310_scripting", 2),
    ("register_t311_i18n", 1),
    ("register_t31_concurrent_extras", 11),
    ("register_t38_jndi", 7),
    ("register_t39_stax", 11),
    ("register_time_extras_natives", 1),
    ("register_time_natives", 16),
    ("register_timeunit_natives", 9),
    ("register_trust_manager_factory", 4),
    ("register_unsafe_define_class", 2),
];

/// Triples whose two implementations were READ and found to differ materially.
///
/// These are the *live* rows: the shipping body and the synthetic-only body do
/// different things, so a `--features synthetic-jdk` test measuring one says
/// nothing about the other. Each must still be drifting — if one stops, either
/// it was fixed (delete the row and say so) or the scanner stopped seeing it.
const MUST_DRIFT: &[(&str, &str, &str, &str)] = &[
    (
        "java/util/TreeMap",
        "ceilingEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        "p62_tm_ceiling_entry is a comparator-blind linear scan; native_tm_ceiling_entry \
         honours the Comparator, syncs native state and survives a moving GC",
    ),
    (
        "java/util/TreeSet",
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "same split as TreeMap.ceilingEntry, on the Set side (p62_ts_ceiling vs \
         native_ts_ceiling)",
    ),
    (
        "java/io/ByteArrayOutputStream",
        "close",
        "()V",
        "serialization.rs registers a no-op; native-io's native_baos_close dispatches \
         BaosEvent::Close and runs process_pipe_output_close",
    ),
    (
        "java/time/Instant",
        "getEpochSecond",
        "()J",
        "native_inst_get_epoch_second returns the raw slot; native_synthetic_instant_* \
         coerce Int<->Long. The shipping twin is tagged SyntheticStub, so in --jdk-only \
         NEITHER is registered — the row is here as the standing counter-example to \
         'the shipping copy wins'",
    ),
];

// ===========================================================================
// SECTION 2 — the scanner
// ===========================================================================

mod scan {
    /// One parsed file. `nc` has comments blanked but keeps string CONTENT;
    /// `full` additionally blanks string content. Both preserve byte offsets
    /// and newlines, so one offset indexes both.
    ///
    /// Two buffers are needed because the two jobs conflict: structure (braces,
    /// `fn` headers, call graph) must not see text inside literals, and the
    /// registered class/name/descriptor ARE text inside literals.
    pub struct FileSrc {
        pub rel: String,
        pub nc: Vec<u8>,
        pub full: Vec<u8>,
    }

    /// One `fn` definition. `body` is the offset of its `{`, `end` one past the
    /// matching `}`.
    pub struct FnDef {
        pub file: usize,
        pub name: String,
        pub line: usize,
        pub start: usize,
        pub body: usize,
        pub end: usize,
        pub takes_registry: bool,
        pub syn_gated: bool,
        pub testish: bool,
        pub parent: Option<usize>,
    }

    #[inline]
    pub fn is_ident_start(b: u8) -> bool {
        b.is_ascii_alphabetic() || b == b'_'
    }
    #[inline]
    pub fn is_ident(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn wipe(out: &mut [u8], from: usize, to: usize) {
        let to = to.min(out.len());
        if from >= to {
            return;
        }
        for b in out[from..to].iter_mut() {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }

    /// Blank comments (into both buffers) and string/char literal CONTENT
    /// (into `full` only). Delimiters survive in both, so `full` still shows
    /// `""` where a literal was and the argument parser can tell a literal from
    /// an identifier.
    ///
    /// Region boundaries are ASCII and whole regions are blanked, so a
    /// multi-byte character inside a comment becomes N spaces rather than a
    /// broken code unit; the result is still valid UTF-8.
    pub fn blank(src: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let n = src.len();
        let mut nc = src.to_vec();
        let mut full = src.to_vec();
        let mut i = 0usize;
        while i < n {
            let c = src[i];
            if c == b'/' && i + 1 < n && src[i + 1] == b'/' {
                let j = src[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(n, |p| i + p);
                wipe(&mut nc, i, j);
                wipe(&mut full, i, j);
                i = j;
            } else if c == b'/' && i + 1 < n && src[i + 1] == b'*' {
                // Rust block comments nest.
                let mut depth = 0usize;
                let mut j = i;
                while j < n {
                    if src[j] == b'/' && j + 1 < n && src[j + 1] == b'*' {
                        depth += 1;
                        j += 2;
                    } else if src[j] == b'*' && j + 1 < n && src[j + 1] == b'/' {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                wipe(&mut nc, i, j);
                wipe(&mut full, i, j);
                i = j;
            } else if (c == b'r' || c == b'b')
                && (i == 0 || !is_ident(src[i - 1]))
                && raw_string_open(src, i).is_some()
            {
                let (content, end) = raw_string_open(src, i).expect("checked on the line above");
                wipe(&mut full, content, end);
                i = end;
                // step past the closing delimiter too
                while i < n && (src[i] == b'"' || src[i] == b'#') {
                    i += 1;
                }
            } else if c == b'"' {
                let mut j = i + 1;
                while j < n {
                    if src[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if src[j] == b'"' {
                        break;
                    }
                    j += 1;
                }
                wipe(&mut full, i + 1, j);
                i = (j + 1).min(n);
            } else if c == b'\'' {
                // Char literal or lifetime. A lifetime (`'a`, `'static`) must
                // not be treated as an unterminated literal, and `'\\'` must
                // not be walked with a "skip the char after a backslash" loop:
                // that swallows the literal's own closing quote and blanks
                // real code — braces included — until the next `'` in the
                // file. The brace-balance self-check in `parse_file` exists
                // because that bug produced a plausible-looking wrong answer.
                if i + 2 < n && src[i + 1] == b'\\' {
                    if let Some(p) = src[i + 2..].iter().take(8).position(|&b| b == b'\'') {
                        let end = i + 3 + p;
                        wipe(&mut full, i + 1, end - 1);
                        i = end;
                        continue;
                    }
                    i += 1;
                } else if i + 2 < n && src[i + 2] == b'\'' {
                    wipe(&mut full, i + 1, i + 2);
                    i += 3;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        (nc, full)
    }

    /// If a raw string starts at `i`, return `(content_start, content_end)`.
    fn raw_string_open(src: &[u8], i: usize) -> Option<(usize, usize)> {
        let n = src.len();
        let mut p = i;
        if src[p] == b'b' {
            p += 1;
            if p >= n || src[p] != b'r' {
                return None;
            }
        }
        if p >= n || src[p] != b'r' {
            return None;
        }
        p += 1;
        let hash_start = p;
        while p < n && src[p] == b'#' {
            p += 1;
        }
        let hashes = p - hash_start;
        if p >= n || src[p] != b'"' {
            return None;
        }
        let content = p + 1;
        let mut j = content;
        while j < n {
            if src[j] == b'"'
                && src[j + 1..]
                    .iter()
                    .take(hashes)
                    .filter(|&&b| b == b'#')
                    .count()
                    == hashes
            {
                return Some((content, j));
            }
            j += 1;
        }
        Some((content, n))
    }

    /// Index one past the `}` matching the `{` at `open`.
    pub fn match_brace(b: &[u8], open: usize) -> usize {
        let mut d = 0i64;
        let mut k = open;
        while k < b.len() {
            match b[k] {
                b'{' => d += 1,
                b'}' => {
                    d -= 1;
                    if d == 0 {
                        return k + 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        b.len()
    }

    /// Index one past the `)` matching the `(` at `open`.
    pub fn match_paren(b: &[u8], open: usize) -> usize {
        let mut d = 0i64;
        let mut k = open;
        while k < b.len() {
            match b[k] {
                b'(' => d += 1,
                b')' => {
                    d -= 1;
                    if d == 0 {
                        return k + 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        b.len()
    }

    /// Index one past the `]` matching the `[` at `open`.
    pub fn match_bracket(b: &[u8], open: usize) -> usize {
        let mut d = 0i64;
        let mut k = open;
        while k < b.len() {
            match b[k] {
                b'[' => d += 1,
                b']' => {
                    d -= 1;
                    if d == 0 {
                        return k + 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        b.len()
    }

    /// True when `at` begins the keyword `kw` as a whole token.
    pub fn word_at(b: &[u8], at: usize, kw: &[u8]) -> bool {
        if !b[at..].starts_with(kw) {
            return false;
        }
        if at > 0 && is_ident(b[at - 1]) {
            return false;
        }
        let after = at + kw.len();
        after >= b.len() || !is_ident(b[after])
    }

    pub fn skip_ws(b: &[u8], mut p: usize) -> usize {
        while p < b.len() && (b[p] == b' ' || b[p] == b'\t' || b[p] == b'\r' || b[p] == b'\n') {
            p += 1;
        }
        p
    }

    /// Every `.rs` file under `dir`, sorted, skipping test/build trees.
    pub fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if p.is_dir() {
                // `.claude` holds sibling worktrees — whole extra copies of
                // this repo. Descending into one doubles every count and makes
                // the answer depend on which lanes are running.
                if matches!(
                    name.as_str(),
                    ".git"
                        | "target"
                        | "node_modules"
                        | ".claude"
                        | "tests"
                        | "benches"
                        | "examples"
                        | "fuzz"
                ) {
                    continue;
                }
                rs_files(&p, out);
            } else if name.ends_with(".rs") {
                out.push(p);
            }
        }
    }
}

use scan::{
    is_ident, is_ident_start, match_brace, match_bracket, match_paren, skip_ws, word_at, FileSrc,
    FnDef,
};

/// A `(class, name, descriptor)` triple.
type Triple = (String, String, String);

/// Split a comma-separated argument list at depth 0. Returns `(full, nc)`
/// slices as owned, trimmed strings — parallel views of the same bytes.
fn split_args(full: &[u8], nc: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut depth = 0i64;
    let mut start = 0usize;
    let push = |a: usize, b: usize, out: &mut Vec<(String, String)>| {
        let f = String::from_utf8_lossy(&full[a..b]).trim().to_string();
        let c = String::from_utf8_lossy(&nc[a..b]).trim().to_string();
        if !f.is_empty() {
            out.push((f, c));
        }
    };
    for (i, &b) in full.iter().enumerate() {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                push(start, i, &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(start, full.len(), &mut out);
    out
}

/// Undo the Rust escapes this scanner can encounter in a class/method/descriptor
/// literal. `$` and `/` need none; `\\` appears in a handful of Windows path
/// descriptors and `\"` in none, but both are handled so a future one is not a
/// silent corruption.
fn unescape(lit: &str) -> String {
    let inner = lit
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(lit);
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Is this argument a single string literal? Checked on the BLANKED text, so
/// the content cannot fake a quote, then read from the unblanked text.
fn as_literal(full: &str, nc: &str) -> Option<String> {
    let f = full.trim();
    if f.len() >= 2
        && f.starts_with('"')
        && f.ends_with('"')
        && f.bytes().filter(|&b| b == b'"').count() == 2
    {
        return Some(unescape(nc.trim()));
    }
    None
}

/// Strip a leading `&` and a trailing `.as_str()`-style adaptor, so
/// `&CLS_FOO.as_str()` resolves the same as `CLS_FOO`.
///
/// The two inputs are byte-parallel views of the same source range — blanking
/// preserves length — so ONE pair of offsets can be applied to both. Trimming
/// them independently would desynchronise them the moment a literal's content
/// happened to be whitespace.
fn strip_adaptors(full: &str, nc: &str) -> (String, String) {
    let fb = full.as_bytes();
    let mut lo = 0usize;
    let mut hi = fb.len();
    let ws = |b: u8| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n';
    while lo < hi && ws(fb[lo]) {
        lo += 1;
    }
    while hi > lo && ws(fb[hi - 1]) {
        hi -= 1;
    }
    while lo < hi && fb[lo] == b'&' {
        lo += 1;
        while lo < hi && ws(fb[lo]) {
            lo += 1;
        }
    }
    for tail in [".as_str()", ".as_ref()", ".to_string()", ".clone()"] {
        if hi >= lo + tail.len() && &full[hi - tail.len()..hi] == tail {
            hi -= tail.len();
            while hi > lo && ws(fb[hi - 1]) {
                hi -= 1;
            }
            break;
        }
    }
    let f = full.get(lo..hi).unwrap_or("").to_string();
    let c = nc.get(lo..hi).unwrap_or("").to_string();
    (f, c)
}

fn is_plain_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                is_ident_start(b)
            } else {
                is_ident(b)
            }
        })
}

/// Last segment of a `a::b::CONST` path, or `None` if it is not such a path.
fn path_tail(s: &str) -> Option<&str> {
    let tail = s.rsplit("::").next()?;
    if s.contains("::") && is_plain_ident(tail) {
        Some(tail)
    } else {
        None
    }
}

// ===========================================================================
// SECTION 3 — the analysis
// ===========================================================================

/// Everything the assertions read, computed once per test process.
struct Analysis {
    files: usize,
    fn_defs: usize,
    passes: usize,
    register_sites: usize,
    resolved_sites: usize,
    loop_expanded_sites: usize,
    unresolved: BTreeMap<String, usize>,
    triples: usize,
    shipping: usize,
    synthetic_only: BTreeSet<String>,
    direct_synthetic_only: usize,
    synthetic_overrides_body: usize,
    brace_imbalanced_files: Vec<String>,
    /// Every triple registered anywhere -> the passes that register it.
    registrants: BTreeMap<Triple, BTreeSet<String>>,
    /// Drifting triples -> (synthetic-only registrants, shipping registrants).
    drift: BTreeMap<Triple, (BTreeSet<String>, BTreeSet<String>)>,
    /// Drift count per synthetic-only pass.
    per_pass: BTreeMap<String, usize>,
    /// `pass -> "file:line"`, for legible failure messages.
    where_defined: BTreeMap<String, String>,
    /// `triple -> "file:line"` sites, for the same reason.
    sites_of: BTreeMap<Triple, Vec<String>>,
}

fn analysis() -> &'static Analysis {
    static A: OnceLock<Analysis> = OnceLock::new();
    A.get_or_init(build_analysis)
}

/// One file's `fn` definitions, with parent links, plus the file's own
/// brace-balance self-check.
fn parse_file(idx: usize, src: &FileSrc, raw: &str) -> (Vec<FnDef>, bool) {
    let t = &src.full;
    let n = t.len();
    let raw_lines: Vec<&str> = raw.lines().collect();
    // line index for offsets
    let mut line_starts: Vec<usize> = vec![0];
    for (i, &b) in t.iter().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let line_of = |off: usize| -> usize {
        match line_starts.binary_search(&off) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    };

    let mut out: Vec<FnDef> = Vec::new();
    let mut i = 0usize;
    while i < n {
        if !word_at(t, i, b"fn") {
            i += 1;
            continue;
        }
        let kw = i;
        let mut p = skip_ws(t, i + 2);
        if p >= n || !is_ident_start(t[p]) {
            i += 2;
            continue;
        }
        let name_start = p;
        while p < n && is_ident(t[p]) {
            p += 1;
        }
        let name = String::from_utf8_lossy(&t[name_start..p]).into_owned();
        let mut q = skip_ws(t, p);
        // generic parameter list
        if q < n && t[q] == b'<' {
            let mut d = 0i64;
            while q < n {
                match t[q] {
                    b'<' => d += 1,
                    b'>' => {
                        d -= 1;
                        if d == 0 {
                            q += 1;
                            break;
                        }
                    }
                    b'{' | b';' => break,
                    _ => {}
                }
                q += 1;
            }
            q = skip_ws(t, q);
        }
        if q >= n || t[q] != b'(' {
            i = p;
            continue;
        }
        let pe = match_paren(t, q);
        let mut r = pe;
        while r < n && t[r] != b'{' && t[r] != b';' {
            r += 1;
        }
        if r >= n || t[r] == b';' {
            // a bodyless trait declaration
            i = pe;
            continue;
        }
        let body = r;
        let end = match_brace(t, body);
        let sig = String::from_utf8_lossy(&t[kw..body]).into_owned();
        let line = line_of(kw);
        // Attributes are the `#[...]` lines directly above, skipping doc
        // comments and blanks. Matching `registrar_reachability.rs` on purpose:
        // two gates that disagree about what a `#[cfg(test)]` covers would
        // disagree about the population for reasons nobody could see.
        let mut attrs: Vec<String> = Vec::new();
        let mut a = line as i64 - 2;
        while a >= 0 && (a as usize) < raw_lines.len() {
            let txt = raw_lines[a as usize].trim();
            if txt.starts_with("#[") || txt.starts_with("#![") {
                attrs.push(txt.to_string());
            } else if txt.starts_with("//") || txt.is_empty() || txt.starts_with(')') {
                // keep walking
            } else {
                break;
            }
            a -= 1;
        }
        let testish = attrs.iter().any(|s| {
            s.starts_with("#[test]") || s.contains("cfg(test") || s.contains("cfg(all(test")
        });
        let syn_gated = attrs.iter().any(|s| s.contains(SYNTHETIC_FEATURE));
        out.push(FnDef {
            file: idx,
            name,
            line,
            start: kw,
            body,
            end,
            takes_registry: sig.contains(KEY_TYPE),
            syn_gated,
            testish,
            parent: None,
        });
        i = body + 1;
    }

    out.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    // parent links, by a nesting stack over start order
    let mut stack: Vec<usize> = Vec::new();
    for k in 0..out.len() {
        // `.copied()` on purpose: the `while let` scrutinee must not hold a
        // borrow of `stack` across the `pop()` inside the body.
        while let Some(top) = stack.last().copied() {
            if out[top].end <= out[k].start {
                stack.pop();
            } else {
                break;
            }
        }
        out[k].parent = stack.last().copied();
        stack.push(k);
    }
    // `#[cfg(test)] mod` bodies, and downward propagation of testish
    let mut test_spans: Vec<(usize, usize)> = Vec::new();
    let mut j = 0usize;
    while j + 12 <= n {
        if t[j..].starts_with(b"#[cfg(test)]") {
            let mut k = j + 12;
            let mut saw_mod = false;
            while k < n && t[k] != b'{' {
                if word_at(t, k, b"mod") {
                    saw_mod = true;
                }
                k += 1;
            }
            if saw_mod && k < n {
                let e = match_brace(t, k);
                test_spans.push((k, e));
                j = e;
                continue;
            }
        }
        j += 1;
    }
    for k in 0..out.len() {
        // Computed into a local first, so no immutable borrow of `out` is
        // anywhere near the assignment that follows.
        let inside = test_spans
            .iter()
            .any(|&(s, e)| s <= out[k].start && out[k].end <= e);
        if inside {
            out[k].testish = true;
        }
    }
    for k in 0..out.len() {
        let mut p = out[k].parent;
        while let Some(pi) = p {
            if out[pi].testish {
                out[k].testish = true;
                break;
            }
            p = out[pi].parent;
        }
    }

    // A well-formed Rust file's braces balance after blanking. They will not if
    // a literal ate code, and the resulting census is confidently wrong rather
    // than obviously broken — so it is reported, never ignored.
    let opens = t.iter().filter(|&&b| b == b'{').count();
    let closes = t.iter().filter(|&&b| b == b'}').count();
    (out, opens == closes)
}

/// `const NAME: &str = "...";` / `static NAME: &str = "...";` in one file.
fn str_consts(src: &FileSrc) -> BTreeMap<String, String> {
    let t = &src.full;
    let nc = &src.nc;
    let n = t.len();
    let mut out = BTreeMap::new();
    let mut i = 0usize;
    while i < n {
        let is_const = word_at(t, i, b"const");
        let is_static = word_at(t, i, b"static");
        if !is_const && !is_static {
            i += 1;
            continue;
        }
        let mut p = skip_ws(t, i + if is_const { 5 } else { 6 });
        if p < n && word_at(t, p, b"mut") {
            p = skip_ws(t, p + 3);
        }
        if p >= n || !is_ident_start(t[p]) {
            i += 1;
            continue;
        }
        let ns = p;
        while p < n && is_ident(t[p]) {
            p += 1;
        }
        let name = String::from_utf8_lossy(&t[ns..p]).into_owned();
        let p2 = skip_ws(t, p);
        if p2 >= n || t[p2] != b':' {
            i = p;
            continue;
        }
        // The type must be `&str` / `&'static str` and the value one literal.
        // Deliberately NOT stopping at a newline: `const X: &str =` with the
        // literal on the next line is common in this tree, and stopping at the
        // line end drops those consts, which drops every triple that names
        // them — silently, and only in the gate, not in the mirror.
        let mut e = p2;
        while e < n && t[e] != b'=' && t[e] != b';' {
            e += 1;
        }
        if e >= n || t[e] != b'=' {
            i = p;
            continue;
        }
        let ty = String::from_utf8_lossy(&t[p2 + 1..e]);
        if !(ty.contains("str") && !ty.contains('[')) {
            i = e;
            continue;
        }
        let vs = skip_ws(t, e + 1);
        let mut ve = vs;
        while ve < n && t[ve] != b';' {
            ve += 1;
        }
        let full_v = String::from_utf8_lossy(&t[vs..ve.min(n)]).into_owned();
        let nc_v = String::from_utf8_lossy(&nc[vs..ve.min(n)]).into_owned();
        if let Some(v) = as_literal(full_v.trim(), nc_v.trim()) {
            out.insert(name, v);
        }
        i = ve.max(p);
    }
    out
}

/// `let x = "lit";` / `let x = SOME_CONST;` inside one fn body. FIRST binding
/// wins, deterministically; a shadowed rebinding is not modelled.
fn let_bindings(
    src: &FileSrc,
    from: usize,
    to: usize,
    file_consts: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    ambiguous: &BTreeSet<String>,
) -> BTreeMap<String, Option<String>> {
    let t = &src.full;
    let nc = &src.nc;
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut i = from;
    while i < to {
        if !word_at(t, i, b"let") {
            i += 1;
            continue;
        }
        let mut p = skip_ws(t, i + 3);
        if p < to && word_at(t, p, b"mut") {
            p = skip_ws(t, p + 3);
        }
        if p >= to || !is_ident_start(t[p]) {
            i += 3;
            continue;
        }
        let ns = p;
        while p < to && is_ident(t[p]) {
            p += 1;
        }
        let name = String::from_utf8_lossy(&t[ns..p]).into_owned();
        let mut e = p;
        while e < to && t[e] != b'=' && t[e] != b';' {
            e += 1;
        }
        if e >= to || t[e] != b'=' {
            i = p;
            continue;
        }
        let vs = skip_ws(t, e + 1);
        let mut ve = vs;
        while ve < to && t[ve] != b';' {
            ve += 1;
        }
        let fv = String::from_utf8_lossy(&t[vs..ve.min(to)]).into_owned();
        let cv = String::from_utf8_lossy(&nc[vs..ve.min(to)]).into_owned();
        let value = resolve_simple(&fv, &cv, file_consts, global, ambiguous);
        out.entry(name).or_insert(value);
        i = ve.max(p);
    }
    out
}

/// Literal, or `&str` const, and nothing else. Used for `let` right-hand sides
/// and `for`-loop array elements, where a local environment does not apply.
fn resolve_simple(
    full: &str,
    nc: &str,
    file_consts: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    ambiguous: &BTreeSet<String>,
) -> Option<String> {
    let (f, c) = strip_adaptors(full, nc);
    if let Some(v) = as_literal(&f, &c) {
        return Some(v);
    }
    let key: &str = if is_plain_ident(&f) {
        f.as_str()
    } else {
        path_tail(&f)?
    };
    if let Some(v) = file_consts.get(key) {
        return Some(v.clone());
    }
    if ambiguous.contains(key) {
        return None;
    }
    global.get(key).cloned()
}

/// A `for x in [ .. ] { .. }` loop over literal/const elements: the body span,
/// the bound variable, and the values it takes.
struct Loop {
    body: usize,
    end: usize,
    var: String,
    values: Option<Vec<String>>,
}

fn parse_loops(
    src: &FileSrc,
    file_consts: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    ambiguous: &BTreeSet<String>,
) -> Vec<Loop> {
    let t = &src.full;
    let nc = &src.nc;
    let n = t.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if !word_at(t, i, b"for") {
            i += 1;
            continue;
        }
        let mut p = skip_ws(t, i + 3);
        if p >= n || !is_ident_start(t[p]) {
            i += 3;
            continue;
        }
        let vs = p;
        while p < n && is_ident(t[p]) {
            p += 1;
        }
        let var = String::from_utf8_lossy(&t[vs..p]).into_owned();
        let mut q = skip_ws(t, p);
        if !(q < n && word_at(t, q, b"in")) {
            i = p;
            continue;
        }
        q = skip_ws(t, q + 2);
        if q < n && t[q] == b'&' {
            q = skip_ws(t, q + 1);
        }
        if q >= n || t[q] != b'[' {
            i = p;
            continue;
        }
        let be = match_bracket(t, q);
        let mut bs = be;
        while bs < n && t[bs] != b'{' && t[bs] != b';' {
            bs += 1;
        }
        if bs >= n || t[bs] != b'{' {
            i = be;
            continue;
        }
        let end = match_brace(t, bs);
        let elems = split_args(&t[q + 1..be - 1], &nc[q + 1..be - 1]);
        let mut vals = Vec::new();
        let mut ok = !elems.is_empty();
        for (ef, en) in &elems {
            match resolve_simple(ef, en, file_consts, global, ambiguous) {
                Some(v) => vals.push(v),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        out.push(Loop {
            body: bs,
            end,
            var,
            values: if ok { Some(vals) } else { None },
        });
        i = be;
    }
    out.sort_by_key(|l| l.body);
    out
}

fn build_analysis() -> Analysis {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace: &Path = manifest
        .parent()
        .expect("native-builtins has a parent directory (the workspace root)");

    // --- 1. read and blank -------------------------------------------------
    let mut paths: Vec<PathBuf> = Vec::new();
    for c in CRATES {
        let dir = workspace.join(c).join("src");
        if dir.is_dir() {
            scan::rs_files(&dir, &mut paths);
        }
    }
    paths.sort();

    let mut files: Vec<FileSrc> = Vec::with_capacity(paths.len());
    let mut fns: Vec<FnDef> = Vec::new();
    let mut fn_ranges: Vec<(usize, usize)> = Vec::new(); // per file: [lo, hi)
    let mut brace_imbalanced: Vec<String> = Vec::new();
    for p in &paths {
        let raw = std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
        let (nc, full) = scan::blank(raw.as_bytes());
        let rel = p
            .strip_prefix(workspace)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        let idx = files.len();
        files.push(FileSrc { rel, nc, full });
        let (mut defs, balanced) = parse_file(idx, &files[idx], &raw);
        if !balanced {
            brace_imbalanced.push(files[idx].rel.clone());
        }
        let lo = fns.len();
        // parent indices are file-local; rebase them onto the global vector
        for d in defs.iter_mut() {
            d.parent = d.parent.map(|k| k + lo);
        }
        fns.append(&mut defs);
        fn_ranges.push((lo, fns.len()));
    }

    // --- 2. the population -------------------------------------------------
    let mut pass_names: BTreeSet<String> = BTreeSet::new();
    let mut syn_gated: BTreeSet<String> = BTreeSet::new();
    let mut where_defined: BTreeMap<String, String> = BTreeMap::new();
    for f in &fns {
        if !f.takes_registry {
            continue;
        }
        pass_names.insert(f.name.clone());
        where_defined
            .entry(f.name.clone())
            .or_insert_with(|| format!("{}:{}", files[f.file].rel, f.line));
        if f.syn_gated {
            syn_gated.insert(f.name.clone());
        }
    }

    // --- 3. call graph -----------------------------------------------------
    // Identifiers in CALL position only. `r.register(..)` is a method call on
    // the registry; counting it as a reference to the several
    // `fn register(r: &mut NativeMethodRegistry)` definitions in `jca/*` wires
    // every registrar in the tree to every jca pass, inflates the synthetic
    // closure by ~70 names, and moves whole families across the shipping line.
    let mut calls_from: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut non_pass_called: BTreeSet<String> = BTreeSet::new();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); fns.len()];
    for k in 0..fns.len() {
        if let Some(p) = fns[k].parent {
            children[p].push(k);
        }
    }
    let mut synthetic_overrides_body = 0usize;
    for k in 0..fns.len() {
        if fns[k].testish {
            continue;
        }
        if fns[k].name == SYNTHETIC_OVERRIDES {
            synthetic_overrides_body = synthetic_overrides_body.max(fns[k].end - fns[k].body);
        }
        // this fn's own text, minus its nested fn bodies
        let t = &files[fns[k].file].full;
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut pos = fns[k].body;
        let mut kids: Vec<usize> = children[k].clone();
        kids.sort_by_key(|&c| fns[c].start);
        for c in kids {
            if fns[c].start >= pos {
                segments.push((pos, fns[c].start));
            }
            pos = pos.max(fns[c].end);
        }
        segments.push((pos, fns[k].end.min(t.len())));

        let mut hits: BTreeSet<String> = BTreeSet::new();
        for (a, b) in segments {
            let mut i = a;
            while i < b {
                if !is_ident_start(t[i]) {
                    i += 1;
                    continue;
                }
                let s = i;
                while i < b && is_ident(t[i]) {
                    i += 1;
                }
                if s > 0 && (t[s - 1] == b'.' || is_ident(t[s - 1])) {
                    continue;
                }
                let j = skip_ws(t, i);
                if j >= t.len() || t[j] != b'(' {
                    continue;
                }
                // `fn name(` is a definition, not a call
                let mut back = s;
                while back > 0 && (t[back - 1] == b' ' || t[back - 1] == b'\n') {
                    back -= 1;
                }
                if back >= 2 && &t[back - 2..back] == b"fn" {
                    continue;
                }
                let id = String::from_utf8_lossy(&t[s..i]).into_owned();
                if pass_names.contains(&id) && id != fns[k].name {
                    hits.insert(id);
                }
            }
        }
        if fns[k].takes_registry {
            calls_from
                .entry(fns[k].name.clone())
                .or_default()
                .extend(hits);
        } else {
            if !fns[k].syn_gated {
                non_pass_called.extend(hits);
            }
        }
    }
    // Module-level references (a pass named in a `static` table, a `use`
    // re-export) count as non-pass references too.
    for (fi, f) in files.iter().enumerate() {
        let (lo, hi) = fn_ranges[fi];
        let mut tops: Vec<(usize, usize)> = Vec::new();
        for k in lo..hi {
            if fns[k].parent.is_none() {
                tops.push((fns[k].start, fns[k].end));
            }
        }
        tops.sort();
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut pos = 0usize;
        for (a, b) in tops {
            if a >= pos {
                segments.push((pos, a));
            }
            pos = pos.max(b);
        }
        segments.push((pos, f.full.len()));
        for (a, b) in segments {
            let t = &f.full;
            let mut i = a;
            while i < b {
                if !is_ident_start(t[i]) {
                    i += 1;
                    continue;
                }
                let s = i;
                while i < b && is_ident(t[i]) {
                    i += 1;
                }
                // Same `.`-exclusion as the in-fn scan: a method name is not a
                // reference to a free function of the same name.
                if s > 0 && t[s - 1] == b'.' {
                    continue;
                }
                let id = String::from_utf8_lossy(&t[s..i]).into_owned();
                if pass_names.contains(&id) {
                    non_pass_called.insert(id);
                }
            }
        }
    }

    // --- 4. reachability ---------------------------------------------------
    let reach = |seeds: &BTreeSet<String>, block: &BTreeSet<String>| -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = Vec::new();
        for s in seeds {
            if !block.contains(s) {
                seen.insert(s.clone());
                stack.push(s.clone());
            }
        }
        while let Some(n) = stack.pop() {
            if let Some(cs) = calls_from.get(&n) {
                for c in cs {
                    if block.contains(c) || seen.contains(c) {
                        continue;
                    }
                    seen.insert(c.clone());
                    stack.push(c.clone());
                }
            }
        }
        seen
    };

    let syn_seed: BTreeSet<String> = [SYNTHETIC_OVERRIDES.to_string()].into_iter().collect();
    let syn_reach = reach(&syn_seed, &BTreeSet::new());
    // `register_builtins` is ITSELF `#[cfg(feature = "synthetic-jdk")]`, is
    // `pub`, and is referenced from `vm/src/native/builtins.rs`. Taken as a
    // shipping root it reaches `register_synthetic_overrides` and drags every
    // synthetic-reachable name into the shipping set, and this gate reports a
    // clean, confident zero. That is not hypothetical — F34-1 §2.1 records it
    // happening.
    let mut roots: BTreeSet<String> = non_pass_called.clone();
    roots.remove(SYNTHETIC_OVERRIDES);
    for g in &syn_gated {
        roots.remove(g);
    }
    let shipping = reach(&roots, &syn_gated);
    let synthetic_only: BTreeSet<String> = syn_reach.difference(&shipping).cloned().collect();
    let direct_synthetic_only = calls_from
        .get(SYNTHETIC_OVERRIDES)
        .map(|d| d.iter().filter(|n| synthetic_only.contains(*n)).count())
        .unwrap_or(0);

    // --- 5. constants ------------------------------------------------------
    let mut per_file_consts: Vec<BTreeMap<String, String>> = Vec::with_capacity(files.len());
    let mut global: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let m = str_consts(f);
        for (k, v) in &m {
            match global.get(k) {
                Some(prev) if prev != v => {
                    ambiguous.insert(k.clone());
                }
                _ => {}
            }
            global.insert(k.clone(), v.clone());
        }
        per_file_consts.push(m);
    }

    // --- 6. registration sites --------------------------------------------
    let mut registrants: BTreeMap<Triple, BTreeSet<String>> = BTreeMap::new();
    let mut sites_of: BTreeMap<Triple, Vec<String>> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, usize> = BTreeMap::new();
    let mut register_sites = 0usize;
    let mut resolved_sites = 0usize;
    let mut loop_expanded_sites = 0usize;
    let bump = |m: &mut BTreeMap<String, usize>, k: &str| {
        *m.entry(k.to_string()).or_insert(0) += 1;
    };

    for (fi, f) in files.iter().enumerate() {
        let t = &f.full;
        let nc = &f.nc;
        let n = t.len();
        let (lo, hi) = fn_ranges[fi];
        let loops = parse_loops(f, &per_file_consts[fi], &global, &ambiguous);
        let mut let_cache: BTreeMap<usize, BTreeMap<String, Option<String>>> = BTreeMap::new();
        // Line numbers are only for failure messages, but counting newlines
        // from 0 at every one of ~14,000 sites is quadratic over ~80 MB of
        // source and turns a two-second test into a two-minute one.
        let mut line_starts: Vec<usize> = vec![0];
        for (bi, &b) in t.iter().enumerate() {
            if b == b'\n' {
                line_starts.push(bi + 1);
            }
        }

        // innermost-enclosing-fn sweep, in increasing site order
        let mut next_fn = lo;
        let mut stack: Vec<usize> = Vec::new();

        let mut i = 0usize;
        while i < n {
            if t[i] != b'.' {
                i += 1;
                continue;
            }
            let p = skip_ws(t, i + 1);
            if !(p < n && t[p..].starts_with(b"register")) {
                i += 1;
                continue;
            }
            let after = p + 8;
            let q = skip_ws(t, after);
            if q >= n || t[q] != b'(' {
                // `register_with_kind(`, `registered_by`, …
                i += 1;
                continue;
            }
            register_sites += 1;
            let op = q;
            let ce = match_paren(t, op);
            i = op + 1;
            if ce <= op + 1 {
                bump(&mut unresolved, "empty-arg-list");
                continue;
            }
            let args = split_args(&t[op + 1..ce - 1], &nc[op + 1..ce - 1]);
            if args.len() < 4 {
                // `thread_registry.register(..)`, `self.register(..)`,
                // `SubstitutionRegistry::register(a, b, c)` and friends: real
                // methods, wrong registry. Counted so the number is visible.
                bump(&mut unresolved, "arity<4");
                continue;
            }

            // advance the nesting stack to `op`
            while next_fn < hi && fns[next_fn].start <= op {
                while let Some(top) = stack.last().copied() {
                    if fns[top].end <= fns[next_fn].start {
                        stack.pop();
                    } else {
                        break;
                    }
                }
                stack.push(next_fn);
                next_fn += 1;
            }
            while let Some(top) = stack.last().copied() {
                if fns[top].end <= op {
                    stack.pop();
                } else {
                    break;
                }
            }
            let Some(encl) = stack.last().copied() else {
                bump(&mut unresolved, "no-enclosing-fn");
                continue;
            };
            if fns[encl].testish {
                continue;
            }
            let mut owner = Some(encl);
            while let Some(o) = owner {
                if fns[o].takes_registry {
                    break;
                }
                owner = fns[o].parent;
            }
            let Some(owner) = owner else {
                bump(&mut unresolved, "no-registry-owner");
                continue;
            };
            let owner_name = fns[owner].name.clone();

            let lets = let_cache.entry(encl).or_insert_with(|| {
                let_bindings(
                    f,
                    fns[encl].body,
                    fns[encl].end,
                    &per_file_consts[fi],
                    &global,
                    &ambiguous,
                )
            });

            // enclosing `for` loops
            let mut envs: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
            let mut unparsed_loop = false;
            for l in loops.iter() {
                if !(l.body <= op && op < l.end) {
                    continue;
                }
                match &l.values {
                    None => unparsed_loop = true,
                    Some(vals) => {
                        let mut next: Vec<BTreeMap<String, String>> = Vec::new();
                        for e in &envs {
                            for v in vals {
                                let mut d = e.clone();
                                d.insert(l.var.clone(), v.clone());
                                next.push(d);
                            }
                        }
                        if next.len() <= 512 {
                            envs = next;
                        }
                    }
                }
            }
            if envs.len() > 1 {
                loop_expanded_sites += 1;
            }

            let mut any = false;
            let mut why = if unparsed_loop {
                "for-loop-unparsed".to_string()
            } else {
                "unresolved-argument".to_string()
            };
            for env in &envs {
                let mut vals: Vec<String> = Vec::with_capacity(3);
                for (af, an) in args.iter().take(3) {
                    let (sf, sn) = strip_adaptors(af, an);
                    if let Some(v) = as_literal(&sf, &sn) {
                        vals.push(v);
                        continue;
                    }
                    if sf.contains("format!") {
                        why = "format!".to_string();
                        break;
                    }
                    let key: Option<&str> = if is_plain_ident(&sf) {
                        Some(sf.as_str())
                    } else {
                        path_tail(&sf)
                    };
                    let Some(key) = key else {
                        why = "expression".to_string();
                        break;
                    };
                    if let Some(v) = env.get(key) {
                        vals.push(v.clone());
                    } else if let Some(Some(v)) = lets.get(key) {
                        vals.push(v.clone());
                    } else if let Some(v) = per_file_consts[fi].get(key) {
                        vals.push(v.clone());
                    } else if !ambiguous.contains(key) {
                        match global.get(key) {
                            Some(v) => vals.push(v.clone()),
                            None => {
                                why = "unbound-identifier".to_string();
                                break;
                            }
                        }
                    } else {
                        why = "ambiguous-const".to_string();
                        break;
                    }
                }
                if vals.len() != 3 {
                    continue;
                }
                any = true;
                let key: Triple = (vals[0].clone(), vals[1].clone(), vals[2].clone());
                let line = match line_starts.binary_search(&op) {
                    Ok(k) => k + 1,
                    Err(k) => k,
                };
                registrants
                    .entry(key.clone())
                    .or_default()
                    .insert(owner_name.clone());
                let sites = sites_of.entry(key).or_default();
                if sites.len() < 8 {
                    sites.push(format!("{} @ {}:{}", owner_name, f.rel, line));
                }
            }
            if any {
                resolved_sites += 1;
            } else {
                bump(&mut unresolved, &why);
            }
        }
    }

    // --- 7. drift ----------------------------------------------------------
    let mut drift: BTreeMap<Triple, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    let mut per_pass: BTreeMap<String, usize> = BTreeMap::new();
    for (tri, regs) in &registrants {
        let so: BTreeSet<String> = regs.intersection(&synthetic_only).cloned().collect();
        let sh: BTreeSet<String> = regs.intersection(&shipping).cloned().collect();
        if so.is_empty() || sh.is_empty() {
            continue;
        }
        for p in &so {
            *per_pass.entry(p.clone()).or_insert(0) += 1;
        }
        drift.insert(tri.clone(), (so, sh));
    }

    Analysis {
        files: files.len(),
        fn_defs: fns.len(),
        passes: pass_names.len(),
        register_sites,
        resolved_sites,
        loop_expanded_sites,
        unresolved,
        triples: registrants.len(),
        shipping: shipping.len(),
        synthetic_only,
        direct_synthetic_only,
        synthetic_overrides_body,
        brace_imbalanced_files: brace_imbalanced,
        registrants,
        drift,
        per_pass,
        where_defined,
        sites_of,
    }
}

fn triple(t: (&str, &str, &str)) -> Triple {
    (t.0.to_string(), t.1.to_string(), t.2.to_string())
}

fn show(a: &Analysis, t: &Triple) -> String {
    let regs = a
        .registrants
        .get(t)
        .map(|s| s.iter().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_else(|| "<not registered anywhere this scan can see>".to_string());
    let sites = a
        .sites_of
        .get(t)
        .map(|v| v.join("\n            "))
        .unwrap_or_else(|| "<no site>".to_string());
    format!(
        "{}.{}{}\n        registered by: {regs}\n        sites:\n            {sites}",
        t.0, t.1, t.2
    )
}

// ===========================================================================
// SECTION 4 — the assertions
// ===========================================================================

/// The scanner must be measuring something, and must discriminate.
///
/// # What this catches
///
/// * the directory walk finding nothing (`MIN_FILES`);
/// * the `fn` parser breaking (`MIN_FN_DEFS`);
/// * `NativeMethodRegistry` renamed, so the population empties
///   (`MIN_PASSES`) — F34-1 §M4a's mutation;
/// * `register_synthetic_overrides` ceasing to be findable, which leaves
///   `families`-style set checks green and is caught here ONLY by the body-size
///   floor — F34-1 §M4b's mutation, reproduced deliberately;
/// * the descriptor resolver silently degrading (`MIN_RESOLVED_SITES`,
///   `MIN_TRIPLES`), which would otherwise report a small tidy fictional drift
///   number;
/// * loop expansion regressing (`MIN_LOOP_EXPANDED_SITES`);
/// * the shipping closure collapsing, which makes everything look
///   synthetic-only (`MIN_SHIPPING`);
/// * a literal running away and eating code (`brace_imbalanced_files`) — the
///   failure mode that produced this file's first, wrong, census.
#[test]
fn the_drift_scanner_is_not_vacuous() {
    let a = analysis();

    println!(
        "registrar-drift: {} files, {} fn defs, {} passes, {} register sites \
         ({} resolved, {} loop-expanded), {} distinct triples, {} shipping-reachable, \
         {} synthetic-only ({} direct), {} DRIFTING triples",
        a.files,
        a.fn_defs,
        a.passes,
        a.register_sites,
        a.resolved_sites,
        a.loop_expanded_sites,
        a.triples,
        a.shipping,
        a.synthetic_only.len(),
        a.direct_synthetic_only,
        a.drift.len(),
    );
    println!("registrar-drift: unresolved by reason: {:?}", a.unresolved);

    assert!(
        a.brace_imbalanced_files.is_empty(),
        "after blanking, these files' braces do not balance: {:?}\n\
         That means a string or char literal was not terminated where the scanner thought, so \
         it blanked real code — including `{{` and `}}` — until the next delimiter. Every fn \
         span, every call-graph edge and every triple in such a file is unreliable. Fix the \
         blanker; do NOT relax this assertion.",
        a.brace_imbalanced_files
    );
    assert!(
        a.files >= MIN_FILES,
        "found only {} .rs files across {CRATES:?} (floor {MIN_FILES}); the directory walk is \
         broken and every count below it is meaningless",
        a.files
    );
    assert!(
        a.fn_defs >= MIN_FN_DEFS,
        "parsed only {} fn definitions (floor {MIN_FN_DEFS}); the header parser is broken",
        a.fn_defs
    );
    assert!(
        a.passes >= MIN_PASSES,
        "found only {} registration passes (floor {MIN_PASSES}); the population is keyed on a \
         signature mentioning `{KEY_TYPE}` — if that type was renamed, re-point KEY_TYPE rather \
         than lowering this floor",
        a.passes
    );
    assert!(
        a.synthetic_overrides_body >= MIN_SYNTHETIC_OVERRIDES_BODY,
        "`{SYNTHETIC_OVERRIDES}`'s body extracted as {} bytes (floor \
         {MIN_SYNTHETIC_OVERRIDES_BODY}); the locator or the brace scan is broken, so the \
         synthetic closure is a fragment and every drift verdict below is understated. This is \
         the ONLY check that noticed F34-1's M4b mutation.",
        a.synthetic_overrides_body
    );
    assert!(
        a.register_sites >= MIN_REGISTER_SITES,
        "found only {} `.register(` sites (floor {MIN_REGISTER_SITES})",
        a.register_sites
    );
    assert!(
        a.resolved_sites >= MIN_RESOLVED_SITES,
        "resolved only {} of {} `.register(` sites (floor {MIN_RESOLVED_SITES}); the \
         literal/const resolver has degraded, and a census that cannot read descriptors reports \
         a small, clean, entirely fictional drift number. Unresolved by reason: {:?}",
        a.resolved_sites,
        a.register_sites,
        a.unresolved
    );
    assert!(
        a.triples >= MIN_TRIPLES,
        "recovered only {} distinct (class, name, descriptor) triples (floor {MIN_TRIPLES})",
        a.triples
    );
    assert!(
        a.loop_expanded_sites >= MIN_LOOP_EXPANDED_SITES,
        "only {} register sites expanded through a `for x in [..]` loop (floor \
         {MIN_LOOP_EXPANDED_SITES}); F34-1 warned that loop-emitted rows are invisible to a \
         site-counting scan, and without expansion whole classes vanish with no other symptom",
        a.loop_expanded_sites
    );
    assert!(
        a.shipping >= MIN_SHIPPING,
        "only {} passes are shipping-reachable (floor {MIN_SHIPPING}); the shipping closure \
         collapsed, which makes everything look synthetic-only and INFLATES drift",
        a.shipping
    );
    assert!(
        a.synthetic_only.len() >= MIN_SYNTHETIC_ONLY,
        "only {} synthetic-only passes (floor {MIN_SYNTHETIC_ONLY}); if the synthetic closure \
         collapsed, drift reads zero and this gate is decoration. F34-1 §2.1 records exactly \
         this happening when `register_builtins` was taken as a shipping root.",
        a.synthetic_only.len()
    );
    assert!(
        a.direct_synthetic_only >= MIN_DIRECT_SYNTHETIC_ONLY,
        "only {} synthetic-only DIRECT children of `{SYNTHETIC_OVERRIDES}` (floor \
         {MIN_DIRECT_SYNTHETIC_ONLY}); `registrar_reachability.rs` allow-lists 73",
        a.direct_synthetic_only
    );
    assert!(
        a.drift.len() >= MIN_TOTAL_DRIFT,
        "only {} drifting triples (floor {MIN_TOTAL_DRIFT}). A gate that measures nothing \
         passes loudly: this is the check that says the scan still finds the 1,266 rows that \
         were there on 2026-08-16. If drift really was reduced below the floor, that is very \
         good news and the floor is what to re-take.",
        a.drift.len()
    );

    // --- the two-sided control -------------------------------------------
    let pos = triple(CONTROL_POSITIVE);
    let neg = triple(CONTROL_NEGATIVE);
    assert!(
        a.drift.contains_key(&pos),
        "POSITIVE CONTROL FAILED: `{}.{}{}` is not reported as drifting.\n\
         Either the twin was collapsed onto one implementation — in which case say so, remove \
         its MUST_DRIFT row and re-point this control at another known instance — or the \
         scanner has stopped discriminating and every other assertion in this file is \
         worthless.\n    {}",
        pos.0,
        pos.1,
        pos.2,
        show(a, &pos)
    );
    assert!(
        !a.drift.contains_key(&neg),
        "NEGATIVE CONTROL FAILED: `{}.{}{}` is reported as drifting.\n\
         This is F34-1's worked example AFTER its fix: F16 deleted the whole group-layout \
         family from `panama.rs::register_pe2_struct_layouts` so \
         `phases_late/foreign_ffm.rs::register_p67_foreign_memory` is the sole registrant. If \
         this fires, a synthetic-only twin came back, and the two files carry two different \
         layout objects behind one descriptor — read the comment at `panama.rs:5829` before \
         doing anything else.\n    {}",
        neg.0,
        neg.1,
        neg.2,
        show(a, &neg)
    );
    assert!(
        a.registrants.contains_key(&neg),
        "NEGATIVE CONTROL IS VACUOUS: `{}.{}{}` is not registered by ANY pass this scan can \
         see. The control passes for the wrong reason — the resolver lost the triple rather \
         than the drift being absent. Repair the resolver before trusting any number here.",
        neg.0,
        neg.1,
        neg.2
    );

    for &(c, n, d, how) in RESOLVER_WITNESSES {
        let t = triple((c, n, d));
        assert!(
            a.registrants.contains_key(&t),
            "RESOLVER WITNESS MISSING: `{c}.{n}{d}` was not recovered. It is recovered through \
             {how}; if it is gone, that resolution path has broken silently and the drift \
             number is understated by an unknown amount."
        );
    }
}

/// The baseline must stay a table of measured debts, not a wish list.
#[test]
fn the_baseline_is_well_formed() {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut sum = 0usize;
    for &(name, count) in DRIFT_BASELINE {
        assert!(
            seen.insert(name),
            "`{name}` appears twice in DRIFT_BASELINE"
        );
        assert!(
            count > 0,
            "`{name}` has a baseline of 0. A zero row is indistinguishable from absence and \
             gives the false impression that the pass was examined; delete the row instead."
        );
        sum += count;
    }
    // A row per pass, summed, exceeds the distinct total because one triple can
    // be registered by several synthetic-only passes. If the sum ever drops
    // BELOW the distinct total the table has been edited into nonsense.
    assert!(
        sum >= BASELINE_TOTAL_DRIFT,
        "DRIFT_BASELINE sums to {sum} but BASELINE_TOTAL_DRIFT is {BASELINE_TOTAL_DRIFT}. The \
         per-pass rows cannot account for fewer triples than the distinct total, because a \
         triple with several synthetic-only registrants is counted once per pass. One of the \
         two numbers was edited without re-taking the other."
    );
    assert!(
        BASELINE_TOTAL_DRIFT > MIN_TOTAL_DRIFT,
        "MIN_TOTAL_DRIFT ({MIN_TOTAL_DRIFT}) must sit BELOW BASELINE_TOTAL_DRIFT \
         ({BASELINE_TOTAL_DRIFT}); a floor at or above the ceiling can never both hold"
    );

    let a = analysis();
    // A baseline row naming a pass that is no longer synthetic-only is a stale
    // exemption, and a stale exemption is a hole: the ceiling would then be
    // enforced against a pass that can never drift, quietly covering for a
    // different one.
    let stale: Vec<&str> = DRIFT_BASELINE
        .iter()
        .map(|&(n, _)| n)
        .filter(|n| !a.synthetic_only.contains(*n))
        .collect();
    assert!(
        stale.is_empty(),
        "these DRIFT_BASELINE rows name passes that are NOT synthetic-only any more: {stale:?}\n\
         Good news if they were promoted onto the shipping path — delete the rows and re-take \
         BASELINE_TOTAL_DRIFT. Bad news if the reachability scan broke, in which case fix that \
         first: `registrar_reachability.rs` is the gate that owns that question."
    );
}

/// The ratchet. **New drift fails; pre-existing drift does not.**
///
/// # What this catches
///
/// * a NEW triple registered by both a synthetic-only pass and a shipping pass —
///   the species this file exists for;
/// * a synthetic-only pass that starts drifting at all (its ceiling is zero
///   unless it is in the table);
/// * an existing family drifting MORE than it did;
/// * a family whose drift moves to another family, because the ceiling is
///   per-pass and not just global.
///
/// # What this provably does NOT catch
///
/// * **A triple whose drift is swapped for another triple within the same
///   pass at the same count.** The ceiling is a count, not a set. Pinning the
///   1,266 triples exactly would catch it, and would also make this file fail
///   on any harmless resolver improvement; the count was chosen deliberately,
///   and the choice is a weakening.
/// * **Anything the resolver cannot see** — `format!` descriptors,
///   class-parameterised registrars, tuple `for` loops, array-const iteration.
///   New drift arriving through one of those is invisible here. The floors in
///   [`the_drift_scanner_is_not_vacuous`] guard against the resolver getting
///   WORSE, not against its existing blind spots.
/// * **Kind drift.** Two registrations of one triple with the same body but
///   different `NativeKind` behave differently under `--jdk-only`, because
///   `SyntheticStub` is refused there. This gate cannot see kinds at all.
/// * **Which body actually wins.** That is registration ORDER at runtime, and
///   only `--dump-native-registry` answers it (`owns_slot`).
#[test]
fn no_new_mode_drift() {
    let a = analysis();
    let base: BTreeMap<&str, usize> = DRIFT_BASELINE.iter().copied().collect();

    let mut over: Vec<String> = Vec::new();
    for (pass, count) in &a.per_pass {
        let allowed = base.get(pass.as_str()).copied().unwrap_or(0);
        if *count > allowed {
            let examples: Vec<String> = a
                .drift
                .iter()
                .filter(|(_, (so, _))| so.contains(pass))
                .take(4)
                .map(|(t, (so, sh))| {
                    format!(
                        "{}.{}{}  synthetic-only: {:?}  shipping: {:?}",
                        t.0,
                        t.1,
                        t.2,
                        so.iter().collect::<Vec<_>>(),
                        sh.iter().collect::<Vec<_>>()
                    )
                })
                .collect();
            over.push(format!(
                "  {pass} (defined at {})\n      drift {count}, baseline {allowed}\n      e.g. {}",
                a.where_defined
                    .get(pass)
                    .cloned()
                    .unwrap_or_else(|| "<unknown>".to_string()),
                examples.join("\n           ")
            ));
        }
    }

    assert!(
        over.is_empty(),
        "NEW MODE DRIFT.\n\n{}\n\n\
         Each line is a `(class, name, descriptor)` newly registered by BOTH a synthetic-only \
         pass and a shipping pass. `register()` is last-write-wins and \
         `register_synthetic_overrides` runs last, so synthetic-JDK mode gets one body and the \
         shipping modes get the other — and every test built with `--features synthetic-jdk` \
         measures the copy that does not ship.\n\n\
         Three questions, in order:\n\
         1. Do the two bodies agree? Read both. If they are the same free function under two \
            names, say so in the record; if they differ, the shipping one is the one nobody \
            tested.\n\
         2. If they differ, which should survive? Deleting the synthetic-only copy is NOT \
            automatically right — F34-1 §5 records that dropping `register_pe_panama` would \
            have taken 14 triples with it that its twin does not register.\n\
         3. If both must stay, add the row to DRIFT_BASELINE with the measurement behind it, \
            and add a MUST_DRIFT entry if you read the two bodies and they differ.\n\n\
         Do NOT raise the baseline without doing (1).",
        over.join("\n")
    );

    assert!(
        a.drift.len() <= BASELINE_TOTAL_DRIFT,
        "total drift is {} against a baseline ceiling of {BASELINE_TOTAL_DRIFT}, but no single \
         pass exceeded its own row. That means drift appeared under a pass the per-pass sweep \
         attributed elsewhere, or the baseline table and the total were taken at different \
         times. Re-take both together.",
        a.drift.len()
    );
}

/// The rows whose two implementations were READ and found to differ.
///
/// A count ratchet cannot tell "these two bodies disagree" from "these two
/// names point at one function". These four were diffed by hand, so they are
/// pinned individually: if one stops drifting, either it was fixed — say so and
/// delete the row — or the scanner stopped seeing it, which is worse.
#[test]
fn the_known_live_twins_still_drift() {
    let a = analysis();
    let mut missing: Vec<String> = Vec::new();
    for &(c, n, d, why) in MUST_DRIFT {
        let t = triple((c, n, d));
        if !a.drift.contains_key(&t) {
            missing.push(format!(
                "  {c}.{n}{d}\n      why it mattered: {why}\n    {}",
                show(a, &t)
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "these triples were measured as LIVE mode drift (two implementations that behave \
         differently) and are no longer reported as drifting:\n{}\n\n\
         If the twin was genuinely collapsed onto one implementation, delete the MUST_DRIFT row \
         and re-take DRIFT_BASELINE in the same commit. If it was not, this scanner has stopped \
         resolving something and the whole number is understated.",
        missing.join("\n")
    );
}
