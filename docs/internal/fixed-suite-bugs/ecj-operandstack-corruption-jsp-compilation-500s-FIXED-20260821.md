# ECJ's own `OperandStack.pop()` threw `AssertionError: Unexpected operand at stack top` while compiling JSPs — an INLINED callee's result parked at the wrong operand-stack depth

| | |
|---|---|
| **Status** | ✅ FIXED — 2026-08-21, `jit/src/x64/inlining.rs` + `jit/src/x64/bytecode_walk.rs` (both switch arms) |
| **Severity** | high — any test that compiles a JSP was at risk; hit correctness (500s) and throughput |
| **HotSpot** | PASS on every class checked |
| **CratonVM** | FAIL before the fix, reproduced deterministically; **all 15 classes PASS after** |
| **Root cause** | the x64 single-pass **inliner** pushed a spliced callee's return value from a spill cursor its own callee-locals + merge-region reservations had advanced past the caller's operand depth — and `tableswitch`/`lookupswitch` were the one branch shape that did not repair it |

This page supersedes the 2026-08-06 `…-FIXED.md` record and its 2026-08-21
`…-REGRESSED.md` successor. The 2026-08-06 fix is **not** reverted and is still
correct; the same *symptom* came back through a different emission arm.

## Why the 2026-08-06 fix did not cover it

The original defect was in `bytecode_walk.rs`'s two **direct-call** arms: a call
site carrying `invoke_info` reserves a cold-deopt copy of its arguments through
`reserve_direct_call_service_slots`, which moves `next_spill_offset` past the
argument slots, and the return value was pushed from that moved cursor. That fix
(capture the post-pop cursor, restore it before the push) is unchanged in the
current tree and still passes its regression test.

The first isolation arm this session **falsified the assumption that it was the
same family**:

| Arm | Result |
|---|---|
| base (`dev@5b606e85e`, the reported binary) | 5 failures, 10 asserts |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | **still failing** — 2 asserts by test case 14 |

The doc's own "next step" predicted exactly this branch: *"if not, this is a
related-but-distinct spill-cursor bug in a newer code path"*. It is.

## Root cause

`CRATONVM_DBG_JIT_DISASM='OperandStack.pop,TypeIds.getCategory'` on the failing
run shows what changed since 2026-08-06: `TypeIds.getCategory` is no longer
CALLED, it is **INLINED**. `OperandStack.pop(OperandCategory)` compiles to (all
offsets from the real dump; `base_spill_offset` = `0x50`, so slot *i* lives at
`rbp-0x50-8i`):

```
 74b: mov [rbp-50h],rax   ; getfield TypeBinding.id  -> slot 0        (depth 1)
 74f: mov rax,[rbp-50h]   ; splice: pop the argument …
 753: mov [rbp-58h],rax   ; … into the callee's local 0 (callee_local_base)
 757: mov rax,[rbp-58h]   ; inlined getCategory body: iload_0
 75b: mov [rbp-80h],rax   ;   pushed in the CALLEE's operand area (save_spill)
 75f: mov rax,8           ;   iconst 8  (T_double)
 778: cmp eax,ecx         ;   if_icmpeq
 7a9: mov rax,2           ;   iconst_2
 7b0: mov [rbp-80h],rax
 7b4: mov r11,[rbp-80h]   ;   merge spill …
 7b8: mov [rbp-60h],r11   ;   … into the merge region (merge_base)
 7d4: mov rax,[rbp-60h]   ; ireturn: pop the callee's value
 7d8: mov [rbp-80h],rax   ; push it at save_spill   <-- WRONG, belongs at slot 0
...
1455: mov rax,[rbp-50h]   ; if_icmpeq reads slot 0 …
1459: mov rcx,[rbp-58h]   ;                    … and slot 1
145d: cmp eax,ecx
```

`try_emit_inline_body` carves the callee's locals **and** a
`MAX_INLINE_MERGE_DEPTH` (= 4) merge region out of the caller's spill area, and
it has to do that *before* the arguments are popped — an argument slot stays
live until `load_slot_to_reg` marshals it into a callee local. `pop_stack`'s
reclaim arm (`off == next_spill_offset - 8`) therefore cannot recognise any
argument as the top slot and never rewinds. The `xreturn` arms then pushed the
result from `save_spill`, i.e. `callee_locals + 4` slots above the caller's
actual operand top.

Inside the caller's basic block that is invisible — the linear walk writes and
reads the same shifted slots and computes the right answer. It becomes wrong
code at the first merge point whose depth is re-established from the bytecode at
the canonical `base_spill_offset + i*8`. Here that is the `tableswitch` at bci
20: its arms push the expected category at canonical slot 1 while slot 0 still
holds the **raw `TypeBinding.id`** the caller pushed before the splice. So the
compiled body compared `TypeBinding.id` against the expected category instead of
`TypeIds.getCategory(id)` — the same wrong comparison the 2026-08-06 record
describes, produced by a different arm.

### Why now, and why through a `tableswitch`

Two independent facts had to meet:

* **`0f55466d0` (2026-08-18) — "the inline emitter can splice a value-producing
  branch merge"** introduced `MAX_INLINE_MERGE_DEPTH` and made *branchy* callees
  spliceable. `TypeIds.getCategory` is an if-chain; before that commit the
  splice bailed and the direct-call arm ran, which is why the 2026-08-06 fix
  held through the 2026-08-14 census and broke by 2026-08-21. The `save_spill`
  push predates the refactor that moved this code into `inlining.rs`
  (`e7e810089`, 2026-08-03) — it was latent, and this commit lit it.
* **`tableswitch`/`lookupswitch` never canonicalised.** `ifeq`/`if_icmp`/`goto`
  all call `canonicalize_stack()` before branching to a forward target, which
  relocates every surviving operand onto `base_spill_offset + i*8` and silently
  repaired the shift. The two switch arms did not — while their own arms are
  revived from dead code at exactly that canonical layout. A first attempt at a
  regression test used `ifeq` and **passed with the fix reverted**; that is what
  found this second half.

## Fix

Two changes, either of which alone clears the ECJ shape; both are correct
independently and both are kept.

1. **`jit/src/x64/inlining.rs`** — compute `caller_post_pop_spill` (the minimum
   `Frame` offset among the popped arguments, seeded with the pre-reservation
   cursor for a no-arg or register-resident callee) and restore *that*, not
   `save_spill`, in the `ireturn`/`lreturn`/`areturn`/`freturn`/`dreturn` arm,
   the void `return` arm, and the function's exit path — which must agree with
   the return arms because a multi-`return` callee falls out of the walk on
   whichever one was emitted last. Safe: the `pop_to_rax` load has already read
   the value out of the callee's slot, and `caller_post_pop_spill` is strictly
   below `callee_local_base`, so the store cannot alias anything the callee owns.

   The void arm is not observable at the outer level — the main walk's
   per-instruction `reset_spills()` already lowers the cursor to just past the
   highest live operand — but it *is* load-bearing for a **nested** splice,
   whose mini-walk has no such reset. A test asserting otherwise was written and
   deleted for passing either way.

2. **`jit/src/x64/bytecode_walk.rs`** — the `tableswitch` (0xaa) and
   `lookupswitch` (0xab) arms now `canonicalize_stack()` when every target is
   forward and more than the key is live, *before* popping the key (so the key
   participates in the relocation and cannot be clobbered by another slot's
   move) — the same ordering rule the `ifeq` arm states for itself.

The IR (C2) lowerer does not inline (`jit/src/ir.rs:1108` says so explicitly) and
the aarch64 backend has no inliner, so the x64 single-pass backend is the only
site — same scope conclusion the 2026-08-06 record reached for its own arm.

## Regression tests

`jit/src/x64/tests.rs`:

* `a_spliced_callees_result_lands_at_the_callers_operand_depth` — compiles a
  reference-returning leaf callee with and without an inline site and asserts the
  oop-map frame slot recorded at the following safepoint is the same. This pins
  the **splice half alone**. Negative control (fix reverted, test kept):
  `left: [104] right: [56]` — a six-slot shift.
* `an_inlined_result_held_across_a_tableswitch_reaches_the_merge_intact` — ECJ's
  shape reduced to its skeleton and **executed**: a spliced result held across a
  `tableswitch` and compared against a constant pushed at a switch arm.
  Satisfied by either half of the fix, deliberately; with **both** reverted it
  fails `left: 0 right: 1`, i.e. it reproduces the miscompile.

## Verification

`bin/cratonvm-tcjsp-fix2-fe600cd7b`, Azure Linux, real JDK 25, `apps/tomcat`
fixture, one process per class. Every class the REGRESSED page named, plus the
three the 2026-08-06 record used as controls, plus `util.TestCookieFilter` (see
the HTTPS/OCSP page — same session, different defect):

| Class | 2026-08-21 before | after |
|---|---|---|
| `jasper.compiler.TestCompiler` | FAIL | **OK (12 tests)** |
| `jasper.compiler.TestEncodingDetector` | 5 failures, 10 asserts | **OK (22 tests)** |
| `jasper.compiler.TestJspDocumentParser` | FAIL | **OK (22 tests)** |
| `jasper.compiler.TestParser` | FAIL | **OK (16 tests)** |
| `jasper.runtime.TestJspContextWrapper` | FAIL | **OK (3 tests)** |
| `jasper.tagplugins.jstl.core.TestForEach` | FAIL | **OK (2 tests)** |
| `jasper.tagplugins.jstl.core.TestOut` | FAIL | **OK (2 tests)** |
| `jasper.tagplugins.jstl.core.TestSet` | FAIL | **OK (2 tests)** |
| `jasper.TestJspCompilationContext` | FAIL | **OK (5 tests)** |
| `catalina.core.TestStandardContextResources` | FAIL | **OK (4 tests)** |
| `jakarta.el.TestCompositeELResolver` | `expected:<200> but was:<500>` | **OK (1 test)** |
| `jasper.compiler.TestGenerator` (control) | — | **OK (85 tests)** |
| `jasper.compiler.TestJspConfig` (control) | — | **OK (18 tests)** |
| `jasper.compiler.TestValidator` (control) | — | **OK (12 tests)** |
| `util.TestCookieFilter` | `NoClassDefFoundError` | **OK (10 tests)** |

**15 of 15 classes pass, 216 tests, zero `Unexpected operand at stack top`.**

Unit suites: `cratonvm-jit` 2078 passed / 0 failed.

## A second defect this one was hiding

With the JIT half fixed, six of the eleven classes still failed — all with the
same **`NoSuchMethodError: java.util.jar.Attributes
cratonvm.synthetic.AnonymousObject$1.getTrustedAttributes(java.util.jar.Manifest,
java.lang.String)`** out of `java.net.URLClassLoader.definePackage`, surfacing as
the same HTTP 500 from `JspServlet.service`. That is a `SharedSecrets`
stand-in-minting defect, root-caused and fixed in the same commit; the record is
in `shared_secrets_bridge.rs`'s `alloc_singleton` doc comment and summarised on
the HTTPS/OCSP page. It is why the class table above is green and the
intermediate run was not.

## Residual

None for this defect.
