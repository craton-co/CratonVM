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
//! 2. in every other file, scanning stops at the **first line containing
//!    `#[cfg(test)]`** — everything from there on is test code;
//! 3. a line whose first non-blank text is `//` is skipped (this blanks
//!    `//`, `///` and `//!` comments alike).
//!
//! Each of the three is a deliberate approximation with a known leak, and the
//! leaks are named here rather than papered over:
//!
//! * Rule 2 sees only `#[cfg(test)]` **inside** the scanned file. Two files —
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
//! new `.unwrap()` in `ir.rs` hide behind a removed one in `pgo.rs`: the total
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
/// **A dead module (2).** `pgo.rs`: `PgoRepository::get_or_create`'s
/// `.unwrap()` after a `contains_key`/`insert`, and
/// `DevirtualizationAnalyzer::analyze`'s `.find(…).unwrap()` on an entry whose
/// class id `dominant_type()` just returned. That module has no production
/// caller at all (its header, and `pgo.rs`'s own
/// `pgo_is_named_only_by_comments_outside_this_module` test), so neither can
/// run today. Both are also real fragilities the moment it is wired: the
/// second in particular assumes `dominant_type` and `entries` agree, which is
/// an invariant nothing enforces after a hand-built or deserialised profile.
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
/// **Lock poisoning (3).** `ir.rs` `register_site_trap_method`,
/// `method_has_site_trap`, `claim_site_trap_decision`: `.write().unwrap()` /
/// `.read().unwrap()` on a `RwLock`. These panic only if some *other* thread
/// panicked while holding the lock — that is, they turn one panic into a
/// cascade rather than starting one. Worth converting to
/// `unwrap_or_else(|e| e.into_inner())` (the set is a memo; a poisoned memo is
/// still a usable memo), which would take this row from 5 to 2.
///
/// **Miscounted test modules (46).** `x64/loop_unroll_admission.rs` (37) and
/// `x64/flag_and_header_contracts.rs` (9) are whole-file `#[cfg(test)]`
/// modules declared in `x64.rs`; every occurrence is an assertion in a test
/// (`.expect("admitted")`, `.expect("test buffer")`, …). The "first
/// `#[cfg(test)]` in the file" rule cannot see a gate that lives in another
/// file. See the module doc's "If you are adding a test".
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
const FROZEN: &[(&str, usize)] = &[
    ("aarch64_backend.rs", 2),
    ("direct_helpers.rs", 1),
    ("escape_analysis.rs", 1),
    ("ir.rs", 5),
    ("ir_optimize.rs", 1),
    ("lib.rs", 3),
    ("pgo.rs", 2),
    ("x64/arith.rs", 1),
    ("x64/disp.rs", 1),
    ("x64/flag_and_header_contracts.rs", 9),
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
    for line in text.lines() {
        if line.contains(gate) {
            break;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
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

    // Rule 2: nothing at or after the first gate line counts.
    let gated = format!("let a = b{uw};\n{gate}\nmod tests {{ let c = d{uw}; }}\n");
    assert_eq!(
        count_occurrences(&non_test_code(&gated, &gate), &needles),
        1
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
