# W8-E24-1 — the `aastore_check` ABI slot APPLIED (5 of 5), and SEVEN count literals `W8-E19-1` §3(b) did not carry

> **RECONCILED 2026-08-16 (merge of `dev`).** Every `aastore_check`,
> `jit_aastore_check` and `HelperFnAastoreCheck` below names the spelling that
> was current when this record was written. The merge of `dev` into
> `claude/jdk-only-mode-completion-1351c0` settled on **`aastore_type_check`**
> (ABI field), **`jit_aastore_type_check`** (helper) and
> **`aastore_store_is_refused`** (the predicate the helper and the x64 inline
> lowering now share), and the slot is `required: true` -- the old
> `aastore_check == 0` fallback that routed the whole opcode to
> `helpers.aastore` no longer exists. `grep -rn 'aastore_check' --include=*.rs`
> returns nothing in the tree. The names here are kept as written; read them as
> history, not as a pointer to live code.

> **STATUS: the atomic set is now 5 of 5 applied and the tree should compile.**
> This lane applied `W8-E19-1` §3 halves (a), (b) and (c) — `jit-api/src/lib.rs`,
> `jit-api/src/helpers_abi.rs`, `jit/src/x64/tests.rs` — on top of the two the
> E19 lane had already written (`jit/src/x64/bytecode_walk.rs`,
> `vm/src/jit/helpers.rs`). No `cargo` was run and no binary was executed in this
> lane; "should compile" is a claim about the five files being mutually
> consistent, verified by machine-counting every table, not by a build.
>
> **This is still not a performance win.** `W8-E19-1` §4's two conditions stand
> verbatim and neither has been met. §3 below adds a reason the set is worth
> landing that is *not* a throughput reason.

Lane E24, 2026-08-13. Predecessors: `W7-38`, `W8-E11-1`, `W8-E19-1`.

---

## 1. Every anchor in `W8-E19-1` §3 re-verified, and all of them matched

Re-checked against the current files before editing, as the handoff required.
**Every anchor a1–a7, b1–b9 and (c) matched exactly and uniquely.** Line numbers
had drifted by ±2 in `jit-api/src/lib.rs` (a7's golden-probe row is at 2436 in
the record and 2438 in the tree; the surrounding text is identical), which is a
line-number drift, not an anchor failure. Nothing had to be forced and nothing
was skipped.

The three findings `W8-E19-1` flagged as its own predecessor's misses were all
re-confirmed as real:

* **Five files, not four.** `jit/src/x64/tests.rs:319`'s `test_helpers()` is an
  exhaustive literal. Confirmed independently: `jit/src/x64/tests.rs` contains
  **no `..Default::default()` anywhere in the file**, and its only other builder,
  `array_test_helpers()` (line 12654), derives from `test_helpers()` by
  mutation, so the one row covers both. Every other in-tree
  `JitRuntimeHelpers { … }` either is `mem::zeroed()` (`ir_lower.rs:10033`) or
  ends in `..Default::default()` / `..dummy_helpers()`.
* **`aastore_check: 0`, not a sentinel.** Applied as `0` with the record's
  comment. `array_test_helpers()` inherits it, so both aastore-bearing codegen
  paths in that file stay on the `helpers.aastore` lowering they were written
  against.
* **b6–b9 are real.** All four applied.

---

## 2. SEVEN more literals — and FOUR of them are BUILD errors, not test failures

`W8-E19-1` §3(b) is nine edits. It is **sixteen**. The seven it does not carry
are all in `jit-api/src/helpers_abi.rs`, and the record's own framing —
"a green `cargo build` and four red tests" — is too optimistic: four of the
seven are `const _: () = assert!(…)` items, so applying b1–b9 verbatim gives a
**RED BUILD**, not a green one.

| # | site | literal | old → new | when it trips |
|---|---|---|---|---|
| b10 | `helpers_abi.rs:817` | `NUM_HELPER_FIELDS == 63` | 63 → **64** | **BUILD** (`const _`) |
| b11 | `helpers_abi.rs:825` | `JIT_HELPERS_ABI_SIZE == 504` (+ its "63 * 8 = 504" message) | 504 → **512** | **BUILD** (`const _`) |
| b12 | `helpers_abi.rs:1279` | `functions == 54` | 54 → **55** | **BUILD** (`const _`) |
| b13 | `helpers_abi.rs:1294` | `optional_fns == 12` | 12 → **13** | **BUILD** (`const _`) |
| b14 | `helpers_abi.rs:1668–1676` | the six literals in `helper_table_size_and_align_are_the_literal_abi_numbers` — `size_of` 504→512, `JIT_HELPERS_ABI_SIZE` 504→512, `NUM_HELPER_FIELDS` 63→64, `NUM_FIELDS` 63→64, `NUM_HELPER_FN_FIELDS` 54→**55**, `JIT_HELPERS_ABI_VERSION` 4→**5** | — | test |
| b15 | `helpers_abi.rs:1905` | `assert_eq!(functions, 54)` | 54 → **55** | test |
| b16 | `helpers_abi.rs:1909` | `assert_eq!(functions - required, 12)` | 12 → **13** | test |

**Why the record missed them, stated so the pattern is reusable.** `W8-E19-1`
found b6–b9 by reading the `mod tests` block; b10–b13 are *outside* `mod tests`,
in the "Compile-time ABI pins" and "slot census" sections that sit between the
tables and the tests. A reader who greps `mod tests` for the old value finds
four sites; a reader who greps the **whole file for the literals `63`, `504`,
`54` and `12`** finds eleven. The second grep is the one that works, and it is
cheap: `grep -nE '\b63\b|\b504\b|\b54\b' jit-api/src/lib.rs jit-api/src/helpers_abi.rs`.

`jit-api/src/lib.rs` needed nothing beyond a1–a7 — its only count literals are
a3 (`const _`) and a6 (test), both in the record.

### 2.1 The final counts, machine-verified against the tables themselves

Not asserted from the edits — re-derived by parsing the four tables after
editing, so the literals are checked against the data rather than against each
other:

```
helper_fields!    (lib.rs)        64 rows, last = (aastore_check, OptionalPtr)
helper_field_table! (helpers_abi) 64 rows, last = (aastore_check, Function, false)
                                  Function 55 / Offset 4 / Constant 5
                                  Function: required 42, optional 13
helper_fn_slots!  (helpers_abi)   55 rows, last = HelperFnAastoreCheck
GOLDEN_HELPER_OFFSETS             64 rows, last = ("aastore_check", 504),
                                  offsets sequential (index * 8), order == helper_field_table!
mod tests probes  (helpers_abi)   64 rows, order == helper_field_table!
mod tests probes  (lib.rs)        64 rows, indices 0..63 contiguous, names == helper_field_table!
```

`64 * 8 = 512`, and the last golden offset `504 + 8 == 512`, so
`golden_offsets_are_the_struct_offsets`'s closing assertion holds.

`zero_field_by_name` correctly needed **no** arm: it covers `RequiredPtr` only,
and this slot is optional — exactly as `W8-E19-1` §3(a) said.

No consumer of `JIT_HELPERS_ABI_VERSION`, `JIT_HELPERS_ABI_SIZE`,
`NUM_HELPER_FIELDS`, `NUM_HELPER_FN_FIELDS`, `GOLDEN_HELPER_OFFSETS` or
`HELPER_FN_SIGS` exists **outside `jit-api/src/`** — grepped repo-wide. So the
version bump 4 → 5 has no second site to update.

---

## 3. The correctness argument (`W8-E19-1` §2.3) — READ AND CONFIRMED

I was asked to land on this one way or the other. **It holds.** Every link was
read in the current tree, not inferred:

1. **`has_dispatch` is eleven terms and the helper-only arm sets none of them.**
   `jit/src/x64/driver.rs:1901` reads, in full: `invoke_info`, `direct_calls`,
   `bounds_check_stubs`, `null_check_store_stubs`, `emitted_athrow`,
   `emitted_monitor_call`, `emitted_alloc_oom_check`, `emitted_checkcast_throw`,
   `self_call_patches`, `static_field_info`, `new_info`. The `aastore_check == 0`
   arm (`bytecode_walk.rs:1875–1883`) emits three loads, one
   `emit_call_absolute(helpers.aastore)` and one
   `emit_post_invoke_exception_check` — and the guard pushes only to
   `deopt_stubs` / `exception_check_stubs`, **neither of which is on that list**
   (checked, `deopt_stubs.rs:990+`).
2. **The fast entry skips the TLS.** `jit_bridge.rs:6735`'s
   `if !compiled.has_dispatch` arm calls `try_call` / `try_call_with_context`
   under a `JitEntryGuard` and never `set_jit_thread`.
3. **The helper then fails open.** `jit_aastore_check`
   (`vm/src/jit/helpers.rs:5363`) ends `if let Some((thread, _guard)) =
   jit_thread_mut() { … return i64::MIN; }` followed by a bare `0` — its
   documented "no thread available … let the store happen rather than
   corrupting VM state" path.
4. **And `jit_aastore` inherits it**, because `helpers.rs:5530` is
   `if jit_aastore_check(vm_ptr, array_ptr, val) == i64::MIN { return; }` — the
   extraction the whole family rests on. So the helper-only arm has the same
   hole as the check-only helper; it is not insulated by being "the complete
   opcode". NPE and AIOOBE survive (they publish through plain TLS flags that
   need no thread); **only the ASE is lost**, and the illegal store lands.
5. **The inline arm closes it structurally.** It calls
   `emit_null_check_array_store_at` (`arrays.rs:281`, which does
   `null_check_store_stubs.push(...)`) and `emit_bounds_check`
   (`arrays.rs:379`, `bounds_check_stubs.push(...)`) — terms 3 and 4 of the
   eleven. `has_dispatch` becomes true as a consequence of the lowering's
   shape, not of a flag someone must remember.

`static void put(Object[] a, int i, Object v) { a[i] = v; }` compiles to
`aload_0; iload_1; aload_2; aastore; return` — no invoke, no `new`, no
`getstatic`, no `checkcast`, no `athrow`, no monitor, no `arraylength`, no
self-call. It hits every step above. And `ir_lower.rs:4777` bails on
`MemKind::Ref` stores, so an `aastore`-bearing method is guaranteed to reach the
single-pass backend where this lowering lives — the shape cannot escape into a
tier that behaves differently.

**Where I land: this set is a correctness fix first and a throughput question
second.** It is still PREDICTED, not witnessed — `W8-E19-1` §5's last row is the
falsifying run and nobody has made it. But the prediction is now checked at
every link against the current source rather than at three of them, so the
benchmark should be weighed as "how much does the correctness fix cost", not as
"does the correctness fix pay for itself". If arm B comes back slower than A, the
question becomes whether to accept the cost or close the hole another way — not
whether to abandon it.

### 3.1 Residual the argument does NOT close

The inline arm makes `has_dispatch` true, so `jit_thread_mut()` is `Some` and
the fail-open path is unreachable **for this lowering**. It does not make
`jit_aastore_check` safe in isolation: the helper still returns `0` (proceed) on
a missing thread, and the guarantee is now a coupling between two files rather
than a property of the helper. Tightening it to refuse instead of proceed would
trade a silent illegal store for a silent dropped store with no exception, which
is not obviously better and is not this lane's file. Recorded, not fixed.

---

## 4. What was applied, per file

| file | halves | applied |
|---|---|---|
| `jit-api/src/lib.rs` | §3(a) a1–a7 | **YES** — field, `helper_fields!` row, `NUM_FIELDS` 63→64 (`const _`), both hand-built test tables (`0x11C0` / `0`), the count assertion, golden probe row 63 |
| `jit-api/src/helpers_abi.rs` | §3(b) b1–b9 **+ b10–b16 (§2)** | **YES** — all sixteen |
| `jit/src/x64/tests.rs` | §3(c) | **YES** — `aastore_check: 0` with the record's comment |
| `jit/src/x64/bytecode_walk.rs` | §2.1 | already applied by E19 (not this lane's file) |
| `vm/src/jit/helpers.rs` | §2.2 | already applied by E19 (not this lane's file) |

**NOT applied — nominations, both outside this lane's files:**

* **`W8-E19-1` §3(d)** — the two `docs/known-issues/jdk-only/INDEX.md` rows
  (`W8-E11-1`'s, still never applied, and `W8-E19-1`'s). Add a third for this
  record. `W8-E19-1`'s row says "THE TREE DOES NOT BUILD"; that text is now
  wrong and should read `FIXED-UNVERIFIED` / applied 5 of 5.
* **`W8-E19-1` §6.1** — the IR tier's missing `has_dispatch` obligation for
  `Op::MonitorEnter` / `Op::MonitorExit` in `jit/src/lib.rs`. Untouched, still
  open, still PREDICTED. Unrelated to this set except in shape: it is the same
  "a lowering forgot a term of `has_dispatch`" defect class this record's §3
  walks through, which is now the third instance (`athrow` cov-07, `aastore`
  here, monitors there).

---

## 5. Verification — ALL PREDICTED, nothing run

| run | predicted | falsifies |
|---|---|---|
| `cargo build -p cratonvm-jit-api` | **PASSES** (it failed before only via `-p cratonvm-jit` / `-p cratonvm-vm`) | a miscount in §2.1 — a `const _` message names which |
| `cargo build -p cratonvm-jit`, `-p cratonvm-vm` | **PASS** — this is the whole deliverable | the set is not 5 of 5 |
| `cargo test -p cratonvm-jit-api` | green | b14/b15/b16 or a golden-table order error |
| `cargo test -p cratonvm-jit` | green, and every existing `aastore` codegen test **byte-identical** to before | `aastore_check: 0` is not routing `test_helpers()` to the fallback arm |
| `W8-E19-1` §4 conditions 1 and 2 | NOT RUN. **Gates, not advice.** | — |

The one prediction I would bet against myself on: `cargo test -p cratonvm-jit`
green. `jit/tests/*.rs`'s thirteen builders all end in `..Default::default()`
(verified), and `ir_lower.rs`'s is `mem::zeroed()`, so all of them get
`aastore_check == 0` and stay on the fallback arm — but that is a repo-wide
claim resting on one grep, and a builder that constructs its table some third
way would be a compile error in an integration test, i.e. visible only at
`cargo test`, not at `cargo build`.
