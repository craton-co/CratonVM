# `CompletableFuture` composition — the three residuals discharged, and 1.19x

## Status
**CLOSED, 2026-09-02.** Opened 2026-09-01 as the successor to
`juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`.
It carried a flat profile, four refutations, and three "where to look next"
items. All three are discharged, each with a mechanism, an instrument and a
number:

| item | disposition |
|---|---|
| #1 why is there so little compiled code to inline? | **ANSWERED, and the answer is that it does not matter.** `receiver_is_java_util` gated the invocation COUNTER, so no `CompletableFuture` method was ever nominated. Fixing that takes the tracked set from 7 methods to 19 and C2 bodies from 3 to 12 — and costs **0.994x**. See "#1" |
| #2 `Integer.intValue` at 3.99 crossings per chain, bind not engaging | **FIXED** — the bind was refused at COMPILE time at every door, by a pin added for an unrelated rule. `sites_bound=0 served=0` becomes `4 / 159 178`, worth **1.026x**. See "#2" |
| #3 name-keyed lookup, 8.2 % | **FIXED in the half that dominated it, and the page's attribution of the other half was wrong.** A native stub called `postComplete` by NAME 2.5 times per chain; that is **1.154x** on its own. See "#3" |

Composition is **1.193x faster** and the three refutations the page opened with
still stand. What it was ABOUT — that composition is ~20x — is not fixed, for
the reason the page itself gave: the cost is flat and structural. The part of
that which is now newly askable has its own page:
[`../../known-issues/perf/composition-native-callback-and-the-promotion-question-20260902.md`](../../known-issues/perf/composition-native-callback-and-the-promotion-question-20260902.md).

Everything below is measured on this branch unless stated.

## The measurement

`HibfixComposeProbe2`, 2 threads x 320 000 chains, **user CPU** (the host is
shared; see the predecessor's "Why the idle box stopped being required"), eight
INTERLEAVED reps per arm, `wrong=0` in every run of every table on this page.

| arm | user-cpu range (s) | median | us/chain | ratio |
|---|---|---:|---:|---:|
| **shipping default** | 15.96-16.43 | **16.18** | **25.29** | 1.000x |
| the pre-change binary | 19.17-20.51 | 19.30 | 30.16 | **1.193x** |
| same binary, every switch off | 18.56-20.04 | 19.43 | 30.36 | 1.200x |

**The third row is the control that makes the first two a measurement.** It is
ONE binary with five switches thrown, and it reproduces the separately-built
pre-change binary to within 0.7 % — so the delta is the change and not the
build, and nothing in it is unaccounted for.

### Per lever, same protocol, ranges given because two of them overlap

| lever | switch | ratio | ranges disjoint? |
|---|---|---:|---|
| `postComplete` no longer resolved by NAME | `CRATONVM_NATIVE_CF_POSTCOMPLETE_DIRECT=0` | **1.154x** | yes (17.67-18.88 / 15.56-16.38) |
| `Integer.intValue` thin bind | `CRATONVM_JIT_INT_VALUE_DIRECT=0` | **1.026x** | no (15.98-16.81 / 15.86-16.45) |
| frame-free compiled lambda capture | `CRATONVM_JIT_INDY_LAMBDA_FAST=0` | **1.021x** | no (15.76-17.03 / 15.56-16.38) — but 1.019x and 1.026x in two other runs |
| skip `postComplete` when there is no waiter | `CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP=0` | 1.006x | no — and it CANNOT help here, see #3 |
| nominate java.util callees | `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS=1` | **0.994x** | no — this is why it ships OFF |
| cache the two hot-path flag reads | `CRATONVM_JIT_HOT_LOOKUP_CACHE=0` | 0.995x | no — **a refuted hypothesis, see #3** |

1.154 x 1.026 x 1.021 = **1.209**, against 1.200 measured for all-switches-off.
The three that separate account for the whole change; the other three are noise
either way and ship on their merits, not on a number.

## #1 — why is there so little compiled code, and why that is the wrong question

`CRATONVM_DBG_TIERUP_DECLINE=1` is new. It names, per method, the FIRST
condition in `execute_invokevirtual_cached`'s tier-up chain that refused the
site. On 40 000 chains it reports 46 364 declines and **every meaningful row is
the same one**:

| declines | reason | method |
|---:|---|---|
| 39 998 | `receiver_is_java_util` | `CompletableFuture.getNow` |
| 855 | `receiver_is_java_util` | `CompletableFuture.tryPushStack` |
| 853 | `receiver_is_java_util` | `CompletableFuture.unipush` |
| 534 | `receiver_is_java_util` | `CompletableFuture.thenCompose` |
| … | `receiver_is_java_util` | eleven more `java/util/concurrent/` rows |
| 35 | `admitted` | `java/lang/Enum.compareTo` |

`java/util/concurrent/` is inside the `java/util/` prefix, so the exclusion
added for a Spring **collections** graph refuses the whole of
`CompletableFuture`. And the exclusion sat in the `&&` chain that also gates the
invocation COUNTER — so the refused methods were never counted, never
nominated, and therefore invisible to `jit-method-stats`, whose population is
NOMINATED methods. That is the mechanism behind this page's own finding #2:
lowering `CRATONVM_JIT_THRESHOLD` 25x bought six more tracked methods and no
time, because the methods that matter never reached the counter at all.

**Nomination is not promotion.** The exclusion is a PROMOTION hazard — it stops
the interpreted site entering a compiled body directly — and there is no
argument for it stopping the counter. `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS=1`
separates them, leaving the promotion barred exactly as before:

| | tracked | ever invoked | C1 | C2 |
|---|---:|---:|---:|---:|
| default (nomination barred) | 7 | 5 | 3 | 3 |
| `=1` (nomination admitted, promotion still barred) | **19** | **17** | 6 | **12** |

and, priced as the only difference in one binary, eight interleaved reps:
**25.24 us/chain with the lever on against 25.09 off — 0.994x.**

**It buys nothing**, and `CRATONVM_DBG_INTERP_FRAMES=1` says why:
`getNow` is compiled under the opt-in and is STILL entered interpreted 40 000
times in 40 000 chains, because the site that counts is the site that may not
promote. So the lever ships **default-OFF** — not deleted, because it is the
instrument that establishes the point, and the arm anyone re-opening the
promotion question has to run first.

The question "why is there so little compiled code to inline" is answered and
retired: the small compiled set was a real defect in the counter, and it was
never the constraint.

### The instrument that made this readable

`CRATONVM_DBG_INTERP_FRAMES=1` counts interpreted frame pushes per method — the
census `jit-method-stats` structurally cannot produce, because its population is
the complement of the answer. On the unfixed binary, 40 000 chains:

| interpreted frames | share | |
|---:|---:|---|
| 99 458 | 63.5 % | `<jit-indy>.bridge` — one per lambda CAPTURE |
| 40 000 | 25.5 % | `CompletableFuture.getNow` — 1.00 per chain, forever |
| 17 179 | 11 % | everything else, ~540 each: warm-up before each method compiled |

Two rows are the whole steady state. That is what "which frames is
`execute_frame_from_index` running?" resolves to, and neither row is what the
page expected.

**The first row was a fixable defect.** A compiled `invokedynamic` that has
already been bootstrapped is an allocation and nothing else, but the bridge
built a whole interpreter frame — five `Arc` clones, a pooled locals/stack
acquisition and a `Box::new(OwnedFrameMeta)` — purely so
`allocate_lambda_proxy` could pop the captures off an operand stack that the
compiled caller had already filled. `execute_jit_indy_generic_raw` now takes the
captures straight from the spill buffer for an already-cached
`ResolvedCallSite::Lambda`; everything else falls through to the frame path
unchanged. Interpreted frames go **155 951 -> 58 276** and the `<jit-indy>` row
disappears. 1.021x.

## #2 — `Integer.intValue`: the bind was refused by the property that justifies it

The page said the bind "is plainly not engaging at these sites" and asked why.
`--dump-native-registry` cannot answer that: the thin helper is wrapper-free and
does not bump the registry's invocation counter, so a crossing count is evidence
about the SLOW route only. `CRATONVM_DBG_DIRECT_BINDS=1` is the instrument that
separates the possibilities — sites bound, calls served, calls declined — and it
reported

    Integer.intValue: sites_bound=0 served=0 declined_to_dispatch=0

against 318 758 registry crossings on the same run. **Refused at compile time,
at every door.** `CRATONVM_DBG_INTRINSIC=1` names the refusal in one line:

    single-pass site java/lang/Integer.intValue()I @pc=2 kind=1

`kind=1`, not 0. `invokevirtual_site_final_owner` pins an `invokevirtual` whose
target is unoverridable — a `final` method, or any method of a `final` class —
as statically bound, substituting the declaring class and reclassifying the site
from virtual (0) to special (1). `java/lang/Integer` is `final`, so every
`intValue()` site is pinned, and:

* the single-pass ladder's recognition tests `invoke_kind == 0`, so it stopped
  matching;
* the optimizing ladder's door is `is_static || is_special`, and it never had an
  arm, because its own scope note recorded — correctly when written — that
  "`Integer.intValue` is an `invokevirtual`" and therefore out of reach.

**The irony is exact: the property the bind's comment cites to justify being
guard-free (`Integer` is final, so the site is monomorphic) is the property that
disqualified it.** The pin made the optimizing door's premise false and nothing
re-read it — the same shape as
`a-workaround-can-outlive-its-defect-and-its-comment-is-the-last-to-know`.

Both doors now carry the arm (`Long.longValue` with it, on the identical
argument — `java/lang/Long` is `final` too):

| | sites bound | served | declined |
|---|---:|---:|---:|
| `CRATONVM_JIT_INT_VALUE_DIRECT=0` | 0 | 0 | 0 |
| default | **4** | **159 178** | **0** |

159 178 over 40 000 chains is 3.98 per chain — exactly the population the
registry crossings were. 1.026x.

## #3 — the name-keyed lookup is a native calling BACK into Java by name

The page asked "which call sites reach `find_with_kind` and
`invoke_on_class_shared_inner` with a NAME rather than a resolved id". The
registry's own miss census (`CRATONVM_DBG=native-lookups`, which already
existed) answers it in one row, on 40 000 chains:

    miss  100 008  java/util/concurrent/CompletableFuture.postComplete()V

**76 % of every missed registry lookup on the workload, and `postComplete` is
not a native and can never be one.** `native_cf_complete` — the registered
`Bridge` stub this page's finding #3 proved is a 3.14x WIN — ends with
`ctx.invoke_virtual(this, "postComplete", "()V", &[])`, a by-NAME virtual
dispatch out of a native. That goes through `invoke_or_native`, which hashes the
53-byte triple against the slot table twice (`find` and `find_with_kind`), runs
the cold descriptor-quirk rewrite, misses all three times, and only then reaches
`invoke_on_class_shared_inner`. A dwarf profile shows the whole loop:

    execute -> invoke_on_class_shared_inner -> invoke_or_native
            -> NativeContextImpl::invoke_virtual -> invoke_on_class_shared_inner -> execute

`invoke_virtual_bytecode_only` is the primitive for exactly this — "run this
method's bytecode, do not ask the native registry" — and six other stubs in the
same file already use it. Per 40 000 chains:

| | before | after |
|---|---:|---:|
| registry lookups | 332 536 | **132 704** |
| `find` | 123 422 | 23 544 |
| `find_with_kind` | 203 609 | 103 629 |
| descriptor-quirk rewrites | 219 214 | **19 325** |
| general invokes | 200 817 | **100 818** |
| `postComplete` missed lookups | 100 008 | **0 (row gone)** |

**1.154x, ranges disjoint.** It is the largest single lever this page produced,
and it was hiding inside a bucket the page had already looked at twice.

**Five call sites, not one.** `postComplete` is reached this way from
`native_cf_complete`, `native_cf_complete_exceptionally`,
`util_concurrent_ext`'s completion arm, `phases_late/concurrent`'s
`completeThrowable` arm and `native-io`'s async-socket completion. Only the
first is on the measured path; all five are changed, because a fix that lands
for one of five identical call sites is at the wrong level.

### And the page's own attribution of this bucket was half wrong

The profile listed `name-keyed lookup (string hash/compare) 8.2 %`, itemised as
`HashMap<&str,()>::contains_key 1.24 %`, `__memcmp_evex_movbe 2.54 %` and
`NativeMethodRegistry::find_with_kind 0.70 %`. The first two are **not the
native registry**. `CRATONVM_DBG_FLAGREADS=1` — which also already existed —
reports 400 000 declared-flag reads in 80 000 chains, 98 % of them two names:

| reads | flag | site |
|---:|---|---|
| 250 135 | `CRATONVM_DBG_SHADOW` | `set_jit_thread`, i.e. every interpreter->JIT crossing (3.1/chain) |
| 141 112 | `CRATONVM_NEEDS_EXACT_TRACE` | `NativeContextImpl::invoke_virtual` (1.76/chain) |

A declared-flag read converts the `OsStr`, FxHashes the ~20-byte name against
the declared-flag SET, then hashes it again against the legacy-value MAP — which
is what `contains_key::<str>` and a share of the `memcmp` line actually are. The
`CRATONVM_DBG_SHADOW` read sat IN FRONT of the `ONCE` swap that makes its trace
one-shot, so it kept paying long after the trace could ever fire again.

Both are now cached gates, and the `lambda_proxies` probe on the same callback
path got the id-range guard its sibling `is_lambda_proxy_class` already had.

**And it is worth 0.995x — nothing.** That was a hypothesis, it had a switch
built for it precisely so it could be priced rather than asserted, and the
switch refuted it. It ships anyway because it is strictly less work for an
identical answer, but **no part of the 1.193x is this**, and a reader taking the
8.2 % bucket as "the native registry" would have spent the day in the wrong
crate. Recorded because the correction is worth more than the row.

## What is NOT fixed

* **Composition is still ~25 us/chain against HotSpot's sub-microsecond.** The
  page's own diagnosis stands: the cost is flat, uniform across the dispatch and
  GC support stack, and structural. 1.193x is a real 1.193x and it is not a
  dent in the ratio.
* **`getNow` is still one interpreted frame per chain.** It is nominated and
  compiled under the opt-in and never promoted, by design.
* **Every other native->Java callback in the VM still resolves by name.**
  `invokes(general)` is still 2.52 per chain after this change, and
  `find_with_kind` still runs about once per general invoke.

The last two are the successor page:
[`../../known-issues/perf/composition-native-callback-and-the-promotion-question-20260902.md`](../../known-issues/perf/composition-native-callback-and-the-promotion-question-20260902.md).

## Still excluded, with the evidence

The page's four refutations are untouched by any of this and must not be
re-tried: IR-tier inlining (1.011x, 12 bodies spliced), the tier-up nomination
THRESHOLD (25x lower buys nothing — and #1 now explains why in terms of the
counter rather than the threshold), de-registering
`CompletableFuture.complete` (3.14x SLOWER — and #3 is the same stub made
cheaper rather than removed), and locks/thread scaling (0.2 % of the profile;
CratonVM is the flatter of the two VMs).

## Repro

```bash
javac -d <out> apps/probes/HibfixComposeProbe2.java
/usr/bin/time -f "cpu user=%U sys=%S wall=%e" \
  cratonvm --java-home <jdk> -Dprobe.threads=2 -Dprobe.chains=320000 -cp <out> HibfixComposeProbe2

# the five switches, all default-ON except the last, one binary each way:
CRATONVM_NATIVE_CF_POSTCOMPLETE_DIRECT=0   # 1.154x
CRATONVM_JIT_INT_VALUE_DIRECT=0            # 1.026x
CRATONVM_JIT_INDY_LAMBDA_FAST=0            # 1.021x
CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP=0     # 1.006x
CRATONVM_JIT_HOT_LOOKUP_CACHE=0            # 0.995x
CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS=1     # 0.994x -- default-OFF, opt-IN

# engagement, which needs no quiet box:
CRATONVM_DBG_DIRECT_BINDS=1   cratonvm ... 2>&1 | grep direct-binds
CRATONVM_DBG_INTERP_FRAMES=1  cratonvm ... 2>&1 | grep interp-frames
CRATONVM_DBG_TIERUP_DECLINE=1 cratonvm ... 2>&1 | grep tierup-decline
CRATONVM_DBG=native-lookups   cratonvm ... 2>&1 | grep native-lookups
CRATONVM_DBG_FLAGREADS=1      cratonvm ... 2>&1 | grep flagreads
CRATONVM_DBG_INTRINSIC=1      cratonvm ... 2>&1 | grep 'single-pass site'
```

Read the **user** CPU column: `cpu_sys` on this host carries the other tenants
and swamped a 2 % row until it was dropped. Interleave the arms, six reps
minimum, and quote the ranges — two of the three levers on this page do not
have disjoint ranges on their own and say so.

## Gates

Regression suite **85/85 scheduled vectors, 0 failures, 0 harness-blindness
flags**, on the shipping default.

Three new default-off diagnostics (`CRATONVM_DBG_INTERP_FRAMES`,
`CRATONVM_DBG_TIERUP_DECLINE`, `CRATONVM_DBG_DIRECT_BINDS`), each one relaxed
load when unarmed.

## Related

- `juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`
  — the predecessor, and the source of every "already excluded" row.
- `ir-inline-gauntlet-soak-20260828.md` — the soak finding #1 was measured
  against.
- `retired/aqs-thread-handoff-latency-RETIRED-20260805.md` item 3 — the
  measurement that refused to narrow the `java/util/` prefix. It priced
  admitting the PROMOTION; #1 above separates nomination from it and prices the
  other half.
- [`../../known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`](../../known-issues/perf/interpreted-invoke-cost-350ns-20260825.md)
- [`../../known-issues/perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](../../known-issues/perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
