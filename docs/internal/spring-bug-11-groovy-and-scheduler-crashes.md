# spring-bug-11: CratonVM crashes — Groovy runtime (×3) + async scheduler (×1)

| | |
|---|---|
| **Category** | **VM-CRASH** (rc=139 / abort) |
| **Modules** | spring-context, spring-scripting, spring-scheduling |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | verified on dev `0c904c04` (2026-06-21) |
| **Status** | 🟢 **VM-CRASH RESOLVED** (this ticket's subject). Verified on dev `0c904c04` (binary `b1011vm`, JDK 25, `--Xmx 2g`): **none of the 5 classes SIGSEGV / rc=139 any more** — they complete with a `RESULT` line (or hang), never abort. The crash was fixed by the bug-12 real `HashMap` layout fix (`87091dec`) plus the Groovy MH/indy subsystem (`7bb93483`,`702efcf7`,`ac84f0b8`,`31b09c7c`,`f58e1bf6`,`92b7bd80`,`560fa5a5`). **Residuals are non-crash and reclassified out of this ticket** (see the 2026-06-21 section): the Groovy *test suites* still fail/hang functionally (Groovy-runtime, deep), and the scheduler/reflective tests are correctness FAILs. |
| **Suggested owner** | handoff — Groovy-runtime functional residual (deep); scheduler = separate CompletableFuture/threading ticket |

## ★★★★★ VERIFIED 2026-06-21 (dev `0c904c04`, binary `b1011vm`, JDK 25, `--Xmx 2g`) — the SIGSEGV crash is GONE; what remains is non-crash and belongs to other tickets

Each of the 5 classes named in this ticket was re-run on a fresh dev build. **Not one
SIGSEGV / rc=139** — the rc=139 crash this VM-CRASH ticket exists for no longer
reproduces. The crash ticket is **CLOSED**. The remaining failures are functional
(Groovy runtime) or threading correctness, and are split out below.

| class | result | crash? | residual class |
|-------|--------|:------:|----------------|
| `scripting.groovy.GroovyScriptEvaluatorTests` | completes **0/8** (one run hung) | ✅ no | Groovy-runtime functional + flaky |
| `context.groovy.GroovyApplicationContextDynamicBeanPropertyTests` | completes **0/2** | ✅ no | Groovy-runtime functional |
| `context.groovy.GroovyBeanDefinitionReaderTests` | **TIMEOUT** (300 s) | ✅ no | Groovy-runtime **hang** |
| `scheduling.concurrent.SimpleAsyncTaskSchedulerTests` | completes **6/10** then 150 s timeout | ✅ no | CompletableFuture / threading |
| `expression.spel.support.ReflectiveIndexAccessorTests` | completes **4/5** | ✅ no | reflection access-control (1 assert) |

### Groovy cluster (3) — crash gone; functional failures + one hang remain (deep, HANDOFF)
The 3 Groovy classes no longer crash; they now run far enough to emit a `RESULT`
(or hang in compilation), exactly the "deep Groovy compiler internals × CratonVM"
residual the original analysis flagged for handoff.
- **Primary root cause — ANTLR ATN parse THROUGHPUT (same family as
  [[springrepos-extension-hang-jit-throughput-and-deep-recursion]]).** A standalone
  probe (`GScriptProbe`: `new GroovyScriptEvaluator().evaluate(new
  StaticScriptSource("return 3 * 2"))` — the isolated `groovyScriptFromString` path)
  returns `6` on HotSpot **instantly**, but on CratonVM **hangs and is aborted by the
  120 s stack-dump watchdog**. The watchdog stack dump pins the single `main` thread
  frozen in:
  `GScriptProbe.main → GroovyScriptEvaluator.evaluate → GroovyShell.parse →
  GroovyClassLoader.doParseClass → CompilationUnit.compile → GroovyParser.<clinit> →
  groovyjarjarantlr4 ATNDeserializer.deserialize → BitSet.get/<init>` — i.e. it never
  finishes **deserialising the ANTLR parser ATN** in `GroovyParser`'s static
  initialiser. This is the **same ANTLR ATN cold-path JIT-throughput problem** that
  doc tracks (Groovy bootstrap = "one-time ATN deserialize + metaclass ~55 s"; the
  assert/`BitSet`-heavy ANTLR code runs interpreted-slow). The 3 dev fixes there
  (`43f5fe03`/`05b9622a`/`2fbabc0b`) help but do **not** close it for this Groovy
  entry path. This also explains the **non-determinism**: when the slow ATN parse
  squeaks through before a timeout the test *completes* (with the class-gen failures
  below); when it doesn't, it *hangs* — `GroovyBeanDefinitionReaderTests`, the
  `GScriptProbe` standalone probe, and a `CRATONVM_DBG_NPE_STACK=1` rerun of
  `GroovyScriptEvaluatorTests` all timed out, while the plain
  `GroovyScriptEvaluatorTests` run completed 0/8.
- **Secondary, downstream class-gen failure (only when compilation proceeds):**
  `GroovyScriptEvaluatorTests` shows `NullPointerException: Cannot invoke
  "java.lang.Class.getPackageName()" because "c" is null` (3/8, + 1/2 in the
  dynamic-bean-property test) — the **defineClass-returns-null** signature, the *same*
  NPE shape documented for Gradle's `LookupClassDefiner` (`native-builtins/src/lib.rs`
  ~9285, `lookup_define::register_lookup_define_class`). Groovy's `GroovyClassLoader`
  defines the generated script class via a path that yields a **null `Class`**, so
  downstream `c.getPackageName()` NPEs; the rest are `ScriptCompilationException` +
  `AssertionFailedError`. **Fix direction:** (1) the ANTLR ATN throughput is the
  load-bearing item — pursue the springrepos cold-path work (let the ATN
  deserialize/sim methods JIT-compile); (2) separately, make the Groovy
  class-definition path (`ClassLoader.defineClass`/`defineClass1` →
  `define_class_via_full`, `native-builtins/src/classloader.rs`) return a real mirror
  for Groovy's generated classes the way the Gradle path was fixed.
- **`GroovyBeanDefinitionReaderTests` still HANGS** (300 s timeout) — same ANTLR
  parse-throughput hang; not closed for this class. Same handoff.

### `SimpleAsyncTaskSchedulerTests` — RECLASSIFY (not a crash; CompletableFuture/threading)
Completes **6/10**, no SIGSEGV (confirming the earlier "batch CRASH was a
mis-attribution"). Real failures: `submitCompletableCallable` /
`submitFailingCompletableRunnable` (awaitility 5 s `ConditionTimeoutException` — the
async stage never signals completion), `submitFailingCompletableCallable`
(`java.lang.Object: null` — an exceptionally-completed future whose cause renders as a
bare `Object`), plus a JUnit-platform `ClassCastException: Object cannot be cast to
…ThrowableCollector` and a 150 s run timeout under the scheduler's thread-pool load.
**This is a separate CompletableFuture/threading-correctness ticket** (cf.
[[spring-bug-04]]); the `Object→ThrowableCollector` CCE under heavy threading is the
same shape as the still-open register-invisible cross-thread JIT-root gap
([[jit-junit-discovery-reflection-corruption]] / precise-maps Stage B/C). **Not** a
VM-CRASH item.

### `ReflectiveIndexAccessorTests` — crash gone (was spring-bug-09 OOB)
Completes **4/5**, no SIGSEGV — confirms the SIGSEGV was fixed by [[spring-bug-09]].
The 1 fail (`nonPublicReadMethod`, bare `AssertionError`) is a reflection
access-control correctness nuance, not a crash. Drop from this ticket.

## ★★ PINNED (this session, worktree `fix/spring-bug-10-11`) — culprit is `HashMap$KeySpliterator.tryAdvance`, OSR + dup_x1, DETERMINISTIC (not GC, not canonicalize, not dup2)
Direct root-cause on a fresh build, every prior hypothesis tested and most **falsified**:

- **Crashing method NAMED:** added a JIT code-range→name registry (`CRATONVM_DBG_JIT_NAMES=1`,
  populated in `JitCache::put`, consumed by `crash_handler.rs`). The crash frame resolves to
  **`java/util/HashMap$KeySpliterator.tryAdvance(Ljava/util/function/Consumer;)Z`** — a **JDK
  method, NOT Groovy code**. Groovy/Spring just exercise it heavily (HashMap key iteration in
  metaclass/classload paths). The faulting deref is a `getfield` (`mov rax,[rax+0x40]`) on a
  garbage receiver, reached via the `current = tab[index++]` idiom (bytecode pc 64–78, **dup_x1
  at pc 71**).
- **It is `dup_x1` specifically, NOT `dup2`/`dup_x2`.** Added split gates: `CRATONVM_JIT_NO_DUP_X1`
  **removes** the SIGSEGV; `CRATONVM_JIT_NO_DUP_X2` does **not**. The whole "dup2 root" framing
  below is **wrong** — the load-bearing opcode is `dup_x1` (0x5A).
- **NOT the canonicalize / non-canonical-offset leak (H1 FALSIFIED).** Added
  `CRATONVM_JIT_DUPX_EAGER_CANON` (canonicalize_stack immediately after the dup_x1 rotate, killing
  the non-canonical offsets in place). It does **NOT** fix the crash. So the prior leading theory
  ("rotated offsets reach an un-canonicalized merge") is dead.
- **NOT GC-related.** `--Xmx 6g` (little/no GC) → identical crash, identical faulting value.
- **NOT loop unrolling.** `CRATONVM_DISABLE_UNROLL=1` → identical crash, identical value.
- **DETERMINISTIC garbage:** every run (any heap, unroll on/off) faults with
  `rax=rcx=0x3B9ACA00_2A869E70`, **byte-identical**, while the heap base varies run-to-run
  (`rbx=0x0204…/0x011D…/0x024A…`). So the bad receiver is **not** a corrupted/stale heap pointer
  (those would track the heap base) — it is a **constant**: the **high 32 bits of a reference get
  overwritten** (`0x3B9ACA00` = 1e9; not present anywhere in VM source), i.e. an **uninitialized /
  wrong frame-slot read** produced by `dup_x1`'s slot management in this method's specific branch
  structure. The low half (`0x2A869E70`) is a plausible reference low-word; the high half is junk.
- **The COMMON-path codegen is CORRECT.** Dumped the annotated OSR disasm
  (`CRATONVM_DBG_JIT_DISASM="tryAdvance"`; locals `L0(this)=r15 L1(consumer)=r14 L2(hi)=r13
  L3(tab)=r12 L4(key)=r12`). At pc 64–78 `r15=this` is used correctly for both putfields, `tab=r12`,
  the index flows through `[rbp-40]`/`[rbp-38]` correctly. tryAdvance never writes `r15`. So the
  steady-state code is fine — the defect is a slot-read on a specific iteration/branch path (or the
  OSR-entry / loop-peeling variant), not on the common path the disasm shows.
- **The dup_x1 IDIOM ALONE is fine.** `probe/OsrDupX1.java` reproduces the exact
  `this.current = this.table[this.index++]` bytecode (dup_x1 field-post-inc array index in an
  OSR'd loop) and **matches HotSpot** (rc=0). So it is the *interaction* of dup_x1 with
  tryAdvance's register pressure (5 locals, `tab`/`key` sharing r12) + branch structure, not the
  opcode in isolation — which is why the earlier minimal repros "matched HotSpot".

**Why this method only OSR-compiles:** it is an instance method whose only hot path is the inner
loop (pc 42–81), so it tiers up via the loop back-edge (OSR), never via invocation counting
(cf. memory `jit-instance-methods-no-invocation-tierup`). The earlier repros compiled via the
normal path and so never exercised the buggy OSR/loop variant.

**Remaining to land a fix:** instrument the JIT to log the frame-slot read that yields the
uninitialized value on the crashing path (needs a build), or single-step the OSR'd loop variant.
The fix is in `dup_x1`'s slot allocation/oop-map for the register-resident-`this` + frame-`index`
shape under OSR. Per-run mitigation: `CRATONVM_JIT_NO_DUP_X1=1` (cleaner than `NO_DUPX`; only
disables the load-bearing arm).

**SEPARATE bug found (not bug-11):** CratonVM's **interpreter** mis-handles
`HashMap$KeySpliterator.tryAdvance` — `probe/KSplProbe.java` throws
`internal error: expected object reference, got int(16)` at `arraylength` (pc 25) **even under
`--nojit`** (0 methods JIT-compiled). `int(16)` = the HashMap capacity, so a `getfield table:[Node`
returns the array *length* instead of the array — a distinct interpreter/native-HashMap field
defect worth its own ticket.

Probes/tooling added on `fix/spring-bug-10-11`: `probe/OsrDupX1.java`, `probe/OsrDupX2.java`,
`probe/KSplProbe.java`, `probe/dupx-matrix.sh`, `probe/pin-dupx.sh`; JIT flags
`CRATONVM_JIT_NO_DUP_X1`/`NO_DUP_X2`/`DUPX_EAGER_CANON`, `CRATONVM_DBG_JIT_NAMES`,
`CRATONVM_DBG_DUPX_METHODS`.

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

---

## RESOLUTION (2026-06-16) — the dup_x1/SIGSEGV crash is FIXED; "dup_x1 OSR miscompile" was a misdiagnosis

After merging dev + the **spring-bug-12** HashMap-view layout fix, the Groovy `EXCEPTION_ACCESS_VIOLATION`
**no longer reproduces**: `dupx-matrix.sh` (GroovyScriptEvaluatorTests) is `no-crash` under EVERY
variant — baseline, NO_DUPX, NO_DUP_X1, NO_DUP_X2, EAGER_CANON — and 3/3 fresh baseline runs are
crash-free (the crash was previously called "non-deterministic"; it is now simply gone).

**Root cause was bug-12, not dup_x1.** The crashing method is `HashMap$KeySpliterator.tryAdvance`,
whose hot idiom is `current = tab[i++]` (the dup_x1 site). bug-12 was that `getfield table` returned
the array **length** (synthetic-map layout put capacity at the real table slot) instead of the
`Node[]` reference — so `tab` was an `int`, and `tab[i++]` (`aaload`) ran on a garbage "array",
producing the wild deref / SIGSEGV. The dup_x1 *codegen is correct*: the focused
`spring-suite/probe/OsrDupX1.java` (the exact `this.current = this.table[this.index++]` idiom, driven
hot to OSR-compile) matches HotSpot bit-for-bit (`FINAL sum=4560000000`, rc=0). bug-12's real-HashMap
layout fixes `map.table` for BOTH interpreter and JIT, so the dup_x1 path no longer reads garbage.
The `CRATONVM_JIT_NO_DUP_X1` / `_NO_DUP_X2` / `_NO_DUPX` / `_DUPX_EAGER_CANON` gates remain as
bisection tooling but are not load-bearing.

**Residual (re-classified, NOT a crash):** the Groovy test now **hangs at `BEGIN`** (no test output
in 150 s) under **both JIT and `--nojit`** — i.e. a general Groovy compile/execute hang in
CratonVM's runtime (indy/MethodHandle/reflection/class-generation), independent of the JIT and of the
resolved SIGSEGV. This is the deep "Groovy compiler internals × CratonVM" issue the original analysis
flagged for handoff; it is a separate investigation from the bug-11 dup_x1 crash, which is closed.
