# Handoff — IR/JIT work (wire-tiered-manager Step 3 + activate-ir-optimizer steps 2/3)

Date: 2026-06-20. Branch: `dev` (shared checkout — see "Operational notes").
Companion design docs: [`wire-tiered-manager.md`](wire-tiered-manager.md),
[`activate-ir-optimizer.md`](activate-ir-optimizer.md). Those carry the
authoritative per-increment detail; this is the session-level map + next steps.

---

## 1. What this session delivered

Two threads, both on `dev`:

### A. wire-tiered-manager **Step 3** — real per-call C1/C2 backend toggle
`jit::try_compile` gained a trailing `optimize: bool`. `true` runs the optimizing
IR pipeline (C2-equivalent); `false` skips it and routes to the single-pass
`x64::compile` (C1). Threaded VM-side through `try_jit_compile_callee[_slow]`;
`background_compile_task` selects per the task's tier via
`tier_uses_optimized_backend`. Every inline caller passes `true` (no default
behaviour change). **This toggle is the lever the differential harness uses.**

### B. activate-ir-optimizer — increments 4–13
| Inc | What | Commit |
|-----|------|--------|
| 4 | DSE: a load of a *distinct local alloc* isn't a barrier | (swept) `c1e40e3d` |
| 5 | LICM recognizes real javac `Merge`-header loops | `e18a6a03` |
| 6 | LICM hoists past non-aliasing in-loop stores (local allocs) | `945b6f8c` |
| 7 | …and when the load base is a `Param` | `dd2da30b` |
| 8 | …points-to lattice resolves *store* bases through phis (cross-merge) | `97e37e62` |
| 9 | …and *load* bases (invariant-phi recognition) | `dc6b373f` |
| 10 | **Step 2**: IR-vs-single-pass differential harness | `b03e72a8` |
| 11 | **Step 1 residual**: IR multi-return DCE fix (harness caught it) | `19aea180` |
| 12 | **Step 3 slice 1**: IR builder lowers `i2b`/`i2c`/`i2s` | `1b01cc76` |
| 13 | **Step 3 slice 2**: IR builder lowers `tableswitch`/`lookupswitch` | `cc6fb467` |

Increments 4–9 (the LICM/DSE points-to alias oracle) are **gated default-OFF**
behind `CRATONVM_JIT_LICM` and are **latent** until the IR builder emits
`Op::Load`/`Op::Store`/`Op::New` (see §4) — they're sound + unit-tested but don't
fire on production methods yet.

---

## 2. The differential harness — the central tool

`jit/tests/ir_vs_singlepass.rs`. For each corpus method it compiles **both** ways
via the Step-3 toggle (`try_compile(.., optimize=true)` = IR pipeline,
`optimize=false` = single-pass `x64`), **executes both** via
`CompiledMethod::try_call`, and asserts IR == single-pass == a host-computed
answer. Any divergence is a miscompile.

Run it:
```
cargo test -p cratonvm-jit --test ir_vs_singlepass
```
It already paid for itself: it caught the multi-return miscompile (inc 11) the
first time it ran.

**Hard constraint**: the harness wires **dummy (panicking) runtime helpers**, so
it can only validate **helper-free** method shapes — pure arithmetic, branches,
loops, int conversions, switches. The moment a method needs a real helper
(field load/store, allocation, method dispatch) the dummy helper would be called
and panic. Lifting this is the gating prerequisite for the next frontier (§4).

Corpus currently covers (all green): `add`, `poly`, `sum` loop, conditional
early-return + two-branch/three-return + `abs`, `i2b`/`i2c`/`i2s` (+ chained),
`tableswitch`, `lookupswitch`.

---

## 3. Where the IR path stands

The optimizing IR path (`try_compile_inner` → `ir::IrBuilder::build` →
`ir_optimize::optimize` → `ir_schedule` → `ir_lower`) now correctly handles and
is harness-proven for the **full helper-free int instruction set**: straight-line
+ multi-op arithmetic, bitwise/shift, branchy control flow, conditional early
returns, counted loops, `i2b`/`i2c`/`i2s`, and `switch`.

Gate location: `jit/src/lib.rs` `try_compile_inner` (~line 4185):
`if optimize && ir::ir_compatible(&scan) && !method_uses_category2(...) { build … }`.
The builder's `_ => return None` (`jit/src/ir.rs`, end of the opcode match) is the
"bail to single-pass" for any unhandled opcode.

---

## 4. THE NEXT FRONTIER (biggest remaining lever)

**Make the IR builder emit `Op::Call` / `Op::Load` / `Op::Store` / `Op::New`** so
methods with `invoke*` / field / array ops take the optimizing IR path. This is
the keystone because it ALSO de-latents the entire inc-4–9 optimizer suite (DSE,
escape→scalar-replacement, LICM all only fire when these ops exist in the IR) —
i.e. it advances activate-ir-optimizer steps 3 **and** 6 at once.

Two halves, do them together:

1. **Builder + lowerer emission.** The builder bails on `getfield`/`putfield`
   (0xb4/0xb5), `invokestatic`/`virtual`/`special`/`interface` (0xb8/0xb6/0xb7/
   0xb9), `new`/array ops. `ir_compatible` already *admits* bounded counts of
   these (≤5 invokes, ≤5 fields, ≤3 new) — so only the builder/lowerer are
   missing. The single-pass `x64.rs` is the reference for the helper-call ABI
   (`getfield`, `invoke_dispatch`, `new_object`, write barriers, etc.) and the
   `JitRuntimeHelpers` table. Field/invoke resolution: the builder currently takes
   only `(code, code_len)`; the resolved field indices / invoke targets are
   threaded into `try_compile` as `cp_*_resolver` closures — you'll need to pass
   those (or pre-resolved info) into the builder.

2. **Real-helper (or VM-level) harness.** The dummy-helper harness can't validate
   helper-calling methods. Options: (a) supply *real* helper stubs in
   `ir_vs_singlepass.rs` that operate on a small synthetic heap object (e.g. a
   `Box<[i64]>` of fields) with the same ABI the JIT expects — feasible for
   getfield/putfield; harder for dispatch; or (b) move differential validation to
   the **VM level** (a `cratonvm-vm` test that runs a real Java method twice via
   `CRATONVM_BG_COMPILE`/the toggle and compares). (b) is more faithful for
   calls/allocation.

Start small: **getfield/putfield of a non-escaping local object** is the cleanest
first sub-slice — it's what most directly de-latents DSE/escape, and a synthetic
heap object in the harness can validate it.

Other (smaller) helper-free slices still open, if you want quick wins first:
`ldc`/`ldc_w` of an int constant (needs the resolved value threaded into the
builder), and stack ops (`swap`/`dup_x1`/`dup_x2`) — but these are low-value for
pure-int methods (javac mostly emits `dup_x*` for field/array stores, which bail
anyway).

---

## 5. Gotchas a future session WILL hit (hard-won this session)

- **`CachedBytecodeMethod.code` is padded with 2 trailing `0x00` bytes.**
  `jit::try_compile` strips them (`code.len() - 2`, `jit/src/lib.rs`). The VM
  builds it padded (`interpreter.rs` "padded_bytecode adds 2"; `frame.rs` asserts
  the two trailing zeros). A test/corpus method must include the 2-byte trailer or
  it silently loses its last two opcodes and emits **without a `ret`** → crash on
  call. The harness's `cached()` helper pads automatically.
- **`graph.exit` is "an exit", not the sole exit.** Each `ireturn` overwrites it
  with its own `Op::Return`, so multi-return methods leave `graph.exit` pointing
  at the *last* return. Anything that needs every exit must enumerate all
  `Op::Return` nodes — the scheduler does, and `eliminate_dead_nodes` now roots
  DCE from **all** returns (rooting from `graph.exit` alone was the inc-11 bug).
- **JIT entry ABI** (for `try_call`): Win64 / System V integer args (RCX,RDX,…
  on Windows), result in RAX, ints kept sign-extended to 64 bits by single-pass
  (the IR path uses 32-bit ops → upper bits zeroed; compare results `as i32`).
  `needs_context=true` methods take the VM pointer as the first arg
  (`try_call_with_context`).
- **IR int shifts are 32-bit** (`SHL/SAR EAX`) for `IrType::Int` — that's why
  `i2b = (x<<24)>>24` works with 24, not 56.
- **`tableswitch`/`lookupswitch` padding** is 0–3 bytes to 4-byte-align the table
  *from method start*, and branch offsets are relative to the **opcode** pc.
  `parse_switch` (`ir.rs`) is the single source of truth — the build loop AND both
  length walkers (`find_branch_targets`, `find_loop_headers`) must use it, or the
  table is mis-parsed as opcodes.
- **The IR `Store`/`Load` carries a `MemKind` (width), not a field index** — so
  the alias oracle can't be field-sensitive (two `int` fields look identical).
  Field sensitivity needs a field index in the IR Store, a separate change.

---

## 6. How to verify the current state

```
cargo test -p cratonvm-jit --lib                      # 790 pass (unit)
cargo test -p cratonvm-jit --test ir_vs_singlepass    # 12 pass (differential)
cargo build -p cratonvm-vm                             # builds clean
```
Windows note: stray test exes can hold the link lock (1104) and contend with the
concurrent orchestrator's builds —
`Get-Process | ? ProcessName -like 'cratonvm_jit*' | Stop-Process -Force` before
re-running if a link fails.

Pre-existing UNRELATED failure (not from this work): `cargo test -p cratonvm-jit`
(full, all integration tests) shows `intrinsic_arrays_ops::
test_arrays_fill_null_array_deopts` failing — array null-deopt code untouched
here (array methods go single-pass, never the IR `optimize()`); likely from the
orchestrator's concurrent merges. Worth a separate look.

---

## 7. Operational notes (shared checkout)

`C:\craton\CratonVM` is the **`dev`** checkout; branch-switching happens only in
separate worktrees. An **orchestrator** concurrently merges branches and
periodically sweeps the working tree into `save …`-style commits — twice this
session it committed my uncommitted code before I could (`87e28915`,
`c1e40e3d`). Consequence: commit promptly; `HEAD` and the branch may advance
under you between tool calls; `docs/` is gitignored so design/handoff docs need
`git add -f`. All work here is committed and verified despite the churn.

---

## 8. One-line status

Steps 2 (harness) and 1-residual (multi-return) are **done**; step 3 (gate
relaxation) is **in progress** — every helper-free int shape is now on the IR
path; the remaining frontier is field/call/alloc emission + a real-helper harness,
which is also the unlock for step 6 (broad escape analysis).
