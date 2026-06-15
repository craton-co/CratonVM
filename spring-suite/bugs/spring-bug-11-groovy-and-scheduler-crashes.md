# spring-bug-11: CratonVM crashes — Groovy runtime (×3) + async scheduler (×1)

| | |
|---|---|
| **Category** | **VM-CRASH** (rc=139 / abort) |
| **Modules** | spring-context, spring-scripting, spring-scheduling |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 |
| **Status** | OPEN (inventory; needs per-cluster trace) |
| **Suggested owner** | handoff (Groovy runtime is deep) / me (scheduler) |

## Crash inventory (5 CRASH classes found so far, baseline @ ~1100/2930)
**Groovy cluster (3 — likely one root cause in the Groovy runtime under CratonVM):**
```
org.springframework.context.groovy.GroovyApplicationContextDynamicBeanPropertyTests
org.springframework.context.groovy.GroovyBeanDefinitionReaderTests
org.springframework.scripting.groovy.GroovyScriptEvaluatorTests
```
**Other:**
```
org.springframework.scheduling.concurrent.SimpleAsyncTaskSchedulerTests   (threading/scheduler)
org.springframework.expression.spel.support.ReflectiveIndexAccessorTests  (SIGSEGV fixed by spring-bug-09;
                                                                            re-test — should no longer crash)
```

## SHARPENED diagnosis (this session) — culprit is dup_x1/dup_x2, NOT dup2; codegen is correct
Direct investigation corrected and narrowed the earlier hypothesis:
- **The mitigation flag `CRATONVM_JIT_NO_DUPX=1` gates `dup_x1`(0x5A)/`dup_x2`(0x5B) codegen
  (`jit/src/x64.rs:987`), NOT `dup2`(0x5C).** Since NO_DUPX removes the Groovy SIGSEGV, the culprit
  is a **`dup_x1`/`dup_x2`-containing method**, not `dup2`. (`dup2_top_cat2` + the 0x5C arm are sound.)
- **The `dup_x1`/`dup_x2` codegen itself is provably correct** (`x64.rs:13318`/`13350`): copy via
  `push_from_rax` (always a frame slot, never volatile RAX) + `rotate_right` of slots AND oop-marks
  — verified correct even for the ref/int mix (`xBuf[xBufOff++]`-style: the int index duplicated
  below the `this` ref lands in the right slots with the right oop marks). `dup_x2` additionally
  bails unless the next op is a cat-1 array store.
- **Three minimal repros do NOT reproduce** (all match HotSpot under JIT): FORM-1 dup/dup2
  compound-assignment; `dup_x1` array post-increment; **`dup_x1` + field-post-increment array index
  + a call-boundary `canonicalize_stack` + ref read-back**. So the common shapes are correct.
- **Therefore the bug is a more exotic interaction:** most likely `canonicalize_stack`'s
  parallel-move of the *non-canonical rotated offsets* `dup_x1`/`dup_x2` leave (resolved at a
  branch/call boundary), for a Groovy-codegen-specific stack shape; OR a *co-located* JIT op in the
  same method that NO_DUPX masks by bailing the whole method to the interpreter. The crash is a
  getfield on a bogus receiver (small int as a pointer → `EXCEPTION_ACCESS_VIOLATION` at `[recv+0x40]`).
- **Pinning requires method-level JIT dumps** (`CRATONVM_DBG_DUMP_JIT="Class/method"`) on the
  crashing method — but the release-binary native symbols are unreliable, so the JIT owner should
  identify the method via the JIT method registry / a debug build and dump it to locate the
  `dup_x1`/`dup_x2` + the following op that mis-shapes the stack.
- **Working per-run mitigation:** `CRATONVM_JIT_NO_DUPX=1` avoids the SIGSEGV (forces those methods
  to interpret; small perf cost). NOT recommended as a default (the codegen is correct; this would
  regress the BC-digest/Nat hot paths) — the root (canonicalize parallel-move / co-located op) is
  the real fix.

## dup2 root — status after the landed JIT fixes (dev `334fe5e7`) → HANDOFF to JIT team
Re-tested on dev with instance-method tier-up default-ON + the wildfly JIT fixes (virtual-dispatch
BAIL, codePointAt spill, aastore-SATB): `GroovyScriptEvaluatorTests` **still SIGSEGVs**. A minimal
dup2 repro (int/long/array compound-assignment `obj.field op= v`, 2M hot iters) **matches HotSpot
exactly** — so the common dup2 paths are correct; the miscompile is a *Groovy-codegen-specific*
stack shape (the FORM-1/FORM-2 mixup at `jit/src/x64.rs:2180-2227`), not reproducible blindly.
`CRATONVM_JIT_NO_DUPX=1` removes the segv (confirms dup-family). **Recommendation: hand off to the
JIT team** (actively in this code) — pin via `CRATONVM_DBG_DUMP_JIT` on the crashing Groovy method.
A surface bounds-guard is NOT a fix (mine was reverted as net-negative). This is the single
load-bearing JIT defect behind the Groovy crash and the `AntPathMatcher`/`MergedAnnotations` residual
hangs.

## CORRECTION (dev `a3fdba43`): the getfield guard was REVERTED — net-negative
Follow-up testing showed the Layer A inline-getfield bounds guard (`486c93e1`) was a **regression**
and gave **no** Groovy benefit, so it was reverted (`4f2129e9`):
- It **hung** `StringUtilsTests` (passes 67/67 WITHOUT the guard, hangs WITH it) and others — the guard
  nulls a getfield whose JIT-computed `field_index >= num_slots`, but that read was *benignly
  returning the correct value* (the real defect is the upstream field_index/receiver-layout
  miscompile; the slot landed correctly). Nulling it → infinite loop.
- It did **not** fix Groovy: `GroovyScriptEvaluatorTests` SIGSEGVs **with and without** the guard
  (the crash just moved to another deref site when present).
- `AntPathMatcherTests` hangs **with and without** the guard → a *separate* JIT/perf issue, not the guard.

**The single load-bearing fix is the upstream JIT operand-stack / field_index miscompile**
(dup2-family) — it underlies the Groovy SIGSEGV, the StringUtils/AntPath getfield issues, and more.
Deep JIT work (needs `CRATONVM_DBG_DUMP_JIT` to pin the opcode; the parallel JIT-dispatch fixes on
dev are adjacent). A bounds guard is NOT the fix — it only masks/relocates symptoms.

## (superseded) getfield site hardened, Layer B fixed, dup2 root REMAINS
- **Layer A getfield guard (commit `486c93e1`, on dev):** eliminated the *inline-getfield-site*
  EXCEPTION_ACCESS_VIOLATION (memory-safety hardening, helper-parity). **But the Groovy crash is not
  gone — it MOVED:** with the guard, `GroovyScriptEvaluatorTests` still SIGSEGVs at a *different* pc
  (the dup2 operand-stack miscompile feeds the bad receiver to other unguarded deref sites too).
  Non-deterministic (ABEND vs SIGSEGV across runs). **The dup2 miscompile is the load-bearing fix
  and remains OPEN** (needs `CRATONVM_DBG_DUMP_JIT` to pin the opcode + rebuild iteration;
  `CRATONVM_JIT_NO_DUPX=1` / `DISABLE_INLINE_GETFIELD=1` are mitigations).
- **Layer B (`jrt:`):** fixed on dev (`560fa5a5`) — `--nojit` GroovyBugError gone.

## Groovy cluster — ROOT-CAUSED + FIXES STAGED (deep investigation)
**Layer A (SIGSEGV) — FIX STAGED.** The inline `getfield` JIT emitter (`jit/src/x64.rs` ~15065)
dereferenced the receiver with only a null check — **no `num_slots` bounds check** — while the
runtime helper `jit_getfield` (`vm/src/jit/helpers.rs:1637-1702`) was hardened with one ("B2" fix).
A JIT operand-stack miscompile (dup2-family) feeds a **garbage receiver** (`rax=0xC5…`, not a heap
ptr) into the inline getfield → wild OOB read at `rax+0x40` → `EXCEPTION_ACCESS_VIOLATION`.
`DISABLE_INLINE_GETFIELD=1` (routes through the bounded helper) deterministically avoids the crash
on all 3 Groovy classes. **Fix:** add the matching `CMP DWORD [RAX+16],field_index; JBE null-path`
guard inline (staged, mirrors the helper + the proven class-id-guard idiom in the same emitter). The
deeper dup2 operand-stack miscompile that supplies the bad receiver remains (needs a JIT method dump
+ rebuild — separate).

**Layer B (`--nojit` GroovyBugError) — ALREADY FIXED ON DEV.** `URL.openStream()` on a `jrt:` URL
threw "unsupported scheme: jrt:" in my (stale c5644da4) worktree, so Groovy's
`AsmDecompiler.parseClass` couldn't decompile `java.lang.Object` → its `ClassNode` was left
unresolved (`redirect==null`, not primary) → `addTypeAnnotation(@Generated)` tripped its
`isRedirectNode()||isResolved()||isPrimaryClassNode()` guard → "Adding type annotation @Generated to
non-redirect node: java.lang.Object". The dev tree already added the `jrt:` arm to `URL.openStream`
(`native-builtins/src/net_phase_e.rs`, commit `560fa5a5`). **Merging current dev into the worktree
brings this fix** — so the verification build covers both layers. (NOT the annotation/`Proxy$Instance`
defect — independent boot-resource loading.)

## Groovy cluster — (original) DIAGNOSED (two layers)
`GroovyScriptEvaluatorTests` traced precisely:
- **With JIT (default):** `EXCEPTION_ACCESS_VIOLATION (SIGSEGV)` at a **JIT return address** (CratonVM
  hs_err dump shows "Code bytes preceding JIT return addresses") — a JIT **miscompilation** while
  running Groovy's class-generation path.
- **With `--nojit`:** no crash, but all 8 tests FAIL with a **Groovy-compiler-internal** error:
  ```
  org.codehaus.groovy.GroovyBugError: BUG! exception in phase 'class generation' in source unit
  'Script1.groovy' Adding type annotation @Generated to non-redirect node: java.lang.Object
  ```

**Root cause (underlying):** Groovy's compiler adds a `@Generated` annotation to a `ClassNode` that
is a *non-redirect* node representing `java.lang.Object`. A non-redirect node means Groovy never
resolved/redirected that `ClassNode` to the real `Object` class — i.e. CratonVM's reflection /
`ClassNode` resolution (or annotation handling, cf. [[spring-bug-01]] — note it's literally about an
annotation, `@Generated`) feeds Groovy a malformed view of `java.lang.Object`. Under the JIT this
same path miscompiles → SIGSEGV.

**Assessment:** genuinely deep — Groovy compiler internals **×** CratonVM reflection/annotation **×**
a JIT miscompilation. Not a quick win. Two independent fixes needed (the JIT crash AND the
ClassNode-resolution/annotation feed). **Recommend handoff** (or a dedicated session), likely
gated on [[spring-bug-01]]. `--nojit` avoids the crash but not the failure.

## Notes / next steps
- The **Groovy** cluster: Groovy compiles + runs scripts via its own runtime + `invokedynamic`
  call-site caching, which is a heavy exerciser of CratonVM's MethodHandle/indy + reflection paths.
  Reproduce one (`GroovyScriptEvaluatorTests`) with `RUST_BACKTRACE=1` and
  `--stack-dump-on-timeout`/`CRATONVM_DBG_OOBFIELD` to localize. Likely a single Groovy-runtime
  defect gating all three. Deep — good handoff unless Groovy support is in scope.
- **`SimpleAsyncTaskSchedulerTests`**: in **isolation it does NOT crash** (rc=0, 4/19 pass) — it
  FAILs on `submitCompletableCallable` (result mismatch) + `submitFailingCompletableCallable` (NPE).
  Its batch "CRASH" was a mis-attribution (a neighbour in the batch crashed). Re-classify as a
  threading/CompletableFuture **correctness FAIL**, not a crash. cf. [[spring-bug-04]].
- **`ReflectiveIndexAccessorTests`**: its SIGSEGV was the EmptyMap OOB — fixed in [[spring-bug-09]]
  (`5941addd`); should now be at most a (separate) perf-hang, not a crash. Re-classify on the
  post-fix rerun.

Each crash is its own investigation + ~17 min rebuild. Prioritize by cluster size: Groovy (3) first.
