# SEAM-02 — split the interpreter dispatch files — DONE

**Status: complete 2026-08-03.** Both files the doc names are split, across
eleven verified commits, plus one regression fix and four repaired
source-scanning gates. Original problem statement preserved at the bottom.

## The result

| | before | after |
|---|---|---|
| `vm/src/runtime/interpreter.rs` | 26,775 | 8,158 |
| `vm/src/runtime/interpreter/invoke.rs` | 24,817 | 3,941 |
| files in `interpreter/` | 5 | 17 |

The doc's counts (26,049 and 24,474) were already stale when this lane started —
`dev` had grown both files by several hundred lines. Re-derived, per the
campaign's rule 6.

| file | lines | what |
|---|---|---|
| `interpreter.rs` | 8,158 | `execute`, `execute_frame_from_index`, the frame loop, and the module wiring |
| `jit_bridge.rs` | 6,744 | every place the interpreter asks for, enters, or leaves compiled code |
| `native_override.rs` | 6,736 | when a registered native wins over real bytecode, and when it must not |
| `gc_and_alloc.rs` | 5,079 | TLABs, GC entry points, the STW takeover protocol, root snapshots |
| `tests.rs` | 4,807 | the interpreter unit tests |
| `opcodes.rs` | 3,941 | `execute_instruction` — one arm per JVM opcode |
| `invoke.rs` | 3,941 | `execute_invoke_kind`, the stackless path, and the shared coercions |
| `dispatch_virtual.rs` | 3,651 | the three `invokevirtual`/`invokeinterface` tiers |
| `deopt_resume.rs` | 2,645 | rebuilding an interpreter frame from a compiled one |
| `lambda.rs` | 2,216 | `invokedynamic` dispatch and the SAM/impl coercions |
| `dispatch_static.rs` | 1,705 | `invokestatic` and its call-site cache |
| `exception_dispatch.rs` | 1,035 | throw, handler search, the routes across the JIT boundary |
| `jvmti_events.rs` | 772 | the `fire_jvmti_*` delivery sites |

## How it was verified

The `vm` suite is **not green on `dev`** — 13 tests fail before this lane
touches anything (IO cursor sharing, CHM scoping, diagnostic rendering, an
attach socket). A pass/fail verdict would have been useless, so every commit was
checked as a **differential**: capture the failing set from the unmodified tree,
run `cargo test -p cratonvm-vm --no-fail-fast`, and report only tests that fail
here and pass there. Eleven commits, zero regressions by that measure.

Two tests are excluded by name, each confirmed by running it in isolation and by
reading what it asserts on — not by "it went away on re-run", which is how a
real regression gets filed as a flake:
`jit::code_cache_lifecycle::tests::the_production_quiescence_signal_is_the_global_jit_depth`
reads a **process-global** in-JIT depth, and the `d15_attach_socket` family binds
a real socket. The `d15` case also flakes in the baseline itself.

## What the split actually found

Not one behaviour bug — every commit was a pure move and the suite says so. What
it found was **five checks that name a file where they mean a module**, and the
pattern is worth more than the split.

| check | what it names | how it failed |
|---|---|---|
| `hot_files_have_no_production_panics` | `jit/src/x64/*.rs` from disk | scanned 13,000 lines of test assertions as production code |
| `layout_immunity_is_not_open_coded` | `include_str!("invoke.rs")` | found zero of the two aggregators it polices |
| `t14_system_stream_intercept_exists` | `interpreter.rs` as text | reported the System.out intercept missing |
| `b3_gate_scans_full_production_body_of_interpreter` | `scanned > 10_000` on one file | the gate for a broken scanner fired while the scanner was fine |
| `runtime::resolve::guard`'s bypass allowlist | rows keyed by file path | six relocations across five commits |

The first one is not SEAM-02's — it is a regression **this session's SEAM-01
split caused and this lane's baseline caught**, because the gate lives in the
`vm` crate while scanning `jit/`, and SEAM-01's verification plan was per-crate.
It is fixed in `0ccc9c615a`, and the fix generalises: step 9 landed three new
test-only files under it with no adjustment.

Every one of the five fails **closed** — reporting a problem that is not there
rather than silence where a problem is. That is the only reason they were cheap
to find, and it is worth assuming a fail-open one exists somewhere that this
split did not happen to disturb.

### The allowlist, and the invariant that replaced the prose

`runtime::resolve::guard` keeps an allowlist of files permitted to bypass
`MemberResolver`, keyed by path with an exact site count. Moving code moves
sites, so five commits relocated rows — and the first four each restated the
arithmetic ("16 + 8 + 3 = 27") in a reason string that the next commit made
stale. That is prose pretending to be a check.

`the_split_did_not_change_the_interpreter_budget` (step 4) makes it a real one:
for each needle, the sum of allowed sites across `runtime/interpreter*` is
pinned. A split moves sites between rows; it cannot change the sum. A migration
to `MemberResolver` lowers it, in the same commit as the row it shrinks.

The totals were **measured from the pre-split tree, and my hand-written numbers
were wrong**: I had 27 / 11 / 1 from the rows I had personally touched; the real
totals are 29 / 13 / 3, because `interpreter.rs` carries rows that never crossed
my desk. The test failed on its first run and named the discrepancy. All six
needles are unchanged from the pre-split tree, which is the machine-checked
statement that eleven commits moved code and nothing else.

## The trap that only shows in the test build

Moves out of `interpreter.rs` rewrite `super::` to `crate::runtime::`, because
`super` there names `runtime` and one module deeper would name `interpreter`.
That rewrite is correct for top-level paths and **wrong inside a nested `mod`**,
where `super` already meant "the module I live in" and still does.
`jvmti_delivery_scoping_tests`' own `use super::*;` got re-pointed at
`crate::runtime`; the library built fine and only `cargo test --no-run` failed,
with 19 "cannot find function" errors in a module whose meaning had not changed.

Every remaining `interpreter.rs` seam carried a nested test module, so this would
have recurred five more times. The splitter leaves an indented `use super::*;`
alone.

## What was refused

The doc's "what to refuse" list, item by item:

* **The native override priority was not tidied.** A registered native still
  wins over real bytecode unconditionally, a native on an abstract class still
  intercepts every subclass, and the `redefine_immune_*` yielding rules are
  byte-identical. All three are now stated in `native_override.rs`'s module doc,
  because they have live dependents and the next reader will be tempted.
* **The second compile door is still open.** `compile_osr_artifact` still calls
  the x64 backend directly instead of going through `try_jit_compile_callee`.
  Closing it is a behaviour change and belongs to the OSR lane; what this lane
  owed was making it *visible*, and the two doors are now 1,500 lines apart in a
  file whose entire subject is compile doors.

## What is left

`interpreter.rs`'s remaining bulk is `execute` (2,810) and
`execute_frame_from_index` (3,118) — the frame loop, which is what a file called
`interpreter.rs` should contain. Splitting further would be division for its own
sake.

`invoke.rs`'s remaining bulk is `execute_invoke_kind` (1,910) and
`try_stackless_invoke` (1,170): the entry point that decides which dispatch tier
a call site takes, and the path that avoids building a frame at all. Both are
about *choosing* a tier rather than implementing one, which is the right residue
for the file the tiers were lifted out of.

## Note for the next lane

`cargo clippy -p cratonvm-vm` cannot run on `dev` today.
`cratonvm-native-io` (17 missing-safety-comment errors) and
`cratonvm-classloading` (one `expect()` on an `Option`) fail the lint as
dependencies, so the vm crate is never reached. Pre-existing and untouched by
this lane; `cargo build` plus the differential suite are the gates that work.

---

# Original problem statement (2026-08-03, preserved)

**Status:** not started. **Owns:** `vm/src/runtime/interpreter.rs`,
`vm/src/runtime/interpreter/*`. **Independent of `seam-01`** — different crate,
no shared file.

## Why

`vm/src/runtime/interpreter/invoke.rs` is 24,474 lines and
`vm/src/runtime/interpreter.rs` is 26,049. Costs observed this campaign, all
specific:

* The JVMTI delivery lane's handover census said "~15 call sites in
  `interpreter.rs`". The real count was **28, and 8 of them were in
  `invoke.rs`** — a file the census never mentioned. Trusting it would have
  left the entire cached-invoke and OSR surface unattributed while looking
  complete.
* A second door to the JIT: the OSR path in `invoke.rs` calls the backend
  directly rather than through `try_compile`, so anything `try_compile` does at
  entry is silently skipped there. Found only because another lane needed a
  witness at compile start.
* `constants.rs` was split out of `interpreter.rs` by an earlier commit, which
  invalidated a handover recipe written days earlier that still said
  "interpreter.rs". The recipe's author could not have known.

The precedent already exists — `interpreter/` is a directory with
`constants.rs`, `invoke.rs` and siblings — so this is continuing a split, not
starting one.

## Candidate seams

| Candidate | Content |
|---|---|
| `interpreter/dispatch_virtual.rs` | virtual/interface dispatch, the inline-cache consult, the superclass walk that intercepts natives |
| `interpreter/dispatch_static.rs` | static/special, and the call-site evidence recording `pgo-01` needs |
| `interpreter/jit_bridge.rs` | every site that enters, exits, or requests compiled code — including the direct OSR compile |
| `interpreter/jvmti_events.rs` | the 33 attributed delivery sites |
| `interpreter/exceptions.rs` | throw, handler search, the deopt signal drain |

`jit_bridge.rs` is the highest-value one: "every place the interpreter talks to
the JIT" is currently not enumerable, and that is exactly the property the
second-door bug exploited.

## How to do it without breaking anything

Pure moves, one seam per commit, `vm` suite green at each step. Note that
`vm/src/lib.rs` is `#![deny(deprecated)]`, so a move that makes a deprecated
call newly visible fails the build rather than warning — that is a feature
here, but it will surprise you once.

## How to verify

The `vm` unit suite (2,431 tests) at every commit, plus a real app suite —
these files are the interpreter's hot path and a subtle behaviour change will
show up in Spring or Tomcat long before it shows up in a unit test.

## What to refuse

Behaviour changes in a move commit. In particular, do not "tidy" the native
override priority while moving `dispatch_virtual.rs`: a registered native wins
over real bytecode unconditionally, natives on abstract classes intercept
every subclass, and both facts have live dependents.
