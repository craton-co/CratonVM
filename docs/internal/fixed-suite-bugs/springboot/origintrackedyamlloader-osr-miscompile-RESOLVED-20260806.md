# `OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb` — not an OSR miscompile; attributed to the recycled-`JitInvokeInfo` defect fixed by `383e7f5cf`

| | |
|---|---|
| **Status** | ✅ **RESOLVED (attributed)** — retired from `docs/known-issues/springboot/` on 2026-08-06. No code change: the fix landed on 2026-08-05 |
| **Cause** | the recycled-`JitInvokeInfo` address that let one call site serve another's dispatch — `383e7f5cf`, 2026-08-05 13:22 |
| **Confidence** | signature match on every datum the page recorded, **plus** the defect's precondition now measured inside this exact test. The confirming red — the corruption itself, with the defect switched back on — needs a loaded multi-core Linux host and was not obtainable here; the procedure is recorded below |
| **Last observed failing** | 2026-08-04, on a binary that predates the fix. Never since |

## What it was filed as, and why that is now excluded

The page was titled "an OSR miscompile" on the strength of two single runs:
`--nojit` passed and `CRATONVM_JIT=-osr` passed. The mechanism it named was
register pressure — `x64::LOCAL_REGS` is 7 on Windows and 5 on System V, so
Linux coalesces more and the OSR dead mask has more to do.

That mechanism was instrumented on 2026-08-06 and is **absent**:

| arm | result |
|---|---|
| default (7 registers) | 13/13 |
| `CRATONVM_JIT_LOCAL_REGS=5` (System V) | 13/13 |
| `CRATONVM_JIT_LOCAL_REGS=4` / `=3` | 13/13 |
| `CRATONVM_DBG_OSR_SEED_COLLISION=1` | **1038 takeable OSR entries across 234 methods, zero violations** of either seed invariant, at 7 registers and at 3 |
| `CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES=1` (reproduces the `Arrays.sort(long[])` defect, 51 violations on `SortProbe`) | **nothing** — this workload has no slot that is cat-2 in one range and cat-1 in another |

So the two known OSR-miscompile shapes are excluded by measurement, and a
Windows green stopped being vacuous for the register-pressure story.

## What the failure's signature actually matches

| the page's own datum | recycled-`JitInvokeInfo` (`383e7f5cf`) predicts |
|---|---|
| `--nojit` PASSes | ✔ JIT-only by construction — the defect is a JIT dispatch memo |
| observed once, on a host **above load 30** | ✔ needs enough `CompiledMethod`s to be dropped for a `JitInvokeInfo` address to be re-issued |
| the **pre-fix** binary passes on an **idle** host, 11 runs | ✔ the sibling page measured exactly this: 11 quiet runs green with the defect deliberately on |
| `deny=canLoadFilesBiggerThan3Mb` **and** `deny=constructSequenceStep2` each PASS, and they cannot both name a sole culprit | ✔ `deny` perturbs compile scheduling globally, i.e. changes *which* sites get recycled addresses. Unrepeatable "this lever fixes it" readings are the defect's calling card |
| the corrupt document is built by `StringBuilder.append(String)` in a loop | ✔ the detector's own published example names `java/lang/StringBuilder.append(Ljava/lang/String;)Ljava/lang/StringBuilder;` as a recycled key (see the sibling page) |
| last seen 2026-08-04; the fix landed 2026-08-05 | ✔ |

The sibling failure — [`reactor-netty-outbound-request-line-corruption-RESOLVED-20260806`](reactor-netty-outbound-request-line-corruption-RESOLVED-20260806.md)
— was *silent buffer corruption in a different subsystem*, in the same fixture,
in the same week, and it was this defect.

## New measurement: this test does recycle site keys, abundantly

This is the part that was missing. `CRATONVM_DBG_SITE_ALIAS` answers "does this
workload recycle `JitSiteKey`s at all?" in one run — the *precondition* of the
defect, rather than its rare visible corruption. Run against
`OriginTrackedYamlLoaderTests` itself (8 concurrent runs, Windows):

```
[site-alias] #3 of 620 keys: key=0x166fcaf9b80
  WAS org/springframework/boot/env/OriginTrackedYamlLoader$OriginTrackingConstructor
        .constructTrackedObject(Lorg/yaml/snakeyaml/nodes/Node;Ljava/lang/Object;)Ljava/lang/Object;
  NOW java/util/Map.put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;

[site-alias] #3 of 562 keys: key=0x223fa644e80
  WAS org/yaml/snakeyaml/scanner/Constant.has(I)Z
  NOW org/yaml/snakeyaml/nodes/NodeTuple.getKeyNode()Lorg/yaml/snakeyaml/nodes/Node;

[site-alias] #2 of 566 keys: key=0x166fcaf4880
  WAS java/util/Iterator.hasNext()Z
  NOW org/yaml/snakeyaml/nodes/NodeTuple.getValueNode()Lorg/yaml/snakeyaml/nodes/Node;
```

Every lane recycles keys — 1 to 4 events per run out of 25→620 distinct keys —
and the sites involved are **the loader's own constructor callback and
snakeyaml's scanner/parser**, i.e. exactly the two halves of this test that the
page could never choose between. That is the first direct evidence tying this
test to this defect rather than to a resemblance.

## The confirming red, and why it is not here

`diag/site-alias-detector-20260806` carries `CRATONVM_JIT_NO_SITE_MEMO_FLUSH`,
which puts the pre-`383e7f5cf` behaviour back on one binary. This branch's
`diag/site-alias-yaml-verify-20260806` carries the same switch rebased onto
current `dev` (the flush is now generated from `site_keyed_memos!`'s own
declaration list, so the old commit no longer applies).

**Positive control first** — the switch has to reproduce a known failure, or a
green in the other arm proves nothing. Windows, `cratonvm-sitealias-20260806.exe`:

| class | arm | result |
|---|---|---|
| `WebMvcAutoConfigurationTests` | `CRATONVM_JIT_NO_SITE_MEMO_FLUSH=prefix` | **`tests=93 failed=89`** |

89/93 matches the sibling page's Azure number (89) and its Windows number (88).
The switch is live in this binary.

**Then this class**, `OriginTrackedYamlLoaderTests` (13 tests):

| arm | shape | result |
|---|---|---|
| `=prefix` (faithful pre-fix: two memos flushed, six stale) | 1 serial | 13/13 |
| `=prefix` | **8 concurrent** | **8 × 13/13** |
| `=all` (nothing flushed — a superset of the defect) | **6 concurrent** | **6 × 13/13** |

(15 runs of the class in total across the three arms, all `tests=13 failed=0
containersFailed=0` — real test counts, not mis-scored load failures.)

All green. That is a **null result, not an exoneration**, and it is the same
null result the sibling page got before it moved hosts: with the defect
deliberately on, it took 8-way concurrency at load ~300 on a 16-core Azure box
to make the corruption appear at all, and 11 quiet runs plus 6 Windows runs
beforehand were clean. This host is a quiet Windows desktop; it does not supply
that. Re-running the table above on a loaded Linux box is the one outstanding
step, and it is cheap: check out `diag/site-alias-yaml-verify-20260806`, build,
run the positive control, then the two arms.

## Residuals closed

**"Which side is corrupt — the built `StringBuilder` or the parse — was never
measured."** `probes/YamlSplit.java` exists to answer that and had never been
run. It is now runnable, and it was not before: two defects in the probe itself
were fixed here.

* It resolved `org.yaml.snakeyaml.Yaml` *inside* the try block, so a classpath
  mistake scored as `parse=THREW java.lang.ClassNotFoundException` — a
  classpath error reading as a reproduction of the corruption the probe exists
  to detect. Missing snakeyaml is now its own verdict, `parse=UNAVAILABLE`,
  spelled "this row is ABSENT, not a pass".
* It built a plain `new Yaml()`, whose snakeyaml-2.x default code-point limit
  is 3 MiB. This document is deliberately larger, so a **healthy** host
  reported
  `parse=THREW … The incoming YAML document exceeds the limit: 3145728 code points`.
  It now raises the limit the way `OriginTrackedYamlLoader` does.

With both fixed, the controls are clean and the instrument is usable:

| arm | verdict |
|---|---|
| HotSpot 25 | `build=OK parse=OK entries=233017` |
| CratonVM, `-jit` | `build=OK parse=OK entries=233017` |
| CratonVM, `--nojit` | `build=OK parse=OK entries=233017` |

The build-vs-parse question can still only be *answered* on a failing host —
but the probe that answers it now works, which was not true when the page said
to run it.

## If it ever comes back

The page's capture list stands, with one addition at the front:

1. `CRATONVM_DBG_SITE_ALIAS=1` — if the run that fails also recycles a key
   naming a `StringBuilder`, snakeyaml, or `OriginTrackingConstructor` site,
   this page is the answer and something has regressed `383e7f5cf`;
2. the failing run's full stderr;
3. `CRATONVM_DBG_OSR_SEED_COLLISION=1` and `CRATONVM_DBG=osr`;
4. the host's `/proc/loadavg` and `df`;
5. `probes/YamlSplit.java` on that host, to settle build-vs-parse.

`probes/yaml-osr-lever-matrix.sh` runs every OSR lever 3× and records the load
next to each verdict. Note that its levers are now the *low*-probability
hypotheses: the register-pressure and seed-invariant mechanisms are excluded
above.

## Affected classes

- `core/spring-boot` —
  `org.springframework.boot.env.OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb`
  (1 test; the class is 13). Last observed failing 2026-08-04, on a pre-`383e7f5cf`
  binary. 13/13 on current `dev`, and 13/13 on 14 further runs with the defect
  deliberately switched back on.

## History

Split out of `loader-zip-jit-only-failure-cluster-20260804.md` on 2026-08-06
(retired as
[`loader-zip-jit-only-failure-cluster-FIXED-20260806`](loader-zip-jit-only-failure-cluster-FIXED-20260806.md)),
whose other three items were already fixed.
