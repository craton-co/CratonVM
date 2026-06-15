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

## VERIFIED RESULT (on dev `bc1fa64f`): getfield site hardened, Layer B fixed, dup2 root REMAINS
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
