# Workstream 1 — JIT throughput: JIT-compiled call-heavy code is slower than the interpreter

**Status:** TRUE root cause found and FIXED (2026-06-12, see §0): the dominant
inversion mechanism was the per-native-call **conservative JIT-frame stack scan** in
`update_root_snapshot`, NOT the §2 dispatch-helper costs (those are real but
second-order — the MIC helper measured **0 calls** on the repro). Fix = per-thread
JIT-scan cache invalidated at Rust↔JIT boundaries. ConfigDefTest single-class repro:
interp 6 s / JIT-on 66-96 s before → **8 s** after (bisect-isolated case) and 21 s
with all compiles on. The remaining gap vs interpreter is the §6 codegen-quality
work. **Not a crash — a performance bug.**

---

## 0. TRUE root cause (2026-06-12) — per-native-call conservative JIT-frame stack scan

**Repro distilled** (single class, ~16× inversion): `RunCls
org.apache.kafka.common.config.ConfigDefTest` — `--nojit` **6 s**, JIT-on **~96 s**.
Bisect (`CRATONVM_JIT_BISECT_ONLY`/`_SKIP`) isolated ONE method whose compilation
flips it: `EngineExecutionOrchestrator.lambda$execute$0` — JUnit 5.10's
`withInterceptedStreams` wrapper lambda, whose body invokes the private
`execute(...)` that **runs the entire test plan**. Skipping just that lambda → 6 s.

**Why that lambda:** `try_lambda_dispatch` consults the JIT cache on *every* SAM
dispatch (no warmup gate), so the lambda runs compiled from call #1. Its compiled
frame then sits near the bottom of the native stack (a `JitEntryGuard` chain entry,
`gc_quiescence` active) for the whole run.

**The mechanism** (cdb stack sampling; all samples identical):
`invoke_cached_native_callback` / `safe_native_call` → `update_root_snapshot` →
`scan_active_jit_frames` → `scan_one_frame` → `GenerationalHeap::is_object_address`.
`update_root_snapshot` runs on **every object-returning native call** (twice: once
in `safe_native_call`, once in `native_return_pushed_to_stack`), and the
conservative scan range for a non-precise chain entry is `[scanner_sp, entry_sp]` —
i.e. from the *current* (deep) SP all the way up to the JIT entry near the stack
bottom. With the whole interpreted test plan recursing above that frame, every
native call paid an O(megabytes) word-by-word stack scan. Diagnostics that ruled
everything else out: `mic_calls=0` (MIC helper never entered), `disp_calls=15`
(dispatch helper near-cold), `gc_collections=0` **in both fast and slow runs**
(GC mode/frequency irrelevant — `CRATONVM_DBG_FORCE_MOVING` changed nothing),
identical intrinsic-dispatch counts (same Java work executed).

**The fix — per-thread JIT-scan cache** (`vm/src/jit/conservative_roots.rs`):
JIT spill slots can only change while compiled code executes, and control re-enters
Rust exclusively through a JIT runtime helper or the entry guard's drop. A
generation counter (`note_jit_boundary`) is bumped at every chain mutation
(push/pop/prune) and at the entry of **every** `jit_*` runtime helper; between
bumps, `scan_active_jit_frames` reuses the previous scan's roots verbatim.
Soundness: spills live in `[innermost-helper-entry SP, entry_sp]`, which every
cached scan covered; address stability holds because live JIT frames force the
non-moving sweep (the cache is disabled under `CRATONVM_DBG_FORCE_MOVING` /
`CRATONVM_SHADOW_STACK`, which lift that guarantee, and via
`CRATONVM_NO_JIT_SCAN_CACHE` for bisection).

**Follow-up (not done):** record the innermost helper-entry SP per chain entry so
even cache-miss rescans are bounded to the real JIT band instead of
`[scanner_sp, entry_sp]`; and consider a warmup gate for `try_lambda_dispatch`'s
eager compiled-entry use.

**New diagnostics** (all default-off): `CRATONVM_DBG_MIC_PROF=1` — dispatch-helper
path counters + rdtsc cycle totals, per-call `[DISP_TRACE]` callee/cycles lines,
and a shutdown `[MIC_PROF]` dump incl. `gc_collections`/`quiesce_depth`.

This is the analysis behind bug-01 and the "slow" side of bug-05/bug-06: many
kafka-clients unit packages *complete* but exceed CratonVM's **120 s default watchdog**
(`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` to lift it) because CratonVM is far slower than
HotSpot, and — critically — **slower with the JIT on than with it off**.

---

## 1. The headline numbers (corrected & validated)

Cleanest case: `org.apache.kafka.common.config` (114 unit tests, call/reflection-heavy).
All times are the **in-JVM `RESULT ms`** the harness prints (excludes JVM startup),
cross-checked against wall time. Machine had some concurrent load → ±20% noise.

| Configuration | time | notes |
|---|---|---|
| HotSpot 25 tiered (default) | **~0.8 s** (778/890/1045 ms over 3 runs) | compiles 2845 methods, but **async/background** |
| HotSpot 25 `-Xint` (interp only) | **~1.3 s** | still fast; tiered JIT only buys ~1.6× here |
| CratonVM `--nojit` (interp) | **~13 s** (12.2–14.8 s) | |
| CratonVM JIT-on (default thr=500) | **~227 s** | |

> ⚠️ **Measurement caveat that bit me:** an early note recorded HotSpot config as
> "~9 s". That was **wrong** (probably copied from the CratonVM `--nojit` column).
> The real HotSpot figure is ~0.8–1.3 s. Always compare the **in-JVM `RESULT ms`**
> line across VMs, run each ≥3×, and watch for background load. Don't trust a single
> wall-clock number.

### Two independent gaps

1. **JIT inversion — the catastrophe.** CratonVM JIT-on (227 s) is **~17–24× slower
   than CratonVM's own interpreter** (13 s). JIT must never be slower than the
   interpreter; HotSpot's JIT'd code is *faster* than its interpreter. CratonVM's is
   *slower* for call-heavy code. This is the bug to fix.
2. **Interpreter gap — secondary.** CratonVM's interpreter (13 s) is ~10–14× slower
   than HotSpot's (`-Xint` 1.3 s). Real, but a separate, lower-priority workstream.

### Why HotSpot doesn't have this problem
HotSpot is **interpreter-first with asynchronous (background) compilation**:
short-lived code runs interpreted; hot methods are compiled on a background thread and
the compiled entry is swapped in when ready. So the JIT can *only help* — it never
stalls or slows the running thread. CratonVM compiles **eagerly and inline**, and its
JIT'd call-heavy code then executes slower than interpreting it.

---

## 2. Root cause — **SUPERSEDED by §0**

> ⚠️ **2026-06-12 correction:** the attribution below is WRONG for the `config`
> workload. Instrumented counters show `jit_invoke_virtual_mic` is entered **0
> times** and `jit_invoke_dispatch` only ~15 times on the ConfigDefTest repro —
> the helpers are nearly cold. The dominant cost was the per-native-call
> conservative JIT-frame stack scan (§0). The per-call overheads described here
> are real and were also fixed (negative compile cache + lazy arg decode, see
> §6 item 1 status), but they are second-order for this suite.

The 227 s is spent **executing JIT-compiled code**, specifically virtual/interface
dispatch out of JIT'd call-heavy framework methods (JUnit `ReflectionUtils` /
`AnnotationUtils` / `Preconditions`, JDK reflection, `HashMap`, `Optional`, streams).

Ruled OUT by instrumentation (these are *not* the cause):
- **Compile time:** `CRATONVM_DBG_JITC` + timing → **0 of 101** compiles took >5 ms.
- **Recompile churn:** 101 first-compiles, **0** recompiles, **0** OSR.
- **Deopt thrashing:** `CRATONVM_DBG_DEOPT` → **0** deopt events.
- **Infinite loop:** with the watchdog off the run *completes* (227 s).

The actual cost, in `vm/src/jit/helpers.rs::jit_invoke_virtual_mic` (the runtime helper
the JIT calls on a virtual/interface site) — `helpers.rs:~2758`:

1. **Megamorphic MIC misses.** The helper's `JitMICSlot` is a **monomorphic** inline
   cache (one cached `class_id → entry`). The framework's dispatch sites are heavily
   **megamorphic** (>3 receiver types). They miss the MIC on essentially every call and
   fall through to **full `class_manager` lookup + `invoke_or_native` resolution per
   call** (`helpers.rs:~3116` onward — the "Cache miss: full resolution" path).
2. **Per-call marshalling.** *Every* call (hit or miss) allocates a `Vec` for the
   decoded argument `Value`s and re-parses the method descriptor (`DescriptorParamIter`)
   *before* the cache check — overhead the interpreter doesn't pay.
3. **No intrinsics in JIT'd code.** The interpreter has fast native **intrinsics** and
   a more forgiving polymorphic dispatch for exactly these hot JDK methods
   (`HashMap.get`, reflection, etc.); the JIT-compiled call sites route through the
   slower JIT→runtime dispatch instead. So interpreting wins.

Net: **JIT'd call-heavy code is slower than interpreted call-heavy code.**

---

## 3. The PIC is already implemented — and insufficient here

A polymorphic inline cache exists in full; this surprised me, so it's documented:

- **`JitPICSlot`** — `jit/src/lib.rs:~2787`. **3 entries** (`JIT_PIC_ENTRIES`), parallel
  `class_ids[3]` / `entry_ptrs[3]` / `needs_context[3]` / per-entry `hits[3]` + `misses`,
  LFU eviction in `install()`, `lookup(class_id) -> Option<(entry_ptr, needs_ctx)>`
  (`lib.rs:~2915`), `seed_from_mic()`.
- **Adaptive promotion thresholds** — `lib.rs`: `MIC_TO_PIC_THRESHOLD = 3`,
  `PIC_TO_MEGA_THRESHOLD = 20`.
- **Inline 3-way cascade emitted at the call site** — `jit/src/x64.rs:~18213`+
  (`pic_inline` path). At each `invokevirtual`/`invokeinterface`, codegen emits
  `cmp eax,[pic+CLASS_ID_OFFSETS[i]]; … call [pic+ENTRY_PTR_OFFSETS[i]]` for the 3
  slots, falling through to the helper on a 3-way miss. Offsets are hardcoded and
  `const_assert`-matched to `JitPICSlot::{CLASS_ID,ENTRY_PTR,NEEDS_CONTEXT}_OFFSETS`.
- **PIC slots are built per site** in `try_compile_inner` — `jit/src/lib.rs:~4389`:
  one MIC+PIC `Box` per `invoke_kind == 0` (invokevirtual / `0xb6`) or `== 2`
  (invokeinterface / `0xb9`). So both hot dispatch kinds get a PIC.

### Why the existing PIC doesn't save `config`
- The inline cascade is **gated on `pic_inline = pic_ptr.is_some() && args_fit`** and
  only dispatches inline when the cached entry is a **`needs_context == true`** callee
  (`x64.rs:~18258`). **Non-`needs_context` callees bypass the cascade** and go to the
  helper every call.
- The helper, on a MIC miss, originally went **straight to full resolution without
  consulting the PIC's 3 entries** (so it was effectively monomorphic for non-ctx /
  spillover traffic).
- Most importantly, `config`'s discovery dispatch is genuinely **>3-way megamorphic**.
  A 3-entry PIC **thrashes** (LFU evicts a live entry every round), so it cannot help
  no matter where it's consulted.

### Attempt (a), measured and REVERTED
I added a `pic.lookup(receiver_cid)` fast path in the helper's MIC-miss path (dispatch
directly on a PIC hit, before full resolution). Result:
- `common.config` did **not** improve (still >280 s) — confirmed >3-way megamorphic, so
  the added per-call 3-entry scan was pure overhead with ~no hits.
- It **regressed** other JIT-on cases (the extra scan on megamorphic sites) and was
  therefore **reverted**. Net code change from (a): **zero**. Correctness was never
  affected (a polymorphic-dispatch probe, `Disp3`, stayed correct throughout).

**Takeaway:** making the *helper* polymorphic with the *existing 3-entry* PIC is not the
fix. A real fix needs a **bigger / profile-driven PIC** (N≫3 or megamorphic-vtable
fallback) *and* reduced per-call dispatch overhead — both larger changes.

---

## 4. Shipped: env-configurable JIT threshold (`CRATONVM_JIT_THRESHOLD`)

The JIT warmup threshold was hardcoded `500` in two places; both now read
`CRATONVM_JIT_THRESHOLD` (default **500** → default behaviour unchanged, zero risk):
- `vm/src/runtime/env_cache.rs::jit_invocation_threshold()` (the cached getter; clamps
  to ≥1, falls back to 500 on unset/invalid).
- `vm/src/runtime/interpreter.rs` — the per-method upgrade gate (was
  `const JIT_INVOCATION_THRESHOLD = 500`).
- `vm/src/jit/helpers.rs` — the dispatch-helper gate (was `const DISPATCH_JIT_THRESHOLD
  = 500`, now removed).

Rationale: since CratonVM's JIT'd call-heavy code is *slower* than interpreting it,
raising the threshold keeps medium-hot/short-lived framework code interpreted
(HotSpot's interpreter-first behaviour) while genuinely-hot **compute** loops still
cross the threshold and compile.

**Validation (the key regression check — does a higher threshold hurt compute code?):**

| Workload | thr=500 | thr=5000 | thr=20000 | thr=50000 |
|---|---|---|---|---|
| **bintrees18** (`bench/binarytrees 18`, `-Xmx8g`) | 10418 ms | — | — | **10007 ms (no regression)** |
| kafka `common.config` JIT-on | ~227 s | ~112 s | ~150 s | ~120 s |

- **bintrees18 is unaffected by a 100× higher threshold** — its hot recursion crosses
  any threshold within a few thousand calls and still compiles; steady-state JIT
  throughput is identical. This is the validated safety result.
- kafka `config` roughly **halves** but **plateaus ~120 s** (still at/over the watchdog)
  because its very-hot framework methods (>50k calls) compile to slow code regardless —
  the §2 codegen root, which **no threshold can fix**.

**Default left at 500.** Only `bintrees18` was validated; flipping the default higher
should follow a full bench-suite + app-smoke run. The knob lets that be trialed
without a rebuild.

---

## 5. How to reproduce / measure (exact recipe)

Worktree + binary: `C:\craton\CratonVM-kafka`, branch `fix/kafka-test-suite`; build with
`build-wt.bat` (PowerShell: `cmd /c "C:\craton\CratonVM-kafka\build-wt.bat"`), binary at
`target/release/cratonvm.exe`. Always `taskkill //F //IM cratonvm.exe` and confirm the
binary mtime advanced before trusting a rebuild (exe-lock trap).

Classpath (kafka-clients 3.7.1 unit suite + JUnit5 + mockito), assembled in
`ktest/lib/`; harness `ktest/RunPkg.java` (`selectPackage`) and `ktest/RunCls.java`
(`selectClass` / `Class#method`). Test classes at `C:/craton/tmp/kafka-test-classes`.
`/tmp/kcp.txt` holds the assembled classpath string. Run:

```bash
CP=$(cat /tmp/kcp.txt); CVM=C:/craton/CratonVM-kafka/target/release/cratonvm.exe
# JIT-on (default), watchdog off, in-JVM RESULT ms:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CVM" -Xmx2g -cp "$CP" RunPkg org.apache.kafka.common.config | grep '^RESULT'
# interpreter baseline:
"$CVM" --nojit -Xmx2g -cp "$CP" RunPkg org.apache.kafka.common.config | grep '^RESULT'
# HotSpot baseline:
"/c/Program Files/Java/jdk-25/bin/java.exe" -Xmx2g -cp "$CP" RunPkg org.apache.kafka.common.config | grep '^RESULT'
# threshold sweep:
CRATONVM_JIT_THRESHOLD=50000 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CVM" -Xmx2g -cp "$CP" RunPkg org.apache.kafka.common.config | grep '^RESULT'
# compute-benchmark regression check:
CRATONVM_JIT_THRESHOLD=50000 "$CVM" -Xmx8g -cp C:/craton/CratonVM/bench binarytrees 18
```

### Diagnostic env vars used
- `CRATONVM_DBG_JITC=1` — log each JIT compile/upgrade (`[cratonvm-jitc] first-compile …`,
  `upgrade-FAIL …`, `OSR-…`). Count distinct vs total to detect churn.
- `CRATONVM_DBG_DEOPT=1` — log deopt events.
- `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` — lift the 120 s abort to let slow runs finish.
- `CRATONVM_JIT_THRESHOLD=<n>` — JIT warmup threshold (default 500).
- `CRATONVM_DISABLE_JIT=1` / `--nojit` — interpreter only.
- `CRATONVM_JIT_BISECT_ONLY=<prefixes>` / `CRATONVM_JIT_BISECT_SKIP=<Class.method,…>` —
  restrict which methods may JIT (used to localize miscompiles).
- `HotSpot`: `-Xint` (interp only), `-XX:+PrintCompilation` (count compiles).

---

## 6. Durable fixes (the real cure — substantial JIT-quality work)

In rough priority / risk order:

1. **Reduce per-call dispatch overhead** in `jit_invoke_virtual_mic` — avoid the per-call
   `Vec` allocation (use a stack/`SmallVec` buffer for small arg counts) and cache the
   parsed descriptor on `JitInvokeInfo` instead of re-parsing each call. *Pure win, no
   regression risk* — helps every JIT virtual call; lower-risk than the rest. (Touches a
   hot, safety-critical helper, so test carefully.)
   **✅ DONE (2026-06-12), as part of the §0 fix series:** (a) the `Value` decode in
   `jit_invoke_virtual_mic` is now lazy — the MIC-hit fast path dispatches straight
   off the raw arg slots, and the redundant `method_args`/`full_args` rebuilds are
   gone; (b) `try_jit_compile_callee` got a process-global **negative-result cache**
   (FNV fingerprint, direct-mapped 32K) so callees that can never compile
   (native-shadowed `HashMap.get`/reflection, skip-listed, FJP, backend bails) cost
   one hash probe instead of two class-hierarchy walks + 3 `Arc` allocs + a
   `padded_bytecode` copy + a compile attempt per call (JIT-cache probed FIRST so
   late compilations are always picked up; periodic re-probe bounds staleness);
   (c) `JitMICSlot::cached_class_name` is `Arc<str>` (per-hit refcount bump, not a
   `String` copy).
2. **Polymorphic dispatch that survives megamorphism** — a larger / profile-driven PIC
   (N≫3, or a fast megamorphic vtable/itable fallback) so >3-type sites stop falling to
   full `invoke_or_native` resolution. The current 3-entry PIC thrashes on real
   framework dispatch.
3. **Inline interpreter intrinsics into JIT'd code** — so JIT'd calls to hot JDK methods
   (`HashMap`, reflection, `Optional`) get the same fast path the interpreter uses,
   instead of routing through generic dispatch.
4. **Asynchronous / background compilation** — compile off-thread and swap in the entry
   when ready (HotSpot's model), so a wrong compile decision never *stalls* execution.
   *Note:* this alone would NOT fix `config`, because the cost there is slow JIT'd
   *execution*, not compile stalls — but it's the right architecture and removes a class
   of future risk.
5. **Smarter JIT gating** — e.g. don't compile a method whose body is dominated by calls
   to intrinsic-backed/native methods (where the interpreter is faster), or
   deoptimize-and-stay-interpreted if a compiled method isn't measurably faster.

Any default-threshold change (item-0 quick lever) must be validated against the full
in-repo bench suite (bintrees/crypto) + app smokes, not just `bintrees18`.

---

## 7. Related / open
- **bug-01** (config "JIT hang") = this throughput issue + the 120 s watchdog. The
  serialization-specific JIT miscompile was fixed upstream by dev `3ccd0bef`.
- **bug-05 / bug-06** "execution hangs" = this throughput issue **plus** a thread-
  lifecycle gap (kafka tests leave non-daemon background threads → the JVM won't exit
  after `main` returns → watchdog fires). See those docs.
- **OOB-guard `warn!` flood** (a per-call `String` alloc + unthrottled `warn!` in the
  heap `get_field` guard) was a separate slowness amplifier on speculative
  collection-layout probes; rate-limited — see bug-06.
- **Possible dev regression to investigate:** `common.serialization` JIT-on *completed*
  right after dev `3ccd0bef` but later **timed out on a clean dev binary** (no local
  changes), implying a dev commit merged since then regressed JIT-on discovery
  throughput/stability. Worth a dedicated bisect.

## 8. Practical status
- The whole kafka-clients unit suite runs **correctly under `--nojit`** (the genuine
  correctness bugs bug-02/03/04 are fixed and merged). JIT-on is the open throughput/
  quality gap scoped here.
- Shipped from this workstream: the `CRATONVM_JIT_THRESHOLD` knob (default 500). No
  default behaviour change.
