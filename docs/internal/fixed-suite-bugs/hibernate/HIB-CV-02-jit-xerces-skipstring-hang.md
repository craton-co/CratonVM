# HIB-CV-02 — JIT miscompile of Xerces `XMLEntityScanner.skipString` hangs all XSD/XML-schema parsing

**Severity:** High (hangs every test that compiles an XSD: XML-mapping bootstrap, `LocalXmlResourceResolverTest`, …)
**Status:** ✅ **RESOLVED on `dev` (2026-06-14, worktree `fix/hibernate-open-bugs`) — the provisional `skipString` JIT ban was REMOVED.** The underlying infinite-loop miscompile is no longer reproducible on current `dev`; the original `WORKAROUND` ban was stale and is now deleted (`vm/src/jit/skip_list.rs`).
**Binary:** `C:/craton/CratonVM-hibsuite/target/release/cratonvm.exe` (original); fix verified on `C:/craton/CratonVM-hibopen/target/release/cratonvm.exe`
**HotSpot:** not affected (parses the same XSDs in well under a second)

## ✅ Resolution (2026-06-14)

Re-verified on an idle host with the **actual** failing test and the **real** XSD-bootstrap code path (not a micro-replica):

| check | result |
|-------|--------|
| `LocalXmlResourceResolverTest` (the report's witness), JIT-on, **skipString ban removed**, rebuilt binary | **23/23 PASS** (`found=23 started=23 ok=23 failed=0`, ms≈384k) |
| `XmlRepro` (real `LocalXmlResourceResolver.resolveEntity` → `MappingXsdSupport.<clinit>` builds all ~15 XSDs), JIT-on, unbanned, watchdog off | **completes** in ~76 s (no infinite loop) |
| same, skipString **banned** vs **unbanned** vs `--disable-jit` | all three **complete** — ban makes no correctness difference |
| `Rep5` — faithful **instance** `skipString` (full method: `arrangeCapacity` guard + identical loop + `checkEntityLimit` tail) **forced through the JIT** via a static driver | compiles (`len=1789`) and runs **correctly** (`ok=2000000`) |
| bt18 regression after removing the ban | checksum **68332206** == HotSpot (no JIT regression) |

**What the original report got wrong:** the watchdog stack-dumps that looked like a "hang in `skipString`" (and later in `XSDFACM.buildDFA`/`CMStateSet.hashCode`) were **slowness, not an infinite loop** — `MappingXsdSupport.<clinit>` compiles ~15 XSDs and at interpreter speed exceeds the 120 s watchdog, so the watchdog fired mid-parse on whatever method happened to be running. With the watchdog **off**, every variant completes. The genuine infinite-loop miscompile from the report's era was fixed on `dev` by intervening JIT codegen work (the bug-H/I exception-routing + bug-24 inline-cache-slot fixes) — independent of this ban.

**Diagnostic method that cracked it:** run the *named* test through `CratonRunner` with `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` (so a true hang spins forever but mere slowness still finishes), and force a faithful **instance** method through the JIT via a **static driver** (an instance method called from a single hot loop never reaches the interpreter's per-callee compile counter — `invokevirtual` from one site OSRs the caller and bypasses `increment_invocation`; a static driver compiles and its `invokevirtual` triggers callee-compilation, the same way real Xerces callers do).

**Change:** removed the `("com/sun/.../XMLEntityScanner","skipString")` arm from `is_known_miscompile` in `vm/src/jit/skip_list.rs`. The only residual is JIT-helper *slowness* (≈384 s for the 23-test class) — a separate, tracked perf concern, not a hang; the per-method JUnit 120 s timeout is not exceeded.

---

## Deep investigation log (2026-06-13, superseded by the resolution above)

## Deep investigation (2026-06-13, worktree `fix/hibernate-open-bugs`)

Goal: real repro + codegen fix. Outcome: **strong narrowing of the defect, but the fix is blocked** by two concrete obstacles (below). Findings, in order:

### 1. Exact bytecode shape of the loop (JDK 25 `XMLEntityScanner.skipString(String)`)

```
40: aload_1; iload 5; iinc 5,-1; invokevirtual String.charAt   // c1 = s.charAt(j--)
49: aload_0; getfield fCurrentEntity; getfield ch; iload 4; caload   // c2 = this.fCurrentEntity.ch[i]
59: if_icmpne 120          // mismatch → shared return-false block
62: iload 4; iinc 4,-1; iload_3; if_icmpne 40   // back-edge: while (i_pre != position)
71: …success tail (position += len; columnNumber += len; checkEntityLimit)… ; return true
120: iconst_0; ireturn
```
Two **down-counting** induction vars `i`(local 4)/`j`(local 5), each `iload N; iinc N,-1` (use-pre-decrement), loop-exit via `if_icmpne` against `position`. The hang = the `i` decrement is lost in the compiled loop, so `i_pre` never equals `position` and the loop spins forever.

### 2. The counted-loop / BCE optimizer is NOT the culprit (ruled out by static analysis)

`jit/src/x64.rs::find_induction_variable` only recognises an IV modified by **`iinc local, 1`** (exactly +1) — a `-1` decrement is rejected (Priority-1 requires `inc == 1`). `analyze_loop_bound` only matches `if_icmpge/gt/lt/le` (0xa1–0xa4); our exit is `if_icmpne` (0xa0). So speculative BCE and the counted-loop transforms **do not engage** for this loop. The defect is in the **baseline single-pass bytecode→x64 emission** of `iload N; iinc N,-1; … if_icmpne`, i.e. an induction-variable register/spill-coherence bug, almost certainly when the surrounding method's register pressure spills `i` across the in-loop `charAt`/field-chain accesses.

### 3. Standalone repros built (`/c/craton/scratch/hib02/`)

- `Rep2.java` — loop **byte-identical** to `skipString`'s (verified via `javap`), instance-field `char[]` reached via the same `getfield fCurrentEntity; getfield ch` chain.
- `Rep3.java` — full-method mirror: `arrangeCapacity(len,false)` guard call before the loop, the identical loop, and the success tail (`position`/`columnNumber` update + `checkEntityLimit` call).

Both run **correctly** under CratonVM JIT-on at all counts tried (up to 1.2M calls) and match HotSpot — because **their `skipString`/`skipRep` is never actually JIT-compiled** (see obstacle A). The earlier `Rep2` "crash at 2M" was the OSR-compiled **`main` outer loop**, not the inner method — a *different*, GC/safepoint-correlated defect, not this hang. The original report's note ("a faithful standalone replica does not reproduce") is confirmed and explained: the loop in isolation compiles fine; the trigger is the full method's register allocation under JIT.

### Obstacle A — `skipString` will not JIT-compile in any isolated harness

Across `Rep2`, `Rep3`, and the real `SchemaFactory.newSchema` path (using the actual 165 KB `mapping-8.0.xsd`), with `CRATONVM_JIT_THRESHOLD` as low as 5 and up to 1.2M calls, **`skipString` is never even *attempted* for compilation**: `CRATONVM_DBG_JITC=1` shows 0 `scan-bail`, 0 `compile-bail`, 0 `upgrade-FAIL`, 0 `callee-compile`, and `CRATONVM_DBG_JIT_DISASM=skipString` dumps nothing. Other Xerces methods (`CMStateSet.equals`, `XSElementDecl.equals`, …) and JDK methods (`WeakHashMap.indexFor`, `Arrays.fill`) **do** compile in the same runs. So the compile **trigger never fires for `skipString` from a single hot call site** — strongly suspected: the inline/invoke cache plus quick OSR of the driving loop routes all calls through compiled-caller code, bypassing the interpreter's per-callee `increment_invocation` counter (interpreter.rs:13501). In the **real Hibernate suite** `skipString` is reached from many interpreted scanner call sites during warmup, so it crosses the threshold and compiles → hangs. Reproducing it therefore needs the full suite (or a harness that defeats the inline-cache short-circuit), not a tight micro-loop.

### Obstacle B — machine saturation by a concurrent session

Throughout this session the host ran at **100% CPU with 8 concurrent `cratonvm.exe` processes from another session's suite run** (`timeout 180 …/CratonVM-hibsuite/…`, `…/CratonVM-sbsuite/…`). This starves the background JIT compiler and makes every timing/`rc` measurement unreliable (identical invocations returned `rc=0`/`124`/`127` on different runs; a single real-XSD compile took 14 s). Do **not** `taskkill /IM cratonvm.exe` globally here — it kills the other session's child VMs (and vice-versa). Re-run this investigation when the host is idle.

### Diagnostic tooling added (worktree, env-gated, default-off)

`vm/src/jit/skip_list.rs`: `CRATONVM_UNBAN_SKIPSTRING=1` lifts **only** the `skipString` known-miscompile ban (every other entry, e.g. `Arrays.fill`, stays banned) so the defect can be reproduced + disassembled in isolation without a rebuild and without the coarse `CRATONVM_JIT_ALLOW_PACKAGES=java/util` (which lifts *all* known-miscompiles and instead reproduces the unrelated `Arrays.fill` OSR hang). Marked diagnostic-only; remove when the codegen fix lands.

### Concrete next steps (for an idle host)

1. Run the **actual** Hibernate XSD path under load that compiles `skipString` — either the real test (`LocalXmlResourceResolverTest` / `MappingXsdSupport.<clinit>`) with `CRATONVM_UNBAN_SKIPSTRING=1 CRATONVM_DBG_JIT_DISASM=skipString`, or a harness that calls it from ≥`JIT_THRESHOLD` *distinct interpreted* call sites before any caller OSRs.
2. Capture the emitted x64 for `skipString`; inspect the `iinc 4,-1` lowering and whether the value feeding the back-edge `if_icmpne` is the register holding `i` **before** or after the decrement, and whether a spill slot for `i` is updated across the in-loop `charAt` call.
3. Fix the baseline IV lowering; verify the XSD compile completes JIT-on, then `bt18 == 68332206` + pool regression before removing the ban + the `CRATONVM_UNBAN_SKIPSTRING` toggle.

---

## Symptom

## Symptom

Any code path that calls `javax.xml.validation.SchemaFactory.newSchema(...)` hangs forever
under CratonVM with the JIT enabled. In Hibernate this fires from
`org.hibernate.boot.xsd.MappingXsdSupport.<clinit>` (which eagerly builds ~15 `XsdDescriptor`s,
each compiling its `.xsd`), so the **static initializer never completes** and the calling
thread spins at 100% CPU. `--stack-dump-on-timeout` shows:

```
MappingXsdSupport.<clinit>
 └ LocalXsdResolver.resolveLocalXsdSchema
   └ SchemaFactory.newSchema
     └ com/sun/org/apache/xerces/internal/impl/xs/XMLSchemaLoader.loadGrammar
       └ … XSDHandler.parseSchema … SchemaParsingConfig.parse …
         └ XMLDocumentFragmentScannerImpl.scanDocument / scanEndElement
           └ com/sun/org/apache/xerces/internal/impl/XMLEntityScanner.skipString   ← spins here (pc 53↔56)
```

## Root cause

`XMLEntityScanner.skipString(String)` is a backward character-compare loop:

```
int i = position + len - 1;   // index into the scanner char[] buffer, counts DOWN
int j = len - 1;              // index into the target String, counts DOWN
do {
    if (s.charAt(j--) != buf[i]) return false;   // iload-then-iinc(-1) on both vars
    int prev = i; i--;
} while (prev != position);                       // exit via if_icmpne against `position`
return true;
```

JIT-compiled, this loop **never terminates** — the loop-exit `if_icmpne` against the bound and/or
the `iload`-then-`iinc(-1)` induction-variable update is miscompiled, so the spin condition never
becomes false. Confirmed:

- `CRATONVM_DISABLE_JIT=1` → the XSD compiles and the test passes.
- `CRATONVM_JIT_BISECT_SKIP=com/sun/org/apache/xerces/internal/impl/XMLEntityScanner.skipString` → also fixes it.
- A faithful standalone replica of the loop does **not** reproduce, so the trigger is specific to
  Xerces' exact basic-block shape (other interleaved field loads / the `arrangeCapacity` guard).

This is the same "JIT'd scan/fill loop never returns" family already recorded in the skip list
(`NETTY.1` = `Arrays.fill([BB)V`, the HashMap hot-loop entries, etc.).

## Fix (workaround)

Added a targeted entry to `vm/src/jit/skip_list.rs` `is_known_miscompile`:

```rust
| ("com/sun/org/apache/xerces/internal/impl/XMLEntityScanner", "skipString")
```

The method now runs in the interpreter; XSD compilation completes (≈correct, just slower).
Other Xerces scanner methods may share the defect; if more XML-parse hangs surface, widen to a
`com/sun/org/apache/xerces/` package ban (XML parsing is never a benchmarked hot path).

## Follow-up

A proper codegen fix needs the loop isolated. The defect resisted a faithful synthetic replica, so
the next step is a Xerces-shaped repro (real `char[]` buffer + `arrangeCapacity` guard + the exact
two-countdown block) driven past the JIT threshold, then a diff of emitted code vs. the interpreter.

---

## XSD-path *slowness* analysis (2026-06-14) — separate from the (resolved) hang

After the hang was resolved, the XSD compile path remained ~200x HotSpot (a single
165 KB `mapping-8.0.xsd` compile: HotSpot ~67 ms, CratonVM ~13 s; JIT-on was even
slightly *slower* than JIT-off). Root-caused with a Java sampling profiler
(`--stack-dump-on-timeout=N` fired across many time-points, aggregating the deepest
Java frame per dump):

- **~100% of self-time was in `hashCode`** — `XSElementDecl.hashCode` (~67–91%) and
  `CMStateSet.hashCode` (~9–33%), called from `SubstitutionGroupHandler.getSubstitutionGroup`
  → native `HashMap.get` (`native_map_get`, one `key.hashCode()` per get — same call count
  as HotSpot, *not* an O(n²) bug).

Two layered causes, each making a hashCode call ~1000x slower than HotSpot's inlined version:

1. **`String.hashCode` did not cache in real-JDK mode.** The essential-path registration used a
   non-caching closure (re-decode + re-fold every call); the caching `native_string_hash_code`
   (reads/writes the JDK `String.hash` field) was only wired in `register_synthetic_overrides`,
   which `--java-home` never calls. 5M-call microbench: **17.6 s vs HotSpot 9 ms** (~1950x).
   `XSElementDecl.hashCode` calls `String.hashCode` twice (fName, fTargetNamespace) per call.
   **FIXED** (commit `d2ba8640`, merged to `dev`): wire the caching impl into the essential path
   + cache the `hash`-field slot in a `OnceLock` + check the cache first. Bounds-safe
   (`set_field` rejects undersized objects → just doesn't cache). bt18 `68332206` == HotSpot;
   String.hashCode matches HotSpot across ASCII/URIs/accents/surrogates/HashMap. Microbench
   17.6 s → 14.7 s. **Modest end-to-end** because of cause #2.

2. **`update_root_snapshot` walks the entire Java frame stack on every native/intrinsic return**
   (`native_return_pushed_to_stack`), ~3 μs/call — the dominant per-call cost for any leaf
   method (hashCode/length/...) called millions of times in a hot loop, regardless of caching.
   This is the real remaining gap.

### Attempted (and rejected) big-win: skip the snapshot for pure intrinsics

A `String.hashCode` interpreter intrinsic that **skips `update_root_snapshot`** for pure,
no-Java-alloc, primitive-returning leaf intrinsics gave a large speedup (microbench 17.6 s → 4.5 s
**3.9x**; XSD `XsdReal N=3` 41 s → 21.6 s **1.9x**; bt18 golden). **But it is multi-thread-unsafe
and was REVERTED:** another thread's STW young GC reads this thread's *published* `root_snapshot`,
and skipping the per-call refresh leaves objects pushed onto the operand stack since the last
refresh invisible to that collector → reclaimed-while-live → silent `rc=1` crash. bt18 is
single-threaded (the only thread IS the collector, uses a live scan) so it never exposed it; the
multi-threaded `LocalXmlResourceResolverTest` (H2 cleaner thread etc.) crashed at bootstrap.

### Safe future direction for the remaining gap (cause #2)

The per-native frame-stack snapshot is the bottleneck. A safe optimization must keep the snapshot
correct for cross-thread collectors. Options: (a) make `update_root_snapshot` incremental (only
re-scan frames whose operand stack changed since the last snapshot); (b) for native-`HashMap` with
**String keys**, compute the hash directly in Rust from the cached `String.hash` field without the
`invoke_virtual` callback at all (doesn't help the `XSElementDecl` key case, which is the XSD
hot one); (c) get `XSElementDecl.hashCode` (bytecode) to JIT-compile — it currently never does
because it's reached only via `invoke_virtual` from inside the native `HashMap`, which bypasses the
interpreter's per-callee invocation counter. Verify any change with bt18 (`68332206`) AND a
**multi-threaded** object-churn test, not just bt18.

---

## `update_root_snapshot` per-call cost — ✅ LANDED gated (2026-06-14, commit `c3628c68`)

**Resolved.** Implemented as opt-in `CRATONVM_ROOTSNAP_CACHE=1` (default-OFF), merged to `dev`.
The "hard part" below (stale-cache invalidation needing push/pop hooks) was solved **hook-free**
using the LIFO stack property: give each frame a per-instance `seq`; a frame still present at index
`k` with unchanged `seq` was never popped, so by stack discipline every frame *below* it has been
continuously frozen — its cached roots are exact. The snapshot reuses the cached roots up to the
deepest matching frame (excluding that frame itself, which may have briefly been top) and re-scans
only the churning top; `heap.collection_count()` gates GC-move/promote invalidation. **No push/pop/
exception hooks needed** — that was the insight that made it safe.

Verified on a quiet host: bt18/bt16 gate-ON == gate-OFF == HotSpot golden (68332206 / 14985902);
bt18 force-moving ON == OFF (67674804, the pre-existing under-count — cache changes nothing under
moving GC); **`LocalXmlResourceResolverTest` 23/23 multi-threaded gate-ON** (the exact test the
unsafe *skip* crashed); **~18% XSD speedup** (`XsdReal` N=4 avg 29.8 s → 24.3 s; rootsnap
0.93 µs → 0.32 µs/call). Default path byte-identical; default build pays only a cached-bool read per
frame creation. Default-OFF pending a wider multi-app soak before flipping default-ON.

NOTE: other sessions' interpreter perf work (`7224f53b`, `89a91f79`) had already cut rootsnap from
the 3.4 µs measured below to ~0.93 µs, so the realized XSD win (~18%) is smaller than the ~20-25%
projected on the older base — still a clean, correct, general win for every native-call-heavy
real-JDK workload.

### (original analysis — superseded by the landed implementation above)

Measured on `dev` (with `44db1836` allocation-free + the String.hashCode cache):

Measured on current `dev` (with `44db1836` allocation-free + the String.hashCode cache):
`CRATONVM_DBG_ROOTSNAP=1` on `XsdReal` shows **avg 3.4 μs/call, avg 14.6 frames deep**, called
millions of times — roughly **20-25% of XSD parse time**. `44db1836` removed the per-frame
allocation and `is_object_address` is already lock-free (the "triple-mutex" comment in
`update_root_snapshot` is stale — it's now a lock-free `region_bounds` scan), so the only remaining
cost is the **O(stack-depth) frame walk** itself (scan every frame's locals + operand stack on every
object-returning native return).

### Validated design (cache frozen frames; re-scan only the top)

The deep frames (`main` → `resolveEntity` → … → `getSubstitutionGroup`, ~12 frames) are **frozen**
during the entire hashCode-heavy phase — only the top 1-2 frames churn. So:

1. Per-frame `rs_cached: Option<(gc_gen, Vec<ObjectRef>)>` (default None).
2. `update_root_snapshot` (change sig to `&mut JvmThread` — all 6 callers already hold `&mut`):
   for frozen frames `0..n-1`, reuse `rs_cached` if its `gc_gen == heap.collection_count()`, else
   scan + cache; the **top frame `n-1` is ALWAYS scanned fresh and never cached**.
3. Invalidate a frame's cache when it has run bytecodes since it was cached (it became top, then
   re-froze). GC moves/promotions are covered by the `collection_count()` generation check.

Expected: rootsnap drops from ~3.4 μs to ~0.5 μs (scan 1-2 frames instead of ~14) → **~20% XSD
speedup**, and it helps EVERY native-call-heavy real-JDK workload (not just XSD).

### Why it is NOT landed (the hard part)

Step 3's "ran bytecodes since cached" invalidation is the trap. A frame's cache goes stale if the
stack **dips below it and recovers between two rootsnaps** (the frame was briefly top and executed
bytecodes). Detecting that needs a hook at frame push/pop, but:

- **Frame pushes are NOT centralized**: `push_frame_and_fire_entry` calls itself "the single
  chokepoint" but ~10 sites call `thread.frames.push` directly (jvmti, gc.rs, roots.rs,
  call_stack.rs, vm_init, vm.rs×2). Missing any one = a stale cache reused = **silent heap
  corruption** (this codebase's #1 hazard; exactly what the reverted snapshot-*skip* caused).
- Frame pops also occur on exception unwind, not only the normal return chokepoint.

Combined with the inability to verify under the current host contention (a concurrent session
monopolizes it — every full-suite run crashed `rc=1/127/139` from contention, not code), landing
this **safely** requires a clean host + careful enumeration of all push/pop paths. Shipping it blind
— even gated — risks the silent-corruption class, so it was **deliberately not committed**.

### Recommended landing procedure (clean host)

1. Implement gated **default-OFF** (`CRATONVM_ROOTSNAP_CACHE`), like the other risky-GC gates
   (`CRATONVM_SHADOW_STACK`, `CRATONVM_SELECTIVE_PROMOTE`).
2. For invalidation, prefer a thread-level **`min_frames_len_since_cache`** updated at the few pop
   sites + the `collection_count()` gen check, OR clear `frames.last().rs_cached` at ALL ~10 push
   sites — enumerate them exhaustively.
3. Verify: bt18 (`68332206`) default **and** `CRATONVM_DBG_FORCE_MOVING`; a **multi-threaded**
   GC + String-keyed-HashMap churn stress (the discriminator that catches the skip-class bug —
   bt18 is single-threaded and will NOT); microbench + `XsdReal`; then the full
   `LocalXmlResourceResolverTest` 23/23 on an idle host.
