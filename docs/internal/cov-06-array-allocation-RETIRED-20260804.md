# COV-06 — RETIRED 2026-08-04. `newarray`/`anewarray` shipped; `multianewarray` left refused

Was `docs/known-issues/c2/cov-06-array-allocation.md`. Owned the
`!scan.anewarray_ops.is_empty()` conjunct of `ir_compatible`
(`cov-05`/`cov-07` each own a different conjunct in the same function), and
the `0xbc` / `0xbd` / `0xc5` arms.

The brief staged this as three increments — `newarray` first (no class
resolution, proves the `Op::NewArray` shape with nothing else attached), then
`anewarray` against an already-loaded class, then `multianewarray` "last, or
never." The first two shipped in one session; `multianewarray` was left
refused, explicitly, per the brief's own suggested exit.

## What the lane shipped

| | |
|---|---|
| `Op::NewArray` | `[ctrl, mem, length]` → `Ref`. Carries `element_type` (the JVM atype, 4-11) and `component_class_id`. `element_type != 0` is a primitive `newarray`; `element_type == 0` is a reference `anewarray`, and `component_class_id` names the loaded class. |
| lowering | Both shapes go through one new `emit_new_array_stub` (mirrors `emit_new_object_stub`), calling `helpers.newarray` / `helpers.anewarray_object` — the SAME helpers the single-pass backend's 0xbc/0xbd arms already call. Same zero-on-failure convention as `Op::New`: `0` → the JIT-wide `i64::MIN` sentinel. |
| `newarray` (0xbc) | No resolver at all — the atype is read straight from the bytecode. `IrBuilder::build` needed only a new match arm; `ir_compatible` needed no new conjunct (it never had one for `has_newarray`). |
| `anewarray` (0xbd) | Admitted via `IR_MAX_ARRAY_ALLOCATIONS` (16, same shape as `new`'s cap), resolved through the SAME `cp_new_resolver` `new` uses. A `Deferred` (not-yet-loaded) component class is simply absent from `anewarray_info`, bailing that site — and so the method — to single-pass, the identical model `new` already has for its own deferred case. |
| `multianewarray` (0xc5) | Left refused, deliberately — see "Why `multianewarray` stayed refused" below. |
| scalar replacement | Untouched: `escape_analysis.rs` already refused to scalar-replace any `Op::NewArray` (it predates this lane — the op existed as unused scaffolding before cov-06 wired it up), so the brief's "refuse scalar-replacing an array in this lane" ask was already satisfied by code already in the tree. |
| tests | `jit/src/ir.rs`: 3 new `IrBuilder::build` unit tests (newarray, resolved anewarray, unresolved-anewarray-bails). `jit/src/ir_lower.rs`: 5 new unit tests (both allocation shapes live, the null-failure→sentinel conversion, the no-helper-refuses guard) plus the anti-vacuity witness test's op swapped from `NewArray` to `I2B` now that `NewArray` has a real arm. `jit/tests/ir_vs_singlepass.rs`: one real allocate/store/load-back differential against a correctly-laid-out synthetic buffer. `vm/tests/jit_cov06_array_allocation.rs`: a real end-to-end probe (javac + the actual `cratonvm` binary) covering optimizing-tier admission, `NegativeArraySizeException` on a hot method, and a fresh `anewarray` array surviving a moving young-gen collection as a GC root. |

## Two real bugs found while proving the "How to verify" GC-root claim

The doc's own verification list asked for "a moving-GC test for `anewarray`:
the fresh array is a root at the next safepoint, and its elements are
references the collector must rewrite." Writing that test as a real,
heap-pressure-forcing probe (not a synthetic stand-in) surfaced two genuine
defects, neither of which is specific to this lane's *codegen shape* — both
are gaps in the surrounding GC/exception machinery that a hot,
repeatedly-allocating, reference-returning method finally exercised hard
enough to hit:

1. **Missing safepoint map.** `Op::NewArray`'s lowering called the shared
   allocation stub (which can trigger a real collection via TLAB exhaustion)
   without first publishing a fresh safepoint map, unlike `Op::Call`/
   `Op::MonitorEnter`. Without one, `sp_id_slot_off` keeps naming whichever
   EARLIER safepoint last wrote it, so a collection during the allocation
   call could match a map describing a different program point.
2. **The actual root cause: missing `has_dispatch`.** An IR-compiled method
   whose only exception-stashing operation is a live allocation (`Op::New` or
   `Op::NewArray`) never got `compiled.has_dispatch = true` set — unlike a
   method with an invoke or a `getstatic`, which both explicitly set it. The
   VM's fast call entry never drains the pending exception without that flag,
   so an allocation-failure `i64::MIN` sentinel was NOT recognised as a
   deopt/exception by `execute_jit_call`: its `b'[' | b'L'` return arm only
   checked `result == 0`, so `i64::MIN` (`!= 0`) was pushed as
   `Value::Object(Some(ObjectRef::from_raw(0x8000000000000000)))` — an
   address no live heap region contains. Reading it back later degraded
   through the NaN-box plausibility gate to `Value::Long`, and a subsequent
   array access on it read as silently null instead of throwing the real
   exception.

`Op::New` has the identical `has_dispatch` gap (fixed alongside, same
one-line condition); its own safepoint-map call was left as a follow-up,
flagged separately.

Bisected with a minimal repro that would NOT reproduce standalone (a
single-method loop calling `newarray` in a tight loop never hit it, at any
`-Xmx`) but reproduced deterministically once embedded in a larger program at
`-Xmx 8m`/`32m` and passed cleanly at `-Xmx 64m`/`128m` — i.e., genuinely
heap-pressure-dependent, which is exactly the shape `vm/tests/
jit_cov06_array_allocation.rs`'s GC-root round was written to force. A
`CRATON_JIT_NEWARRAY_TRACE=1`-instrumented run showed the allocation call
itself always either succeeded correctly or (rarely) legitimately hit OOM —
the corruption was entirely in whether the resulting `i64::MIN` sentinel got
drained, not in the allocator.

## Why `multianewarray` stayed refused

2 events against `anewarray`'s 138 in the original survey. The single-pass
backend's 0xc5 arm is a helper call sequence (`jit_multianewarray_2d`) the IR
pipeline has no equivalent op for — `Op::NewArray` cannot represent a
multi-dimensional allocation (it carries one `length`, not a dimension
vector), so this is a genuinely different lowering shape, not a variant of
the other two. Not worth a new `Op` variant and a second resolver-fed info
map for two events. A method containing one still compiles fine on the
single-pass backend; `ir_compatible`'s `!scan.multianewarray_ops.is_empty()`
conjunct is untouched and documented in place as a deliberate decision, per
the brief's own instruction to "say so explicitly if that is the decision
rather than leaving it looking unfinished."

## Design

`Op::NewArray` already existed as unused scaffolding before this lane —
`escape_analysis.rs`'s scalar-replacement refusal, `ir_verify.rs`'s shape
checks, `regalloc.rs`'s clobber/safepoint classification, `ir_schedule`'s
safepoint treatment, and `lib.rs`'s `EaOp::NewArray` mapping all already
paired it with `Op::New` everywhere. What was missing was narrower than it
looked: `IrBuilder::build` never constructed one (no 0xbc/0xbd arm),
`ir_lower::lower_data_node` had no lowering arm for it (it was on the
`UNLOWERABLE` list), and `lib.rs`'s `has_live_new_array` gate forced a
fall-through to single-pass whenever one survived to the lowering stage
regardless. Adding the two missing arms and deleting the now-obsolete gate
was the whole shape of the fix — not a new subsystem.

The field `component_class_id: u32` was added to `Op::NewArray` (previously
just `element_type: u8`) to let one op represent both allocation shapes:
`element_type` carries the JVM atype (4-11) for a primitive array, or `0`
(not a valid atype) as the "this is a reference array" discriminant, with
`component_class_id` naming the loaded class in that case. This is why
`Op::NewArray { .. }` wildcard matches elsewhere in the tree needed no
changes — only the handful of sites that construct or destructure the
specific fields did.

Three exhaustive-match sites needed the new op added by hand, the same
"three enumerations of one set" shape cov-05 hit: `ir_lower::
op_defines_result_slot` (missed initially — its own doc says it must track
`lower_data_node`'s arms, and `Op::NewArray`'s arm alone was not enough
without this), `ir_lower::scan_frame_needs` (needed for `needs_context`, so
the allocation call's VM-context argument is actually reserved a frame slot
by the prologue), and `declared_lowering`'s own exhaustive match (moving
`NewArray` off `UNLOWERABLE`).

## Verification

`jit/src/ir.rs` + `jit/src/ir_lower.rs` unit suite: 1886 passing, 0 failed
(up from 1885 pre-lane; 1 ignored, pre-existing). `jit/tests/
ir_vs_singlepass.rs`: 141 passing (126 pre-lane + 15 cov-05 tests merged in
alongside this lane's own 1 new differential). `vm/tests/
jit_cov06_array_allocation.rs`, `jit_cold_new_cp.rs`,
`jit_collection_ctor_identity.rs`: all passing against the merged, `--release`
-built tree on the Azure build host. All four suites re-verified after each
of the two bug fixes above, not just once at the end.

This lane's verification is unit/integration-level, not a full Spring Boot
suite before/after count in the shape `cov-01`–`cov-05` each ran — the
survey's 138/1/2 event counts were not re-measured against a real corpus
after this fix. That re-survey (`CRATONVM_DBG=ir-compiles` against a real
workload, watching `anewarray_ops` refusals fall and `bodies` rise) is the
natural first step for whoever picks up the next lane, per cov-05's own
retirement note: closing one conjunct re-ranks every other lane's counts.

## Residuals

* **`multianewarray`** (`scan.multianewarray_ops`) — left refused,
  deliberately, per above. `IR_MAX_ARRAY_ALLOCATIONS` (16) bounds
  `anewarray` admission the same way `IR_MAX_ALLOCATIONS` bounds `new`.
* **No re-survey against a real workload** — see "Verification" above.
* **`Op::New`'s own missing `has_dispatch`** — fixed alongside cov-06's fix
  in the same commit (identical one-line condition, `Op::New{..} |
  Op::NewArray{..}`), since it's the exact defect this lane's own GC-root
  test surfaced. `Op::New`'s missing safepoint-map call (the first, less
  consequential bug found along the way) was NOT folded in — flagged
  separately for its own dedicated repro + fix + verification pass.
* **Not-yet-loaded `anewarray` targets stay on single-pass**, unconditionally
  — same shape as `cov-05`'s identical residual for `checkcast`/`instanceof`,
  and the same reason: a deferred-resolution helper call would host a user
  classloader's `loadClass`/`findClass` inside compiled code, which this lane
  declined to take on.

## Reproducing

```bash
cd jit && cargo test --lib -- newarray anewarray
cargo test --test ir_vs_singlepass -- newarray
cd ../vm && cargo test --release --test jit_cov06_array_allocation -- --nocapture
```

The GC-root round in `jit_cov06_array_allocation.rs` needs a real JDK
(`CRATONVM_TEST_JDK` or `JAVA_HOME`) and the real `cratonvm` binary
(`CRATONVM_BIN`, or a `target/{release,debug}/cratonvm` next to the crate) —
it shells out to `javac` and the actual VM, not a synthetic stand-in, which
is what let it catch bugs a hand-built-graph unit test could not.
