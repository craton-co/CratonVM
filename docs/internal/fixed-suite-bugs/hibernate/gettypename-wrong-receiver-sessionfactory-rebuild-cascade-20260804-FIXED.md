# FIXED (by unrelated remediation) — `Type.getTypeName()` wrong-receiver dispatch during a SessionFactory rebuild cascade

| | |
|---|---|
| **Status** | ✅ **CLOSED** 2026-08-04 — no longer reproduces. Root cause was never conclusively pinned to one commit while the bug was live; strong circumstantial evidence it was fixed as a side effect of the 2026-08-01 GC-root remediation wave, which landed hours after the only witness. No new code changed to close this doc. |
| **ID** | `HIB-GETTYPENAME-CASCADE.1` |
| **Original doc** | `docs/known-issues/hibernate/gettypename-wrong-receiver-in-sessionfactory-rebuild-cascade-20260801.md` (moved here) |
| **Witness** | `org.hibernate.orm.test.hql.ASTParserLoadingTest`, JIT, 2 runs of a round of 6, 2026-07-31, `cratonvm-hqlordinal-fix2-20260731.exe`. The only captured occurrence — see that file's evidence excerpt, carried forward below. |
| **Closing verification** | 2026-08-04, this doc's own hunt: **≈166 forced-cascade runs, ~10,000+ SessionFactory bootstraps**, across two hosts (local Windows + the Azure Linux shared build host), with the `CRATONVM_DBG_CCE_BT` tracer armed the whole time. **Zero occurrences.** |

## Original symptom (2026-07-31 witness, unchanged from the original doc)

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/Integer.getTypeName()Ljava/lang/String;"
  caller="org/hibernate/type/descriptor/java/spi/JavaTypeRegistry.addBaselineDescriptor(Lorg/hibernate/type/descriptor/java/JavaType;)V @pc=27"
```

A `java.lang.Class` mirror (the `Integer.class` mirror `IntegerJavaType.getJavaType()` returns) dispatched `getTypeName()` against `java/lang/Integer` instead of `java/lang/Class` — a receiver whose runtime class disagreed with what allocated it. It surfaced only inside a specific failure cascade: a JUnit test failure makes `SessionFactoryExtension` rebuild the `SessionFactory`; from the sixth rebuild on, every subsequent rebuild in the run hit this warning and died before reaching the connection pool, so the run never produced a usable result. The original doc's full "what the field logs show" and "ruled out" sections (61 reproduction attempts across many JIT/GC/parallelism configurations, all clean) are preserved unchanged in git history at the path above.

## What changed since the doc was written

The original doc, rewritten 2026-08-01, already noted that reproduction had failed on every configuration tried, including forcing the exact non-moving-old-gen-sweep-during-a-young-collection path its own leading hypothesis named. It could not explain why — "the honest reason for the null result… not a fix," per its own text, since even the witness-era binary (`4ad586e79`) had stopped reproducing on the retest host.

Investigating that gap on 2026-08-04, `git log --since=2026-07-31 --grep=mirror` (and adjacent GC-root history) surfaced a concentrated cluster of GC/root-correctness fixes, **all landing on 2026-08-01, within about 20 hours of the witness**:

| commit | time (2026-08-01) | what it fixed |
|---|---|---|
| `0b18f15eb` | 00:28 | A moving-young cycle forwarded overlay-held objects but left external-root-provider side tables pointing at the **pre-copy address** until the collector returned — too late if `old_gen_gc` ran a **major GC in the same cycle**, which seeds its mark worklist from those same side tables. A promoted object could be freed while the only reference to it (held by the side table) was still the stale pre-copy address. Symptom in that commit's own repro: zeroed headers (`ClassId(0)`, `num_slots == 0`) and "stale pointer detected in invokevirtual receiver" fallbacks. |
| `64e6d61b4` | 09:12 | Deleted a stale address-keyed `DirectByteBuffer` table and the "VM-agnostic root registry" it belonged to. |
| `f64f14ffa`, `dc9eceeff`, `cf1116ec4`, `43f38adc4`, `4d4748197` | 09:43–12:18 | A wave of "wire the root visitor we hadn't reached yet" fixes: JIT pending exception state, thread-printed values, scoped-value bindings, deopt-stash roots, and the class-loading broker's epoch. |
| `638bd1190` | 20:20 | `VmHeap::mirror_pin_deferrable` — the exact predicate this investigation's own code review (2026-08-03/04) traced as governing whether a young `java.lang.Class` mirror is unconditionally rooted or deferred to `mirror_pin` during a moving-young cycle — had been silently broken since `67de5400a` flipped `DEFAULT_MOVING_YOUNG` to `true` on 2026-07-28. The predicate had collapsed to always returning `false` (every mirror rooted directly, the *safe* direction), which is why it hadn't shown up as memory-unsafety — but it meant the deferral path this doc's own GC-fallback hypothesis rests on had not been exercised as designed for several days spanning the witness window. |

None of these commits mention `getTypeName`, Hibernate, or this doc by name, and no single one is a provable fix for this exact symptom — this investigation could not reproduce the bug on 2026-08-04, so there was nothing live left to bisect against. But the shape is a strong match: a `java.lang.Class` mirror is exactly the kind of GC-root-tracked object this whole cluster was repairing, the cluster is dense and landed immediately after the only witness, and it directly overlaps the moving-young-GC-root-correctness territory the original doc's leading (and explicitly unconfirmed) hypothesis pointed at.

## Closing verification (2026-08-04)

Ran the existing `run-typename-cascade.sh` / a new `run-typename-cascade-azure.sh` sibling — both deliberately shrink the JUnit per-test timeout so every test in `ASTParserLoadingTest` times out and forces a rebuild, reaching the cascade condition (six-plus rebuilds in one process) reliably instead of waiting on a real timeout to fire by chance. `CRATONVM_DBG_CCE_BT=1` armed throughout, so any recurrence would have produced the `NSME-RECV SHAPE` dump neither prior witness log ever captured.

| where | binary | shards × repeats | rounds | bootstraps (approx) | `getTypeName` hits |
|---|---|---|---|---|---|
| local Windows, dev @ `a9241eedf` | `cratonvm-typenamehunt-20260803.exe` | 8 × 3 | 11 (stopped) | ~2,600 | 0 |
| Azure Linux, origin/dev @ `d1829c6995` | `cratonvm-typenamehunt-azure-20260803` | 3–4 × 2 | 40 (exhausted) + 49 (stopped) | ~7,500 | 0 |

**≈166 forced-cascade runs, ~10,000+ SessionFactory bootstraps, 0 occurrences.** The Azure host is the same class of heavily loaded, multi-tenant shared box the field witness ran on (load average 12–25 across the hunt, dozens of concurrent sessions' builds/tests observed running throughout), which is the strongest available proxy for the original field conditions available for this investigation.

## Why this is closed rather than left open

- The original doc's own reproduction table already showed 61 clean runs across many configurations before this investigation started; today's runs are an order of magnitude beyond that on top.
- The circumstantial fix cluster gives a plausible, dated mechanism for why the signal disappeared, rather than "we just got unlucky twice."
- No open engineering task follows from a bug with zero live reproductions and a well-populated field of already-landed candidate fixes overlapping its symptom class. If it resurfaces, `CRATONVM_DBG_CCE_BT=1` plus `apps/hib-suite-runner/run-typename-cascade{,-azure}.sh` will reach it fast — see Tools below.

## Tools (carried forward, still tracked)

| Tool | What it does |
|---|---|
| `apps/hib-suite-runner/run-typename-cascade.sh` | Drives the rebuild cascade on purpose (shrinks the JUnit timeout so every test rebuilds the factory) on the Windows checkout. |
| `apps/hib-suite-runner/run-typename-cascade-azure.sh` | Same, adapted for the Azure Linux shared host's `/data/data/apps/hibernate-orm-harness` fixture layout and its `CratonRunnerDirect` sibling runner (added alongside the shared `CratonRunner` rather than changing its CLI contract, since other sessions on that host depend on it). |
| `BaselinePrimeProbe.java`, `PostSuitePrimeProbe.java`, `JavaTypeAccessorProbe.java`, `TypeNameDispatchProbe.java`, `SessionFactoryChurnProbe.java` | Original doc's targeted probes; still present in `apps/hib-suite-runner/`, all HotSpot-clean, all clean against CratonVM in every run recorded. |
