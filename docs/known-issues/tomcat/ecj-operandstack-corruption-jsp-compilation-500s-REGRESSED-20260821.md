# ECJ's own `OperandStack.pop()` threw `AssertionError: Unexpected operand at stack top` while compiling JSPs — a JIT direct-call result parked at the wrong operand-stack depth

**Status: REGRESSED (2026-08-21).** The identical `AssertionError: Unexpected
operand at stack top` signature is back, on the current `dev` tip
(`5b606e85e`, freshly built), on all three GC collectors. The 2026-08-06 fix
below is still present in `jit/src/x64/bytecode_walk.rs` (both the
spill-cursor-reclaim comment and the `direct_jit_callee_calls_enabled` gate
are unchanged in the current tree) — this is not a revert. Either the original
fix covered only some of the triggering call shapes, or something added since
2026-08-06 reintroduces the same class of spill-cursor bug through a
different arm. Not re-isolated this session; the original doc's isolation
method (below) is the template for whoever picks this up.

## 2026-08-21 recurrence

Full 640-class Tomcat suite, 3 GCs (ZGC/G1/Generational), 1 shard each, Azure,
`dev@5b606e85e`. Same signature, same classes as the original report, hitting
all three collectors identically (not GC-specific):

`org.apache.jasper.compiler.TestCompiler`,
`org.apache.jasper.compiler.TestEncodingDetector`,
`org.apache.jasper.compiler.TestJspDocumentParser`,
`org.apache.jasper.compiler.TestParser`,
`org.apache.jasper.runtime.TestJspContextWrapper`,
`org.apache.jasper.tagplugins.jstl.core.TestForEach`,
`org.apache.jasper.tagplugins.jstl.core.TestOut`,
`org.apache.jasper.tagplugins.jstl.core.TestSet`,
`org.apache.jasper.TestJspCompilationContext`,
`org.apache.catalina.core.TestStandardContextResources` — plus
`jakarta.el.TestCompositeELResolver` failing downstream with `expected:<200>
but was:<500>`, consistent with the same JSP-compilation 500 surfacing
through a different assertion.

None of these were in the 2026-08-14 non-passed census
(`known-issues/tomcat/nonpassed-class-census.md`), which ran after this fix
landed — so the fix held for at least that one run and regressed sometime
between 2026-08-14 and today. Not bisected.

**Next step:** re-run the original isolation table (below, "How it was
isolated") against current `dev` on `TestEncodingDetector` — if
`CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` still clears it, the family is the same;
if not, this is a related-but-distinct spill-cursor bug in a newer code path
and needs its own isolation.

---

# Original record (2026-08-06), preserved below

| | |
|---|---|
| **Status at the time** | ✅ FIXED — 2026-08-06, `jit/src/x64/bytecode_walk.rs` (both direct-call arms) |
| **Severity** | high — any test that compiles a JSP was at risk; hit correctness (500s) and throughput (classes that retried/recompiled ran far longer) |
| **HotSpot** | PASS on every class checked |
| **CratonVM** | FAIL/HANG before the fix, reproduced deterministically; PASS after |
| **Discovered** | 2026-08-06, complete 651-class Tomcat suite rerun after merging `dev` (~1044 commits) |
| **Root cause** | NOT GC, NOT ECJ, NOT `java.util.Stack`: the x64 single-pass backend pushed a raw JIT-to-JIT call's return value from an inflated spill cursor, so it landed `n` operand-stack slots too deep |

## Symptom (as reported)

Jasper's JSP-to-`.java`-to-`.class` pipeline uses Eclipse's ECJ
(`org.eclipse.jdt.internal.compiler`) as its Java compiler backend. ECJ
maintains its own operand-stack simulation while generating bytecode
(`OperandStack`, a plain Java class). That bookkeeping tripped ECJ's own
assertion:

```
SEVERE [http-nio-...] org.apache.catalina.core.StandardWrapperValve.invoke
Servlet.service() for servlet [jsp] in context with path [/test] threw exception
[java.lang.AssertionError: Unexpected operand at stack top] with root cause
java.lang.AssertionError: Unexpected operand at stack top
	at org.eclipse.jdt.internal.compiler.codegen.OperandStack.pop(OperandStack.java)
	at org.eclipse.jdt.internal.compiler.codegen.CodeStream.fieldAccess(CodeStream.java:1368)
	...
```

The `CodeStream` call site varied by run (`fieldAccess`, `pop`, `areturn`),
which read as "not one fixed bytecode shape". It was one fixed shape: every
one of those call sites funnels into the **same** ECJ method,
`OperandStack.pop(OperandCategory)`. Each occurrence surfaced to the HTTP
client as a 500 from the servlet container's exception-to-status handling.

## Root cause

`OperandStack.pop(OperandCategory)` compiles to:

```
 0: aload_0
 1: invokevirtual pop()                      -> TypeBinding
 4: astore_2
 5: aload_2
 6: getfield      TypeBinding.id : I         <- operand-stack slot 0
 9: invokestatic  TypeIds.getCategory:(I)I   <- consumes slot 0, produces slot 0
12: invokestatic  $SWITCH_TABLE$…:()[I
15: aload_1
16: invokevirtual OperandCategory.ordinal:()I
19: iaload
20: tableswitch { 1: 44, 2: 48 }             <- branch: targets re-derive depth
44: iconst_1                                 -> slot 1
62: if_icmpeq …                              <- compares slot 0 against slot 1
   … else: new AssertionError("Unexpected operand at stack top")
```

`TypeIds.getCategory` is small and hot, so the JIT eagerly compiled it and
baked a raw `E8` CALL to its entry (`direct_jit_callee_calls_enabled`). In the
x64 single-pass backend a direct-call site that *also* carries `invoke_info`
reserves a cold-deopt copy of the arguments through
`reserve_direct_call_service_slots`. That range has to sit **above** the
argument slots `pop_stack` just handed back — they are still live sources for
`emit_stack_arg_setup` — so reserving it moves `next_spill_offset` past them
(see `jit-direct-call-arg1-clobbered-by-arg0-FIXED.md`, the fix that put it
there). The return value was then pushed from that moved cursor.

The dispatch-helper arm immediately below both direct-call arms already
reclaimed the cursor for exactly this reason ("Restoring to pre_pop left the
return value parked n slots above its semantic depth"). The two direct-call
arms never did.

Inside a basic block the shift is invisible: the linear walk writes and reads
the same shifted slots and computes the right answer. It becomes wrong code at
the **first branch target after the call**, whose depth is re-established from
the bytecode. Here the `tableswitch` at bci 20 is that merge, so the compiled
`if_icmpeq` read the true slot 0 while the call had written slot 2:

```
   6f7: mov [rbp-50h],rax     ; slot 0 <- TypeBinding.id      (getfield)
   6fb: mov r11,[rbp-50h]     ; service-arg copy, base 0x58 …
   6ff: mov [rbp-58h],r11     ; … which leaves the cursor at 0x60
   703: mov rdi,[rbp-50h]     ; arg
   7f3: call 79e213d4a000     ; TypeIds.getCategory
   8f1: mov [rbp-60h],rax     ; result -> slot 2   <-- WRONG, must be slot 0
   …
  1069: mov [rbp-58h],rax     ; iconst_1 at a branch target -> slot 1 (correct)
  14dd: mov rax,[rbp-50h]     ; if_icmpeq reads slot 0 …
  14e1: mov rcx,[rbp-58h]     ;                    … and slot 1 (correct)
  14e5: cmp eax,ecx
```

So the compiled body compared **`TypeBinding.id`** against the expected
category instead of **`TypeIds.getCategory(id)`**. `OperandCategory.ONE`
expects `1`; almost every real `TypeBinding.id` is not `1`, so once
`OperandStack.pop(OperandCategory)` tiered up, essentially every JSP compiled
afterwards in that JVM threw. That is why the failures cluster at the end of a
class's run and why a class hit it 4–10 times rather than once.

## Fix

`jit/src/x64/bytecode_walk.rs`, both direct-call arms (`invokestatic`, and
`invokevirtual`/`invokespecial`): capture the spill cursor right after the
arguments are popped and restore it after `emit_post_invoke_exception_check`,
before the result is pushed — the same reclaim the dispatch-helper arm
performs. Handing the reserved range back is safe: its only consumer,
`emit_inline_callee_deopt_check`, is emitted just above.

The IR (C2) lowerer is not affected: it marshals into a fixed
`args_stage_top_off` staging region and allocates the call's result slot from
the SSA allocator, never from a spill cursor. The aarch64 backend has no
equivalent reservation.

**Regression test:**
`jit/src/x64/tests.rs::direct_call_result_slot_is_independent_of_service_arg_reservation`
compiles a direct call whose reference result is live across a following
safepoint, with and without `invoke_info` on the direct-call site, and asserts
the recorded oop-map slot is the same. Negative control (fix reverted, test
kept): `left: [72] right: [56]` — exactly the two-slot shift above.

## How it was isolated

Every step below is a real measurement on `TestEncodingDetector`
(Azure Linux host, real JDK 25, `asserts` = occurrences of the string in the
run's stderr). Base repro is deterministic: **5 failures / 10 assertions**,
reproduced in every base arm.

| Arm | Result |
|---|---|
| base (default) | 5 failures, 10 asserts |
| `--nojit` | **OK (22 tests)**, 0 asserts |
| `CRATONVM_JIT_DENY=org/eclipse/jdt` | OK, 0 (A-B-B-A against base: 0 / 10 / 0 / 10) |
| `…/codegen/` | OK, 0 |
| `…/ast/` | 5 failures, 10 |
| `…/lookup/` | OK, 0 |
| `codegen/OperandStack` | OK, 0 |
| `lookup/TypeIds` | OK, 0 |
| `lookup/TypeBinding` | 5 failures, 10 |
| `codegen/CodeStream` | 5 failures, 10 |
| `TypeIds.getCategory` | OK, 0 |
| `OperandStack.pop` | OK, 0 |
| `OperandStack.push` (control) | 5 failures, 10 |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | OK, 0 |
| `CRATONVM_TIER_ENABLED=0` | OK, 0 |
| `CRATONVM_JIT_IR_CALL=0` | 5 failures, 10 |

Two independent denies (`OperandStack.pop` **and** `TypeIds.getCategory`) each
cleared it — the signature of a defect in the *edge* between two compiled
bodies, not in either body. `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` confirmed the
edge; `CRATONVM_JIT_IR_CALL=0` ruled out the IR tier. `CRATONVM_DBG_JIT_COMPILED`
showed `TypeIds.getCategory` was the *only* compiled callee of
`OperandStack.pop(OperandCategory)` (`$SWITCH_TABLE$…` and `Enum.ordinal` never
compiled), and `CRATONVM_DBG_JIT_DISASM='OperandStack.pop,TypeIds.getCategory'`
produced the machine code quoted above.

A hand-written standalone reproducer with the identical bytecode shape
(compiled *with ECJ* so the `$SWITCH_TABLE$` + switch-expression form matched
byte for byte) did **not** reproduce, even though `CRATONVM_DBG_JIT_COMPILED`
confirmed the same three methods compiled: the standalone direct-call site
carried no `invoke_info`, so it never reserved the service-argument range. The
disassembly of the real run, not the reproducer, is what found this.

## The moving-young GC lead — real correlation, not the cause

The original report noted that every occurrence was preceded by
`cratonvm_gc::gc_quiescence [moving-young] fallback #N` warnings, and suspected
a stale/aliased reference into ECJ's `OperandStack` array
(see `gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED.md`).

That lead is **disproven as the cause**, though the correlation was genuine:
both come from the same feature. The fallback reason on these runs is
`innermost-rbp-belongs-to-unguarded-callee` — the GC declining to trust a
precise root map when the innermost frame was entered by a raw JIT-to-JIT call
it cannot decode. That is the **fail-closed safety net** working (it downgrades
to a non-moving sweep), and the same raw-call feature is what carried the
codegen defect. Evidence it is not the cause:

* the fixed binary still emits a moving-young fallback on the same class
  (`reason=unregistered-jit-frame-on-stack`) and returns `OK (22 tests)`;
* the failure is an `int`-vs-`int` comparison, not a reference deref;
* no GC knob was needed to clear it — every clean arm above is a JIT arm.

`java.util.Stack` was also cleared explicitly: a 200 000-round probe of
push/pop/peek/size/clone aliasing on the same binary returns `STACKPROBE OK`
on both CratonVM and HotSpot.

## Verification

Fixed binary, `--java-home` real JDK 25, `apps/tomcat` fixture. All seven
classes the report named, plus the two it named as unaffected:

| Class | Before | After |
|---|---|---|
| `jasper.compiler.TestEncodingDetector` | 5 failures, 10 asserts | **OK (22 tests)**, 0 |
| `jasper.compiler.TestJspDocumentParser` | 6 failures | **OK (22 tests)**, 0 |
| `jasper.compiler.TestParser` | 7 failures | **OK (16 tests)**, 0 |
| `jasper.TestJspCompilationContext` | 1 failure | **OK (5 tests)**, 0 |
| `catalina.core.TestStandardContextResources` | 1 failure | **OK (4 tests)**, 0 |
| `jasper.compiler.TestGenerator` | timed out (hit 10×) | **OK (85 tests)**, 0 |
| `jasper.compiler.TestJspConfig` | hit 4× | **OK (17 tests)**, 0 |
| `jasper.compiler.TestValidator` | (not affected) | **OK (11 tests)**, 0 |
| `jasper.optimizations.TestELInterpreterTagSetters` | (not affected) | **OK (48 tests)**, 0 |

A-B-B-A interleaved on the same host, fixed vs. pre-fix binary:
`fix 0 / base 10 / base 10 / fix 0` assertions.

Unit suites after merging current `dev` (`CARGO_TARGET_DIR` isolated,
debug profile): `cratonvm-jit` 1969 passed / 0 failed, `cratonvm-gc` 979 / 0,
`cratonvm-vm` 2444 passed / 3 failed — the same 3 failures a pristine
`origin/dev` worktree produces
(`runtime_error_array_index_carries_index`,
`no_unallowlisted_metadata_table_bypass_exists`,
`the_allowlist_has_no_dead_rows`), so none of them is this change.

## Residual

None for this defect. The wall-clock of `TestGenerator` (579 s) and
`TestStandardContextResources` (677 s) on a loaded shared host is the
pre-existing embedded-server deployment throughput wall
(`known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md`), not
compile retries — both now report `OK` with zero assertions, which is what the
retry hypothesis in the original report predicted would happen once the
corruption was gone.
