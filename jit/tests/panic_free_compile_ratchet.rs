// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: the JIT may not grow new panicking constructs in non-test code.
//!
//! # The contract this defends
//!
//! `jit/src/aarch64.rs` states it at `Reg::from_u8` as contract **C11**: the
//! compiler never panics in production, it bails to the interpreter instead.
//! Every refusal path in this crate is built on that — `Bailout`,
//! `CompileResult`, `ir_verify`'s "never panics, every index goes through
//! `get`" clause, `compile_gate`'s admission answers.
//!
//! # There is a net, and it is not a licence
//!
//! Be accurate about this, because the overstated version of it invites the
//! reply "so what". A compile panic is **contained** today:
//! `jit::tiered::contain_compile_panic` wraps the compile call in
//! `CompilerCore::compiler_loop` (`tiered.rs:3374`) and at the mutator-thread
//! compile door (`vm/src/runtime/interpreter/jit_bridge.rs:7725`), and
//! `vm/src/jit/helper_guard.rs`'s `contain` wraps every `extern "C"` JIT helper
//! that compiled code can call. A panic in codegen is not a dead VM. It was,
//! before that work.
//!
//! What it still costs, and why this ratchet exists anyway:
//!
//! 1. **A contained compile panic is a permanent decline, not a retry.**
//!    `compiler_loop` turns the `Err` into `CompileOutcome::declined(0)` and
//!    `note_worker_panic` records it, so the method is never optimized again
//!    for the life of the process. A bailout costs one compile attempt and
//!    leaves the method eligible; a panic costs it forever. The two are not
//!    the same outcome and the JIT's own docs do not treat them as one.
//! 2. **The net is paid for elsewhere.** `COMPILE_PANIC_SCOPES` and
//!    `compile_panic_is_contained` exist only so the VM's crash handler can
//!    tell a contained compile panic from a real crash and not spend the
//!    one-`hs_err_pid<pid>.log`-per-process budget on it — see the comment on
//!    that thread-local. Every panic this crate can raise is a case that
//!    machinery has to keep getting right.
//! 3. **The net is installed by the VM crate, not by this one.** Every
//!    `contain_compile_panic` call site is in `vm/`, plus `tiered`'s own
//!    worker loop. A new entry point into `cratonvm-jit` that the VM calls
//!    directly is unprotected until somebody remembers to wrap it, and nothing
//!    in the build fails if they do not.
//! 4. **All of it rests on `panic = "unwind"`,** pinned in the workspace
//!    `Cargo.toml` with a comment saying it must stay because the VM has 37
//!    `catch_unwind` sites. That is a note in a manifest, not a check.
//!
//! So the net makes a panic *survivable*, not *correct*. There are tests for
//! individual bailouts and tests for individual encoders; there was no test
//! saying "the number of things that net has to catch may only go down". This
//! is that test.
//!
//! # What counts
//!
//! Occurrences — not lines, a line with two counts two — of any of
//!
//! ```text
//! .unwrap()   .expect(   panic!   unreachable!   todo!   unimplemented!
//! ```
//!
//! in the **non-test** part of each `.rs` file under `jit/src`.
//!
//! "Non-test" is defined mechanically, and the definition is the one the
//! baseline below was enumerated with, so the numbers and the rule agree:
//!
//! 1. a file whose name is `tests.rs` is skipped entirely;
//! 2. in every other file, the **item gated by a configuration attribute
//!    that selects the test configuration** is skipped — the bare attribute,
//!    and also a compound one such as
//!    `cfg(all(test, target_arch = "x86_64"))`, which is how a test module
//!    that must ALSO be gated on the architecture is written. A negated
//!    predicate (`cfg(not(test))`) is explicitly NOT one of these: it marks
//!    production code. See [`opens_test_code`]. The gated item runs from the
//!    attribute to the `}` that closes its body, or to its `;`/`,` when it
//!    has none (see [`GatedItem`]); code AFTER it is scanned again. Until
//!    round 11 wave 3 the scan stopped at the first gate in the file, which
//!    left most of `ir_lower.rs` and `lib.rs` unread behind one early test
//!    helper;
//! 3. a line whose first non-blank text is `//` is skipped (this blanks
//!    `//`, `///` and `//!` comments alike).
//!
//! Each of the three is a deliberate approximation with a known leak, and the
//! leaks are named here rather than papered over:
//!
//! * Rule 2 sees only a test-configuration attribute **inside** the scanned
//!   file. Two files —
//!   `x64/flag_and_header_contracts.rs` and `x64/loop_unroll_admission.rs` —
//!   are whole-file test modules whose gate is at the `mod` declaration in
//!   `jit/src/x64.rs` (`#[cfg(test)] mod flag_and_header_contracts;` at
//!   `x64.rs:3592`, `#[cfg(test)] mod loop_unroll_admission;` at `x64.rs:3612`).
//!   This rule cannot see that, so their test assertions are counted. They are
//!   frozen like everything else and are labelled as what they are in
//!   [`FROZEN`]. See "If you are adding a test" below.
//! * Rule 3 blanks only *whole-line* comments, the same rule
//!   `jit/tests/no_presence_only_flag_reads.rs` uses. A trailing
//!   `// ... .unwrap() ...` on a code line is therefore counted. That is the
//!   safe direction: it asks for the comment to move onto its own line rather
//!   than letting a real panic hide behind one. Four such prose mentions
//!   already sit on their own lines (`ir_optimize.rs`, `x64/deopt_stubs.rs`,
//!   `x64/osr.rs` ×2) and are correctly not counted.
//! * A panic reached through an alias — `Option::unwrap_or_else(|| panic!(..))`
//!   is counted, but `.expect_err(`, a helper named `must()`, indexing
//!   (`v[i]`), integer division, or an explicit `std::process::abort()` are
//!   not. This is a textual ratchet, not a proof. It stops the *ordinary* way
//!   panics arrive, which is someone writing `.unwrap()` because the `Result`
//!   was inconvenient.
//!
//! # The baseline, and why it is per file
//!
//! [`FROZEN`] is a per-file table, not one grand total. A single number lets a
//! new `.unwrap()` in `ir.rs` hide behind a removed one in `lib.rs`: the total
//! is unchanged and the ratchet says nothing, which is exactly the accounting
//! error a ratchet exists to prevent. Per file, the two show up as one row up
//! and one row down and both have to be explained.
//!
//! The table is asserted in **both directions**. A file that gains an
//! occurrence fails; a file that loses one also fails, until its number is
//! lowered — so the ground the crate gains cannot be given back silently.
//!
//! # If you are adding a test
//!
//! …to one of the two whole-file test modules named above, this ratchet will
//! fail on an `.expect(` in an assertion. That is a false positive produced by
//! rule 2, and the honest fix is not to raise the number: it is for the owner
//! of `jit/src/x64.rs` to move those two modules under a path this rule can
//! see — either an in-file `#[cfg(test)] mod` wrapper, or `jit/src/x64/tests/`
//! alongside the existing `x64/tests.rs`, which rule 1 skips by name. Until
//! that happens, raising the two rows with a one-line note saying which test
//! added the assertion is an acceptable stopgap, and only for those two rows.
//!
//! The needles are assembled at runtime. This file lives under `jit/tests/`,
//! outside the scanned tree, but a literal needle here would be one copy-paste
//! away from a future version of this test counting itself.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Panicking constructs in the non-test part of each `jit/src` file, frozen on
/// 2026-09-16. **Lower a number when a panic is removed; do not raise one.**
///
/// Every site was read before it was frozen. They fall into seven groups, and
/// only one of them is a panic that could actually run on a compile worker.
///
/// **Reachable on the production codegen path — the rows that matter.**
///
/// * `x64/arith.rs` (1) — `emit_arith_hoist_into_rax`, `_ =>
///   unreachable!("non-hoistable binop reached emit")`. The LICM planner that
///   builds the `ArithStep::BinOp` list is the filter; the emitter trusts it.
///   Planner and emitter are different functions in different files, so this
///   is a genuine cross-function assumption enforced by a panic. **This is the
///   one on the list that most deserves to become a bail.**
/// * `x64/operand_stack.rs` (1) — `push_from_rax`'s `unreachable!("push_stack
///   always returns Frame")`, a narrowing panic (next group). The two
///   `canonicalize_stack` `.expect`s in the parallel-move cycle breaker became
///   named bails in round 9 (2026-09-18).
///
/// **Narrowing panics the compiler cannot prove away (5).** A `match` arm
/// whose scrutinee a preceding pattern already restricted, where Rust still
/// demands a `_` arm: `ir.rs` ×2 (`build`, the `0x99..=0x9e` and
/// `0x9f..=0xa4` compare-opcode arms), `ir_optimize.rs` ×1 (`try_fold`),
/// `escape_analysis.rs` ×1 (`lock_regions`, `_ => unreachable!("filtered
/// above")`), `x64/operand_stack.rs` ×1 (`push_from_rax`). These are
/// unreachable in the ordinary sense and cost nothing to leave; they are
/// counted because the scanner cannot tell them from the group above, and
/// because "this arm is unreachable" is a claim that decays when the match
/// above it is edited.
///
/// **A backend that has never executed (2).** Both are in
/// `aarch64_backend.rs`, in `emit_prologue`: `.expect("frame present")` ×2.
/// `docs/jit/aarch64-parity.md` opens with "Nothing described here has been
/// executed on AArch64 hardware" — no AArch64 binary has been built, booted or
/// run — and the backend refuses every object-model opcode, so it compiles
/// almost nothing even if it were. These are frozen, not excused: the day that
/// backend runs is the day two `.expect`s become two methods permanently
/// declined by a contained panic, on the architecture with the least test
/// coverage in the tree.
///
/// This row was 5, then 3. `compile_pass`'s `.expect("just set")` went with
/// the frame-layout rework: the spill area is described once by
/// `Arm64SpillArea` and installed through `install_frame`, so the "we just
/// set it, so it is there" step it was asserting no longer exists.
///
/// Before that, the `r` / `fp` register converters
/// (`.expect("regalloc invariant: …")`) are gone: they now record into a
/// sticky per-compile flag that `emit_machine_code` reads and refuses the
/// body on, which is the shape every other refusal in that file already had.
/// The FP one had already been a live panic once, when `D8`-`D15` arrived as
/// `Arm64Register(40..=47)`.
///
/// **A `const`-evaluation mechanism (1).** `x64/disp.rs` `disp8_const`'s
/// `panic!`. Const evaluation has no `Result`, so a `panic!` in a `const fn` is
/// the only way to reject an out-of-range value at build time. The function's
/// own doc says "call this only in a `const` context" and points runtime
/// callers at `Disp::encode`, which returns `Err`. It carries
/// `#[allow(clippy::panic)]` for the same reason.
///
/// **Start-up probes with no production caller (3).** `lib.rs`
/// `probe_object_ptr_offset` (`.expect("cannot locate Object pointer in
/// Value")`) and `probe_object_null_template` (two `.try_into().unwrap()` on
/// fixed 8-byte sub-slices of a 16-byte array). Both are `pub fn` in non-test
/// code whose only callers in the whole repository are `lib.rs`'s own test
/// module; the `try_into`s cannot fail by construction.
///
/// **Miscounted test modules (51).** `x64/loop_unroll_admission.rs` (37) and
/// `x64/flag_and_header_contracts.rs` (14) are whole-file `#[cfg(test)]`
/// modules declared in `x64.rs`; every occurrence is an assertion in a test
/// (`.expect("admitted")`, `.expect("test buffer")`, …). The "first
/// `#[cfg(test)]` in the file" rule cannot see a gate that lives in another
/// file. See the module doc's "If you are adding a test".
///
/// The second row was frozen at **9** until round 10, and that number was an
/// artefact of a bug in this file's own scanner rather than a count of anything:
/// `non_test_code` tested the boundary BEFORE dropping comment lines, so the
/// comment at `flag_and_header_contracts.rs:710` — which mentions the gate in
/// prose, and even says "frozen at the older tests' total" — cut the scan short
/// and hid the five assertions below it. With the ordering fixed the file reads
/// 14. Raising this row admits no new production panic: every one of the 14 is a
/// test assertion in a file that is entirely test code.
/// **A `const fn` that fails the BUILD, not the process (1).**
/// `direct_helpers.rs` `direct_helper_sig_of`: `match direct_helper_sig(field)
/// { Some(sig) => sig, None => panic!(..) }`. It exists to be called from a
/// `const` context by `assert_direct_helper_call_shape!`, which is the
/// compile-time check that a hand-written direct-helper call site agrees with
/// its slot's declared signature — the hazard that table was added to close.
/// A `const fn` cannot return a `Bailout`, and there is no interpreter to fall
/// back to at compile time: the panic IS the diagnostic, and it fires at
/// `cargo build` for a misspelled field name or a slot added without a
/// signature alias. Its `jit-api` twin `helper_sig_of` has the same shape.
/// Reachable at runtime only if someone calls it outside a `const` context,
/// which nothing does.
///
/// **Invariant assertions the first-gate rule never read (3), frozen
/// 2026-09-24.** Round 11 wave 3 made rule 2 skip only the gated item, and
/// `lib.rs`'s first gate is a test module at ~1672, so these three below it
/// were counted for the first time. None is on a compile path:
/// `CompiledMethod::call_with_heap`'s `.expect` is the documented test-side
/// alias of `try_call_with_context` (production callers use the `try_` form);
/// `lock_retire_queue`'s `panic!` is the lock-order assertion, which fires only
/// while `lock_order::enforcement_active()` and turns a use-after-free of
/// executable memory into a named failure; `RetainedCode::arc`'s `.expect` is
/// on an `Option` that is `None` only inside `Drop`, where `arc` is not called.
///
const FROZEN: &[(&str, usize)] = &[
    ("aarch64_backend.rs", 2),
    ("direct_helpers.rs", 1),
    ("escape_analysis.rs", 1),
    ("ir.rs", 2),
    ("ir_optimize.rs", 1),
    ("lib.rs", 6),
    ("x64/arith.rs", 1),
    ("x64/disp.rs", 1),
    ("x64/flag_and_header_contracts.rs", 14),
    ("x64/loop_unroll_admission.rs", 37),
    ("x64/operand_stack.rs", 1),
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The six needles, assembled so this file cannot count itself.
///
/// `.unwrap()` is spelled with its closing parenthesis on purpose, so
/// `.unwrap_or(`, `.unwrap_or_else(` and `.unwrap_or_default()` — all of which
/// are the *fix*, not the defect — do not match. `.expect(` likewise does not
/// match `.expect_err(`.
fn needles() -> Vec<String> {
    vec![
        format!(".{}()", ["un", "wrap"].concat()),
        format!(".{}(", ["ex", "pect"].concat()),
        format!("{}!", ["pan", "ic"].concat()),
        format!("{}!", ["unreach", "able"].concat()),
        format!("{}!", ["to", "do"].concat()),
        format!("{}!", ["unimple", "mented"].concat()),
    ]
}

/// The lines of `text` that the rule in the module doc calls non-test code:
/// everything before the first `#[cfg(test)]`, with whole-line `//` comments
/// dropped.
///
/// `gate` is the test-configuration attribute, assembled by the caller rather
/// than written here. That keeps the scanner's own *code* free of the tokens it
/// hunts for, the same discipline `needles` follows. It is not a complete
/// defence — the prose above and in the module doc does spell the attribute out
/// — which is another reason this file has to stay under `jit/tests/`, outside
/// the tree it scans.
fn non_test_code(text: &str, gate: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // `Some` while inside a gated item: the lexical state that finds its end.
    let mut gated: Option<GatedItem> = None;
    for line in &logical_lines(text) {
        let line = line.as_str();
        // Comments are dropped BEFORE the boundary test, not after it. A `///`
        // doc comment that merely MENTIONS the attribute in prose is not a gate,
        // and treating it as one truncates the production section at the comment
        // — silently reclassifying every real panic site below it as test code.
        // Round 10 hit exactly that: a doc comment added to `jit/src/ir.rs`
        // explaining that three methods have no caller outside test code spelled
        // the attribute out, and this scanner then reported `ir.rs: 5 -> 0 (all
        // removed)`. Nothing had been removed. The failure message invites you to
        // lower the frozen row, which would have discarded five real sites and
        // left the file unable to regain them.
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        loop {
            if let Some(item) = gated.as_mut() {
                match item.feed(rest) {
                    Some(end) => {
                        gated = None;
                        rest = &rest[end..];
                    }
                    None => break,
                }
            }
            match test_gate_at(rest, gate) {
                Some(at) => {
                    out.push_str(&rest[..at]);
                    gated = Some(GatedItem::default());
                    rest = &rest[at..];
                }
                None => {
                    out.push_str(rest);
                    break;
                }
            }
        }
        out.push('\n');
    }
    out
}

/// The lines of `text`, except that a `cfg` attribute written across several
/// lines is joined onto one, so the predicate is seen whole.
/// `platform.rs`'s `near_globals_tests` is gated by a `cfg(all(` whose `test`
/// sits on the next line; read line by line that module is not a gate at all.
fn logical_lines(text: &str) -> Vec<String> {
    let cfg_open = format!("#[{}(", ["cf", "g"].concat());
    let mut out = Vec::new();
    let mut pending: Option<(String, i64)> = None;
    for line in text.lines() {
        if let Some((mut joined, depth)) = pending.take() {
            let depth = depth + bracket_balance(line);
            joined.push(' ');
            joined.push_str(line.trim());
            if depth > 0 {
                pending = Some((joined, depth));
            } else {
                out.push(joined);
            }
            continue;
        }
        let open = if line.trim_start().starts_with("//") {
            None
        } else {
            line.find(&cfg_open)
        };
        match open.map(|at| bracket_balance(&line[at..])) {
            Some(depth) if depth > 0 => pending = Some((line.to_string(), depth)),
            _ => out.push(line.to_string()),
        }
    }
    out.extend(pending.map(|(joined, _)| joined));
    out
}

/// `[`/`(` opened minus closed on `s` — enough for an attribute's predicate,
/// which holds no strings with brackets in them.
fn bracket_balance(s: &str) -> i64 {
    s.chars()
        .map(|c| match c {
            '[' | '(' => 1,
            ']' | ')' => -1,
            _ => 0,
        })
        .sum()
}

/// Where on `line` a test-configuration attribute starts, if it carries one
/// (see [`opens_test_code`] for which attributes count).
fn test_gate_at(line: &str, gate: &str) -> Option<usize> {
    if !opens_test_code(line, gate) {
        return None;
    }
    let cfg_open = format!("#[{}(", ["cf", "g"].concat());
    line.find(gate).or_else(|| line.find(&cfg_open))
}

/// The end of ONE gated item, found lexically.
///
/// Rule 2 used to stop the scan at the first gate in a file, which is right
/// only when that gate opens the trailing test module. Round 11 wave 3 found
/// files that gate a single test helper near the top (`ir_lower.rs` at ~255,
/// `lib.rs` at ~1672, `x64/licm.rs`, `x64.rs`) and keep tens of thousands of
/// production lines below it, all of them unscanned — with a real
/// `unreachable!` and three `.expect`s among them. Page
/// `r11w3-x64loop-panic-ratchet-blind-after-first-cfg-test-item`.
///
/// The item is fed from its attribute on. It ends at the `}` that closes its
/// first brace, or at a `;` or `,` outside every bracket before any brace
/// opens (`mod tests;`, `use ..;`, a gated field or match arm), or at a closer
/// that would take the depth below where it started (a gated last match arm
/// with no trailing comma). Braces inside strings, raw strings, char literals
/// and comments are not counted, so a test full of Java source in string
/// literals does not end early or run on.
#[derive(Default)]
struct GatedItem {
    depth: i64,
    opened_brace: bool,
    block_comment: u32,
    /// `Some(None)` in a plain string, `Some(Some(n))` in a raw string closed
    /// by a quote and `n` hashes.
    string: Option<Option<usize>>,
}

impl GatedItem {
    /// Consume `line`; the byte offset just past the item's end if it ends on
    /// this line.
    fn feed(&mut self, line: &str) -> Option<usize> {
        let cs: Vec<(usize, char)> = line.char_indices().collect();
        let ch = |k: usize| cs.get(k).map(|&(_, c)| c);
        let ident = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = |k: usize| cs.get(k + 1).map_or(line.len(), |&(b, _)| b);
        let mut i = 0;
        while i < cs.len() {
            let c = cs[i].1;
            if self.block_comment > 0 {
                if c == '*' && ch(i + 1) == Some('/') {
                    self.block_comment -= 1;
                    i += 2;
                } else if c == '/' && ch(i + 1) == Some('*') {
                    self.block_comment += 1;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            match self.string {
                Some(None) => {
                    if c == '\\' {
                        i += 2;
                        continue;
                    }
                    if c == '"' {
                        self.string = None;
                    }
                    i += 1;
                    continue;
                }
                Some(Some(n)) => {
                    if c == '"' && (1..=n).all(|k| ch(i + k) == Some('#')) {
                        self.string = None;
                        i += 1 + n;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                None => {}
            }
            match c {
                '/' if ch(i + 1) == Some('/') => return None,
                '/' if ch(i + 1) == Some('*') => {
                    self.block_comment += 1;
                    i += 2;
                    continue;
                }
                '"' => self.string = Some(None),
                'r' if {
                    let prev = if i > 0 { ch(i - 1) } else { None };
                    let prev2 = if i > 1 { ch(i - 2) } else { None };
                    let starts = !ident(prev) || (prev == Some('b') && !ident(prev2));
                    let mut k = i + 1;
                    while ch(k) == Some('#') {
                        k += 1;
                    }
                    starts && ch(k) == Some('"')
                } =>
                {
                    let mut n = 0;
                    while ch(i + 1 + n) == Some('#') {
                        n += 1;
                    }
                    self.string = Some(Some(n));
                    i += 2 + n;
                    continue;
                }
                '\'' => {
                    // A char literal ('{', '\'', '\u{7b}'); anything else is a
                    // lifetime or a label and has no closing quote.
                    if ch(i + 1) == Some('\\') {
                        // Past the escaped character itself: `'\''` must not
                        // close on its own escaped quote.
                        let mut k = i + 3;
                        while k < cs.len() && ch(k) != Some('\'') {
                            k += 1;
                        }
                        i = k + 1;
                        continue;
                    }
                    if ch(i + 2) == Some('\'') {
                        i += 3;
                        continue;
                    }
                }
                '(' | '[' | '{' => {
                    self.opened_brace |= c == '{';
                    self.depth += 1;
                }
                ')' | ']' | '}' => {
                    self.depth -= 1;
                    if self.depth < 0 || (self.depth == 0 && c == '}' && self.opened_brace) {
                        return Some(after(i));
                    }
                }
                ';' | ',' if self.depth == 0 && !self.opened_brace => return Some(after(i)),
                _ => {}
            }
            i += 1;
        }
        None
    }
}

/// Does `line` carry a configuration attribute that puts everything after it
/// under the test configuration?
///
/// The bare `gate` is the common case. The rest of this exists because the
/// bare form is not the only one: a test module that must ALSO be gated on the
/// architecture writes the predicate as a `cfg(all(..))`, which contains the
/// test token but not the bare attribute. That is not hypothetical —
/// `x64/objects.rs`'s three ZGC / `ldc` test modules EXECUTE the x86-64 bytes
/// they emit, so they are gated on `target_arch` as well (on aarch64 they are
/// a `SIGSEGV` that kills the whole test binary). Before this, a module gated
/// that way was scanned as PRODUCTION code and its assertions counted as
/// panics.
///
/// The negated-predicate exclusion is the other half, and it is not
/// decoration: a `cfg` that selects NOT-test marks production code — eight
/// of them sit above their file's test module (`deopt.rs`, `platform.rs`,
/// `ir_check_elim.rs` x3, `jfr_compile_decision.rs`, `x64/inlining.rs`,
/// `x64/licm.rs`) — and treating one as the start of test code would stop
/// the scan early and SHRINK that file's count, which this ratchet also
/// refuses.
fn opens_test_code(line: &str, gate: &str) -> bool {
    if line.contains(gate) {
        return true;
    }
    let cfg_open = format!("#[{}(", ["cf", "g"].concat());
    let Some(at) = line.find(&cfg_open) else {
        return false;
    };
    // Drop every `not(..)` sub-predicate, then look for the test token in what
    // is left. `cfg(not(test))` leaves nothing and is production code;
    // `cfg(all(test, not(target_os = "windows")))` — `platform.rs`'s
    // `near_globals_tests` — keeps its `test` and is a gate. Rejecting any
    // predicate that merely CONTAINED a negation missed that module.
    let rest = without_negations(&line[at + cfg_open.len()..]);
    let rest = rest.as_str();
    let token = ["te", "st"].concat();
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    rest.match_indices(token.as_str()).any(|(i, _)| {
        let before = rest[..i].chars().next_back();
        let after = rest[i + token.len()..].chars().next();
        before.is_none_or(|c| !ident(c)) && after.is_none_or(|c| !ident(c))
    })
}

/// `pred` with each `not(..)` sub-predicate (to its matching parenthesis)
/// removed.
fn without_negations(pred: &str) -> String {
    let neg = format!("{}(", ["no", "t"].concat());
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(pred.len());
    let mut rest = pred;
    while let Some(at) = rest.find(&neg) {
        let bounded = rest[..at].chars().next_back().is_none_or(|c| !ident(c));
        if !bounded {
            out.push_str(&rest[..at + neg.len()]);
            rest = &rest[at + neg.len()..];
            continue;
        }
        out.push_str(&rest[..at]);
        let mut depth = 0i64;
        let mut end = rest.len();
        for (i, c) in rest[at..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = at + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Total occurrences of every needle in `code`.
fn count_occurrences(code: &str, needles: &[String]) -> usize {
    needles
        .iter()
        .map(|n| code.matches(n.as_str()).count())
        .sum()
}

/// `(relative path, count)` for every file under `root` with a non-zero count.
fn panic_sites(root: &Path) -> BTreeMap<String, usize> {
    let gate = format!("#[cfg({})]", ["te", "st"].concat());
    let needles = needles();
    let mut files = Vec::new();
    rust_sources(root, &mut files);
    files.sort();
    let mut counts = BTreeMap::new();
    for path in &files {
        let shown = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string()
            .replace('\\', "/");
        // Rule 1: a file named `tests.rs` is test code end to end.
        if shown == "tests.rs" || shown.ends_with("/tests.rs") {
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => panic!("cannot read {}: {e}", path.display()),
        };
        let n = count_occurrences(&non_test_code(&text, &gate), &needles);
        if n > 0 {
            counts.insert(shown, n);
        }
    }
    counts
}

#[test]
fn panicking_constructs_in_the_jit_do_not_grow() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    assert!(
        files.len() > 20,
        "only {} Rust sources under {} — the walk is not reaching the crate, so \
         this ratchet would pass vacuously",
        files.len(),
        root.display()
    );

    let actual = panic_sites(&root);
    assert!(
        !actual.is_empty(),
        "counted no panicking constructs under {} — the scanner is broken, not \
         the crate clean",
        root.display()
    );
    let frozen: BTreeMap<String, usize> =
        FROZEN.iter().map(|(f, n)| ((*f).to_string(), *n)).collect();

    let mut grew = Vec::new();
    let mut shrank = Vec::new();
    for (file, &now) in &actual {
        match frozen.get(file) {
            Some(&was) if now > was => grew.push(format!("  {file}: {was} -> {now}")),
            Some(&was) if now < was => shrank.push(format!("  {file}: {was} -> {now}")),
            Some(_) => {}
            None => grew.push(format!("  {file}: 0 -> {now} (not in the table)")),
        }
    }
    for (file, &was) in &frozen {
        if !actual.contains_key(file) {
            shrank.push(format!("  {file}: {was} -> 0 (all removed)"));
        }
    }

    assert!(
        grew.is_empty(),
        "jit/src gained {} panicking construct(s) in non-test code:\n{}\n\n\
         WHAT TO DO. The JIT's contract (jit/src/aarch64.rs, `Reg::from_u8`, \
         contract C11) is: never panic in production, bail to the interpreter \
         instead. `tiered::contain_compile_panic` will catch this one, but a \
         caught compile panic is a PERMANENT decline for that method \
         (CompileOutcome::declined), not a retryable bailout.\n\
         \n\
         1. Preferred: remove the panic. Return `Err(Bailout::new(..))` (see \
         `jit/src/bailout.rs`), or `self.fail(\"...\")` in the single-pass \
         backends, or `None` where the caller already treats `None` as \
         \"cannot compile this\". `.unwrap_or(`, `.unwrap_or_else(` and \
         `.unwrap_or_default()` are not counted, so a real default is also a \
         fix.\n\
         2. If it genuinely cannot be removed — a `const fn` range check, a \
         `match` arm a preceding pattern already made unreachable — raise that \
         one file's number in FROZEN in \
         jit/tests/panic_free_compile_ratchet.rs AND add a line to the FROZEN \
         doc comment saying which site it is and why it cannot bail. A number \
         raised without a justification line is the thing this test exists to \
         make impossible to do quietly.\n\
         \n\
         Do NOT raise the grand total by lowering another file: the table is \
         per file precisely so that a new panic in one file cannot be paid for \
         with a removed panic in another.",
        grew.len(),
        grew.join("\n")
    );
    assert!(
        shrank.is_empty(),
        "jit/src has FEWER panicking constructs than the frozen table says:\n{}\n\n\
         That is good news, and the ratchet has to be told about it or the \
         removal can be undone without anything noticing. Lower those rows in \
         FROZEN in jit/tests/panic_free_compile_ratchet.rs to the new counts, \
         and delete the corresponding justification from the FROZEN doc \
         comment — a justification for a site that no longer exists is how that \
         comment starts describing code that is not there.",
        shrank.join("\n")
    );
}

/// The scanner, against text whose answer is known. A scanner nobody has
/// watched fail passes vacuously — and this one has three rules that are each
/// easy to get subtly wrong.
#[test]
fn the_scanner_applies_the_three_rules_it_documents() {
    let gate = format!("#[cfg({})]", ["te", "st"].concat());
    let needles = needles();
    let uw = format!(".{}()", ["un", "wrap"].concat());
    let ex = format!(".{}(", ["ex", "pect"].concat());
    let pa = format!("{}!", ["pan", "ic"].concat());

    // Rule 3: whole-line comments are dropped, including doc comments, and a
    // needle in one does not count.
    let commented =
        format!("// a{uw} comment\n    /// doc {ex}\"x\")\n//! module {pa}\nlet x = 1;\n");
    assert_eq!(
        count_occurrences(&non_test_code(&commented, &gate), &needles),
        0
    );

    // …but a TRAILING comment is not dropped, which the module doc says is
    // deliberate. If this ever changes, the doc has to change with it.
    let trailing = format!("let x = y; // was y{uw}\n");
    assert_eq!(
        count_occurrences(&non_test_code(&trailing, &gate), &needles),
        1
    );

    // Rule 2: nothing inside the gated item counts.
    let gated = format!("let a = b{uw};\n{gate}\nmod tests {{ let c = d{uw}; }}\n");
    assert_eq!(
        count_occurrences(&non_test_code(&gated, &gate), &needles),
        1
    );

    // …and code AFTER the gated item counts again. The wave-3 shape: one
    // test-only helper near the top of a file, production code below it.
    let early_helper = format!(
        "{gate}\nfn helper(v: &mut [u8]) {{\n    let x = v.first(){uw};\n}}\n\
         fn prod() {{ let c = d{uw}; }}\n\
         {gate}\nthread_local! {{ static F: u8 = 0; }}\n\
         fn prod2() {{ let e = f{uw}; }}\n\
         {gate}\nmod decl;\n\
         fn prod3() {{ let g = h{uw}; }}\n\
         {gate} use std::fmt; fn prod4() {{ let i = j{uw}; }}\n"
    );
    assert_eq!(
        count_occurrences(&non_test_code(&early_helper, &gate), &needles),
        4,
        "production code after a gated fn / thread_local / mod decl / use is scanned"
    );

    // A brace inside a string, a raw string, a char literal or a comment of the
    // gated item neither ends it early nor keeps it open.
    let lexical = format!(
        "{gate}\nmod tests {{\n    const J: &str = \"class A {{ \";\n    \
         const R: &str = r#\"}} \"quoted\" {{\"#;\n    const C: char = '{{';\n    \
         const D: char = '\\'';\n    fn f<'a>(x: &'a u8) {{ /* }} */ let _ = x{uw}; }}\n}}\n\
         fn prod() {{ let c = d{uw}; }}\n"
    );
    assert_eq!(
        count_occurrences(&non_test_code(&lexical, &gate), &needles),
        1,
        "the gated module is skipped as one item and prod() after it is counted"
    );

    // A compound predicate written across lines, with a negated clause BESIDE
    // the test token, is still a gate (`platform.rs`'s `near_globals_tests`).
    let multiline = format!(
        "#[{c}(all(\n    {t},\n    {n}(target_os = \"windows\")\n))]\nmod m {{ let c = d{uw}; }}\n\
         let z = e{uw};\n",
        c = ["cf", "g"].concat(),
        t = ["te", "st"].concat(),
        n = ["no", "t"].concat(),
    );
    assert_eq!(
        count_occurrences(&non_test_code(&multiline, &gate), &needles),
        1,
        "a cfg(all(test, ..)) split over lines gates its module"
    );

    // A gated last match arm with no trailing comma ends at the match's `}`.
    let arm =
        format!("match k {{\n    0 => a{uw},\n    {gate}\n    1 => b{uw}\n}}\nlet z = c{uw};\n");
    assert_eq!(count_occurrences(&non_test_code(&arm, &gate), &needles), 2);

    // Rule 2, the compound form: a test module that is ALSO gated on the
    // architecture stops the scan too. Before `opens_test_code` it did not,
    // and `x64/objects.rs`'s three modules were scanned as production.
    let compound = format!(
        "let a = b{uw};
#[{c}(all({t}, target_arch = \"x86_64\"))]
mod m {{ let c = d{uw}; }}
",
        c = ["cf", "g"].concat(),
        t = ["te", "st"].concat(),
    );
    assert_eq!(
        count_occurrences(&non_test_code(&compound, &gate), &needles),
        1,
        "a cfg(all(test, ..)) module is test code"
    );

    // ...and the negated predicate is NOT: it marks production code, so the
    // scan must carry on THROUGH it. Getting this wrong shrinks eight files'
    // counts, which this ratchet refuses just as loudly as growth.
    let negated = format!(
        "let a = b{uw};
#[{c}({n}({t}))]
fn prod() {{ let c = d{uw}; }}
",
        c = ["cf", "g"].concat(),
        n = ["no", "t"].concat(),
        t = ["te", "st"].concat(),
    );
    assert_eq!(
        count_occurrences(&non_test_code(&negated, &gate), &needles),
        2,
        "cfg(not(test)) marks production code and must not stop the scan"
    );

    // A `cfg` that mentions no test predicate at all leaves the scan alone.
    let unrelated = format!(
        "let a = b{uw};
#[{c}(target_arch = \"x86_64\")]
fn prod() {{ let c = d{uw}; }}
",
        c = ["cf", "g"].concat(),
    );
    assert_eq!(
        count_occurrences(&non_test_code(&unrelated, &gate), &needles),
        2
    );

    // Occurrences, not lines: two on one line count twice.
    let twice = format!("let z = a{uw} + b{uw};\n");
    assert_eq!(
        count_occurrences(&non_test_code(&twice, &gate), &needles),
        2
    );

    // The near misses the needles must NOT match.
    let near = format!(
        "let a = o.{u}_or(0);\nlet b = o.{u}_or_else(|| 0);\nlet c = o.{u}_or_default();\n\
         let d = r.{e}_err(\"x\");\nlet f = maybe_{p}ky();\n",
        u = ["un", "wrap"].concat(),
        e = ["ex", "pect"].concat(),
        p = ["pan", "ic"].concat(),
    );
    assert_eq!(
        count_occurrences(&non_test_code(&near, &gate), &needles),
        0,
        "the needles matched a non-panicking construct: {near}"
    );

    // And each needle is found when it really is there.
    // The three bang-macro needles are assembled with a `!` suffix rather
    // than nested `format!` calls: `clippy::format_in_format_args` fires on
    // the nested form, and the string splitting is only here so this file
    // cannot match itself.
    let ur = ["unreach", "able"].concat() + "!";
    let td = ["to", "do"].concat() + "!";
    let ui = ["unimple", "mented"].concat() + "!";
    let all = format!(
        "a{uw}; b{ex}\"m\"); {pa}(\"m\"); {ur}(); {td}(); {ui}();
"
    );
    assert_eq!(count_occurrences(&non_test_code(&all, &gate), &needles), 6);
}

/// The frozen table names only files that exist, and nothing else.
///
/// A row for a file that was renamed or deleted would otherwise sit in the
/// table forever, reading as an allowance for a site nobody can find.
#[test]
fn every_frozen_row_names_a_file_that_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut missing = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (file, _) in FROZEN {
        if !seen.insert(*file) {
            panic!("FROZEN lists {file} twice; the later row silently wins");
        }
        if !root.join(file).is_file() {
            missing.push(*file);
        }
    }
    assert!(
        missing.is_empty(),
        "FROZEN names {} file(s) that do not exist under jit/src: {missing:?} — \
         they were renamed or deleted, and their rows have to go with them",
        missing.len()
    );
}
