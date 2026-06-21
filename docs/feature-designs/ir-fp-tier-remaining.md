# IR FP XMM tier — remaining-work handoff

Handoff for whoever continues `activate-ir-optimizer.md` **roadmap item 3** (the
double/float XMM value tier). Three slices remain. This doc gives the concrete
mechanism, approach, and gotchas for each, plus the shared invariants you must
preserve.

## What is already DONE (on `dev`)

The FP tier is gated default-OFF behind **`CRATONVM_JIT_IR_FP`** (`=1` opt-in).
Landed increments:

| inc | what |
|-----|------|
| 30 | FP **value ops** (fadd/dadd/…, fneg/dneg), **constants** (fconst/dconst), **FP locals** (fload/dload/fstore/dstore + wide), int/long⇄FP **conversions** (i2f/i2d/l2f/l2d/f2d/d2f, f2i/f2l/d2i/d2l with the JVM NaN→0/overflow fixup). XMM0/XMM1 scratch + `fp_load`/`fp_store`/`fp_binop` in `ir_lower.rs`. |
| 31 | FP **compares/branches** — `fcmpl/fcmpg/dcmpl/dcmpg` → `Op::FCmp { double, nan_greater }` → int `{-1,0,1}` feeding the existing `if<cond>`. Branchless `ucomis` + `SETA`/`SETB` − `SUB`; NaN-unordered rule falls out (cmpl→−1; cmpg swaps operands→+1). |
| 32 | **double returns** + `D` call returns — `dreturn` builder arm; result rides RAX as 64-bit bits. |
| 33 | **float returns** + `F` call returns — `freturn` arm; result rides low-32 of RAX. |
| 34 | **FP params + `D`/`F` call-args** — dropped the gate's `!fp_in_params`; `static_call_shape` admits `D`/`F` args. |
| 35 | **double `ldc2_w` constants** — `cp_ldc2w_resolver` returns `(bits, is_double)`; builder lowers `dconst`/`lconst`. |

Net: a method with a full FP **signature** (params + return), FP arithmetic,
compares/branches, FP calls, and double/long constants takes the IR path. What it
still can NOT do: **FP arrays**, **frem/drem**, and **be live at a deopt** (so
int-div is still excluded).

## Shared invariants (DO NOT BREAK)

1. **Compact all-GPR i64 ABI.** The VM (`execute_jit_call`, `jit_invoke_dispatch`)
   marshals *every* value — including FP — as `to_bits() as i64` into an INTEGER
   register, and reads returns from RAX as `result as u64`/`as u32` →
   `from_bits`. **There is no XMM at the VM↔JIT or JIT↔JIT call boundary.** Any
   "needs XMM ABI marshalling" instinct (e.g. from generic JIT lore) is wrong for
   this VM — see how inc 34 turned out to need *zero* codegen.
2. **`i64::MIN` deopt sentinel.** A dispatched callee signals exception/deopt by
   returning `i64::MIN`; a legit `Long.MIN_VALUE`/`-0.0`(double)/`+0.0f`(float
   with stale upper) collides. The call-site disambiguation (`Op::Call` in
   `ir_lower.rs`; `emit_post_invoke_exception_check(ret_type)` in `x64.rs`) peeks
   the out-of-band signal via the `dispatch_threw` helper on the rare
   `RAX == i64::MIN` branch. It already matches `IrType::{Long,Double,Float}` /
   `b'J'|b'D'|b'F'`. If a new return type can be `i64::MIN`, add it there.
3. **The FP gate** (`jit/src/lib.rs`, `try_compile_inner`): a method is admitted
   to the IR FP path iff `ir_emit_fp && fp_in_body && !method_has_int_div`.
   `fp_in_body` = any opcode in `is_float_opcode`/`is_double_opcode`. New FP
   opcodes must be added to those sets or the method won't route to the FP clause.
4. **gate-OFF byte-identical.** With `CRATONVM_JIT_IR_FP` unset, no FP opcode may
   reach the IR builder (it routes to single-pass). Verify every increment is
   byte-identical gate-off (the differential `check_fp` harness compiles BOTH
   backends; a real-VM A/B is the E2E gate).
5. **Differential discipline.** Add `ir_vs_singlepass` tests via `check_fp`
   (IR == single-pass == host IEEE anchor) where single-pass also compiles the
   shape; otherwise IR == host. Then a real-VM E2E `== HotSpot` (warm a hot method
   under `CRATONVM_JIT_IR_FP=1`, diff stdout vs `java`). bt10/14/16/18 checksums
   must hold on any default flip.

## Build / test / E2E commands

```
cargo test -p cratonvm-jit --test ir_vs_singlepass     # differential (fast)
cargo test -p cratonvm-jit --lib                       # 825+ unit
cargo check -p cratonvm-vm                              # VM compiles
cargo build --release -p cratonvm-cli --bin cratonvm   # ~5-8 min; copy to a UNIQUE name
# E2E: javac a probe into scratch/, get HotSpot ref, run binary with CRATONVM_JIT_IR_FP=1, diff
```
Worktree `C:\craton\CratonVM-irlongret`, branch `feat/ir-fp-cmp-branch` (== dev
after each ff-merge — rebase onto dev before each merge; dev moves fast).
`scratch/longret/*.java` has the FP probes (gitignored).

---

## Slice A — `frem` / `drem` (smallest concept, high churn)

JVM FP remainder == C `fmod` == Rust `%` for floats (truncated, sign-of-dividend;
NaN/inf rules match: `fmod(x, inf)=x`, `fmod(inf, x)=NaN`, `fmod(x, 0)=NaN`).
**No single SSE instruction** and `a - b*trunc(a/b)` is NOT exact for large
operands — you need the libm `fmod`.

**Mechanism.** Add runtime helpers `jit_frem(a: f32, b: f32) -> f32 { a % b }`
and `jit_drem(a: f64, b: f64) -> f64 { a % b }` in `vm/src/jit/helpers.rs`.
Because of the **C ABI for `fn(f64,f64)->f64`** (args in XMM0/XMM1, return XMM0)
— which here matches the IR's own XMM0/XMM1 scratch — the lowering is:
`fp_load(XMM0, a); fp_load(XMM1, b); MOV RAX, helper; CALL RAX; fp_store(slot, XMM0)`.

**Builder** (`ir.rs`): `frem` (0x72) / `drem` (0x73) → `Op::Rem` typed
`Float`/`Double` (currently absent — they bail; see the comment at the fadd arm).
**Lowerer** (`ir_lower.rs`): the `Op::Rem` arm currently handles Int (div-zero
guard + idiv) / Long; add a `Float`/`Double` branch that emits the helper call.

**Gotchas.**
- The helper address must come through the **`JitRuntimeHelpers` golden table**
  (jit-api) — two new `RequiredPtr` fields = ~45 mechanical edit sites (struct,
  `helper_fields!` macro, `NUM_FIELDS` 43→45, the golden-offset probe table, the
  classification-count asserts 36→38, `zero_field_by_name`, `make_helpers` +
  `test_helpers_zero_values` fixtures, EVERY `jit/tests/*.rs` + `jit/src/x64.rs`
  fixture, and `build_helpers`). Follow the `dispatch_threw` / `jit_npe_with_action`
  precedent exactly (append at the END so prior golden offsets stay stable).
- **Stack alignment / shadow space**: the IR frame always reserves 32-byte shadow
  (`shadow = 32` in `Lowerer::new`) and is 16-byte aligned at call sites — the
  `Op::Call` dispatch relies on it, so a frem helper CALL is safe even in a
  call-free method. Double-check alignment if you see a crash.
- **Single-pass likely bails frem** (no fmod helper there either — its 0x72/0x73
  land in the binary-arith group that only emits inline add/sub/mul/div). So the
  `check_fp` IR==single-pass==host harness may not apply; test **IR == host**
  (a Rust `a % b` anchor) and a real-VM E2E `== HotSpot`. Confirm single-pass's
  behavior first.

## Slice B — FP array load/store (highest value, heaviest)

`faload`(0x30)/`daload`(0x31)/`fastore`(0x51)/`dastore`(0x52). **The IR has NO
array element access at all** (only `Op::ArrayLength`); every array load/store
opcode currently bails the builder. This slice introduces array addressing +
null/bounds checks to the IR for the first time — do INT arrays (iaload/iastore)
in the same increment or just FP; either way the infra is new.

**Mechanism (two viable designs).**
1. *Inline + deopt-on-fault (preferred — reuses the wired deopt).* Item 2 wired
   `Op::Guard` → `ir_deopt_entry` → interpreter resume into VM dispatch. Lower an
   array load as: null-check the arrayref (`Op::Guard` deopt if null → interpreter
   re-throws NPE), bounds-check `index u< [array + ARRAY_LENGTH_OFFSET]`
   (`Op::Guard` deopt if OOB → interpreter re-throws AIOOBE), then compute
   `[array + ARRAY_DATA_OFFSET + index*elem_size]` and `fp_load`/`fp_store` the
   element. Mirror single-pass's element addressing (`ARRAY_LENGTH_OFFSET` /
   `ARRAY_DATA_OFFSET` in `x64.rs`; element load/store at the 0x2e-0x35 / 0x4f-0x56
   codegen). Deopt-on-fault is correct (matches the div-by-zero deopt pattern) and
   needs no new helper/table entry.
2. *Helper call.* Add `jit_faload`/`jit_fastore`/… helpers (golden-table churn like
   Slice A) that do null+bounds+access and use the `i64::MIN` sentinel on fault.
   Simpler codegen, but per-element call overhead + table churn.

**New IR pieces.** A new `Op` for array load/store (or extend `Op::Load`/`Store`'s
`MemKind` with array-element variants carrying element type + the array
addressing). `set_field_info`-style plumbing for which pcs are FP-array ops.
`ir_compatible` already does NOT block array ops (it bails them only because the
builder lacks arms) — once you add the arms, also confirm the gate admits them
(`faload`/`fastore` are in `is_float_opcode`; `daload`/`dastore` in
`is_double_opcode` — already counted in `fp_in_body`).

**Gotchas.**
- **GC**: an array ref is a live oop across the bounds check/access. The IR's
  conservative frame scan roots spilled oops (the GC is non-moving while a JIT
  frame is active), so spill the arrayref before any potential safepoint — but an
  inline non-allocating load/store has no safepoint, so this is mostly about
  correctness if a guard deopts (the resume must see the oop). Reuse the
  `Op::Call`/`new` oop-handling precedent.
- **Test harness**: `check_fp` is int-in/int-out — to test arrays you must pass a
  `float[]`/`double[]`. Mirror the synthetic-heap harness the int `getfield` tests
  use (`field_helpers` / the synthetic object in `ir_vs_singlepass.rs`), allocating
  a backing buffer with the array header layout. Then E2E with a real Java array.
- **AIOOBE/NPE parity**: the interpreter must re-throw the *exact* exception at the
  *exact* bci on deopt — validate caught-exception programs `== HotSpot`.

## Slice C — FP-slot deopt resume (lifts the int-div exclusion)

Today the FP gate excludes `method_has_int_div` because an FP value live at the
int-div deopt guard can't be reconstructed by the interpreter resume. Item 2 made
`long` a real `FrameValue` (`StackSlotLong`) on resume; do the same for FP.

**Mechanism.** `frame_value_for` / `typed_stack_slot` (`ir_lower.rs`) already
carry `FrameValue::Float` for an FP slot (search the inc-30 comment). Wire the
resume side: the interpreter's IR-deopt frame reconstruction
(`resume_from_ir_deopt` / `resume_real_ir_deopt` in `interpreter.rs`) must map an
FP `FrameValue` back to a `Value::Float`/`Value::Double` on the operand
stack/locals (FP slots hold bits; rebuild the typed Value). Add a
`FrameValue::Double` width if needed (cat-2, like `StackSlotLong`). Then **drop
`!method_has_int_div`** from the FP gate.

**Gotchas.**
- This is the only remaining slice that touches the **deopt/resume** machinery
  (subtle — see `real-frame-deopt*.md` and item 2's cat-2 resume). Reuse the long
  cat-2 collapse logic; an FP cat-2 (double) slot reconstructs like a long.
- Validate with a method that has BOTH an int-div (deopt point) AND a live FP
  value across it, then force the deopt (div-by-zero) and confirm the resumed
  interpreter has the correct FP value + throws `== HotSpot`.
- After this, also revisit whether any remaining `ldc2_w`/FP exclusions can drop.

## Default flip (the finish line)

Once A/B/C land and soak clean, flip `CRATONVM_JIT_IR_FP` default-ON (mirror the
inc-23/29 `IR_CALL`/`IR_LONG` flips: change the 3+ `try_compile` gate reads in
`interpreter.rs` from `std::env::var_os(..).is_some()` to
`std::env::var(..).map_or(true, |v| v != "0")`). Gate the flip on the app gauntlet
+ bt10/14/16/18 checksums (`135854 / 3222190 / 14985902 / 68332206`) + the
kafka/keycloak/tomcat suites, exactly like the long track.
