# BC suite JIT-ban performance investigation (2026-06-10/11)

Branch `perf/bc-jit-unban`, worktree `C:\craton\CratonVM-bcperf`.
Workloads: `apps/_test-suites/bc-java` `org.bouncycastle.asn1.test.RegressionTest`
(58 tests) and `org.bouncycastle.crypto.prng.test.RegressionTest` (7 DRBG tests),
run via the `run-all-apps-suites.sh` harness conventions
(`--Xmx 1g`, JDK-25 `--java-home`, `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`).

## Starting point

| suite | CratonVM (dev) | HotSpot | gap |
|---|---|---|---|
| bc-asn1-regression | PASS 41.5s | PASS 1.2s | 34.6× |
| bc-crypto-prng | PASS 51.5s | PASS 0.8s | 64.4× |

Working hypothesis was the RBC.1 blanket JIT ban on `org/bouncycastle/`
(`vm/src/jit/skip_list.rs`). Reality turned out richer: lifting the ban
(`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`) crashed asn1 with a native
stack overflow and gave prng **zero** speedup. Root-causing those two facts
surfaced six systemic JIT defects, five of them affecting every workload, not
just BC.

## Defects found and fixed (commits `acc8f5f1`, `1740f43b`)

1. **`cp_ldc_resolver` never wired on the method-entry compile paths**
   (`try_jit_upgrade_with_gate`, its callee-compiler closure,
   `try_jit_compile_callee`). Any method containing an int/float `ldc` —
   BC's `Nat192/Nat256.gte` (loads `Integer.MIN_VALUE`), `SecP*Field`
   reduction constants, `GeneralDigest` processing, many JDK methods —
   failed codegen at the `0x12` arm on every attempt and stayed interpreted
   forever. The OSR path resolved `ldc` itself, which is why OSR compiles
   succeeded while method-entry compiles of the same methods failed.

2. **`ldc` `unwrap_or(0)` miscompile hazard** in `try_compile_inner`: an
   unresolvable String/Class `ldc` would have compiled as "push constant 0"
   (a null reference). Now bails the compile, mirroring `ldc2_w`.

3. **OSR recompiled the whole method on every trigger** ("always recompile in
   OSR"). `SecP521R1Curve$1.lookup` was recompiled 2,610× in one prng run
   (its interface call sites never promote it to method-entry JIT, so every
   call re-trips the 1000-back-edge threshold). Now reuses the cached
   artifact when (a) it has an OSR entry for the pc and (b) it was itself
   OSR-produced (`CompiledMethod::compiled_via_osr`) — first-call/upgrade
   artifacts lack the eager invokestatic direct-call wiring and pinning one
   into a hot loop forever routes callees through the slow dispatch helper.

4. **`dup_x1`/`dup_x2` codegen missing.** `dup_x1` (field post-increment as a
   value: `xBuf[xBufOff++] = in`, the per-byte digest hot path) and `dup_x2`
   in the `++z[i]`-followed-by-cat-1-array-store shape (`Nat.inc`/`Nat.dec`,
   the DRBG block counter) now compile. `Nat.inc` alone caused 35,923 wasted
   full compile pipelines per prng run via the (then unlisted) OSR path.
   Implementation: copy-top into a fresh frame slot + rotation of the model's
   top entries; `canonicalize_stack` already resolves the interim
   non-canonical offsets as a parallel-move problem. `dup_x1` is
   unconditionally safe (JVMS guarantees cat-1 operands); `dup_x2` is gated
   to the verifier-provable FORM-1 shape. Kill switch:
   `CRATONVM_JIT_NO_DUPX=1`. A/B showed the arms are perf-neutral on these
   suites (±1.3s) — the win is eliminating the bail churn.

5. **Planned-inline sites lost their dispatch fallback → bailed inlines
   emitted a CALL to the CALLER's own entry.** `try_compile_inner`'s inline
   branch `continue`d before building `JitInvokeInfo`; the x64 `0xb8` arm's
   fall-through order is inline → direct → info → *else assume
   self-recursive*. When `try_emit_inline` bailed mid-body (rollback), the
   site had neither info nor direct call and was wired as a self-call.
   `Strings.fromByteArray` (invokestatic to same-class sibling `asCharArray`,
   planned for inline, bailed) recursed itself — one `new String` per level —
   until native stack overflow. **This was the deterministic asn1 crash at
   StringTest** (1,365 self-frames identified by matching a cdb `dps` raw
   stack dump against JITC-traced entry addresses), and it is a *global*
   latent wrong-code bug: any method whose static/special callee plans an
   inline that bails could mis-call itself. The earlier RBC.1 comment's
   "rc=139 after ~20 tests" crash signature matches.

6. **Permanent compile failures were retried forever.** Scan rejects
   (e.g. `athrow`, unsupported everywhere) never fed the bail-list:
   `DefiniteLengthInputStream.readAllIntoByteArray` was re-attempted 40,039×
   in one asn1 run (each attempt = skip-list + two native-shadow superclass
   walks under the class_manager lock + a linear bytecode scan). Now: scan
   rejects are bail-listed; the bail-list is checked FIRST in both upgrade
   and callee paths; the first-call path seals scan/compile failures into
   `jit_skip_set` (it used to re-run `padded_bytecode` + `jit_scan` on every
   uncached invocation of an uncompilable method).

## Validation

- `cargo test --release -p cratonvm-jit`: green.
- Regression pool: **23/23 PASS** (worktree binary).
- asn1 under the allow-override: was overflow-crash after 31 tests → **58/58
  pass**.
- Ban-on (default) walls: identical to dev in an interleaved same-conditions
  A/B (prng 60.5 dev vs 60.9 r6; asn1 52.1 vs 52.5). NOTE: absolute numbers
  drifted ~45% slower machine-wide between the morning baselines (41.5/27.8)
  and the overnight measurements (60/52) — interleave any future comparisons.

## What the gap actually is (and is not)

- **It is not the ban.** With every fix above, BC-allow is still net-negative:
  prng 69s vs 61s ban-on; asn1 ~245s vs 52s ban-on. The 34–64× gap vs HotSpot
  is interpreter/JIT-architecture throughput, not skip-list policy.
- prng-allow ≈ ban-on + 13%: the compiled BC leaf methods don't win because
  calls from interpreted callers pay the interp↔JIT boundary
  (`execute_jit_call` marshalling + entry guards), and hot interface dispatch
  (`ECLookupTable.lookup`) never routes through the invoke-cache counter at
  all (it only OSR-enters after 1000 interpreted back-edges per call).
- **asn1-allow has a residual ~190s CPU anomaly that is NOT compiled BC
  code**: proven by two disjoint BISECT_SKIP experiments (skip all 22
  method-entry-compiled BC methods: 262s; skip the 2 OSR-compiled ones:
  245s; ban-on: 52s) and fully CPU-bound (wall 244.6s, CPU 242.9s). The
  remaining suspect set is per-call eligibility-side work that the blanket
  ban short-circuits early (full `should_skip_jit` prefix gauntlet,
  per-call `is_interface_default` class-manager lock in the first-call
  block, invoke-cache counter machinery for eligible-but-uncompiled
  methods). Needs a sampling profiler (build `release-with-debug`, cdb/ETW)
  — next session.

## Recommendation

Keep the `org/bouncycastle/` ban (default behavior unchanged). Merge the six
fixes — they fix a global latent crasher (#5), a global latent miscompile
hazard (#2), and large amounts of wasted compile work (#1/#3/#4/#6) — and
treat "make allow ≥ ban" as a follow-up driven by a real profile:
1. Pin the asn1-allow CPU anomaly with a sampler.
2. Boundary-cost reduction (cheaper `execute_jit_call`, tiny-method inlining
   into interpreted callers, or full-call-tree compilation).
3. `athrow` codegen (set-pending-exception + sentinel-return for
   no-local-handler methods) so the ASN.1 parser statics
   (`readTagNumber`/`readLength`/`checkLength`) can compile at all.
4. Route invokeinterface through the invoke-cache counter so hot interface
   targets (`lookup`) method-entry compile instead of per-call OSR.

## Follow-up session (2026-06-11, commit `8d0f029e`) — anomaly root-caused

The "needs a profiler" anomaly fell to plain code reading before any sampler
ran: in `execute()` (the uncached method dispatch), `padded_bytecode`
(alloc + memcpy), `jit_scan` (linear bytecode walk) and three `Arc` key
allocations ran on EVERY invocation of every JIT-eligible method — BEFORE
the JIT-cache lookup. The blanket BC ban short-circuited all of it at the
`static_skip_reason` gate, which is exactly why ban-on looked fine and
allow looked 4.7× worse with zero BC methods compiled.

**RBC.5 (cache-first dispatch)**: `JitCache::get` takes `&str`, the cache
is consulted first (hit = zero allocations, no scan), padding/scan/keys
moved into the compile-miss closure, and the per-call
"re-resolve getstatic CP refs + ensure-initialized" walk became a
once-per-artifact check (`CompiledMethod::static_init_classes` +
`static_inits_done`). Result: asn1-allow 245s → **151s**; ban-on at dev
parity in a 3-way interleaved A/B (dev 26.2s avg, cache-first 26.6s,
+athrow 27.2s — within noise; note the machine drifts ±2× across hours,
so only interleaved comparisons are meaningful).

**RBC.6 (athrow codegen)** also landed: `jit_scan` accepts 0xbf
(`has_athrow`), the x64 arm lowers it to "call `jit_throw_exception`
(stash pending exception; null → pending-NPE) + return the `i64::MIN`
sentinel + epilogue". Gated to methods with NO local exception handlers,
never via OSR (its bail path could re-run side effects), forces
`has_dispatch` so the drain-aware entry paths run, IR pipeline declines,
and `analyze_escapes` already treated 0xbf as a full-escape barrier. The
helper is a new `JitRuntimeHelpers` field appended at the struct end
(golden offsets stable, NUM_FIELDS 40→41). The BC parser statics now
compile; allow-mode asn1 timing was unchanged (148s) — they were not the
remaining bottleneck.

**Remaining allow-mode gap** (asn1 151s vs 26s ban-on, prng 36s vs 31s):
with the eligibility overhead gone, what's left is the per-call
interp↔JIT boundary on `execute()`-dispatched compiled BC methods
(catch_unwind + set_jit_thread + JitEntryGuard + arg marshalling per
call) — i.e. follow-up items 2 and 4 below. Verdict unchanged: keep the
ban as default policy until boundary cost comes down.

One harness trap for posterity: running the regression pool from a shell
that exports `MSYS2_ARG_CONV_EXCL='*'` (needed for BC's semicolon
classpaths) breaks the pool's `/c/...`-style path ARGUMENTS (3 modload
probes "fail" with rc=1). Pool is 23/23 in a clean shell.

## Diagnostics added (all env-gated, in-tree)

- `CRATONVM_DBG_JITC=1`: compile events with entry addresses/lengths
  (`full-compile`/`upgrade-OK`/`OSR-compile`/`OSR-reuse`), bail causes
  (`scan-bail op/pc`, `codegen-bail pc/op`, `compile-bail` layer,
  `upgrade-FAIL`).
- `CRATONVM_DBG_DEOPT=1`: deoptimization events with reason/bci/action.
- `CRATONVM_JIT_NO_DUPX=1`: disable the new dup_x1/dup_x2 arms.
- cdb recipe for JIT-frame stack overflows: build `release-with-debug`,
  `sxe -c ".lines; k 20; dps @rsp L4000; qd" sov`, then match `dps` quads
  against the JITC entry table (see `trace-results/` scripts).
