# Repro: hot method never compiles because of a cold `new`

Backs `docs/internal/jit-compile-bail-unresolved-new-cold-class.md` (FIXED
2026-07-31). `ColdNewJitProbe.hot(I)I` is unambiguously hot and its only
allocation sits on a branch that is never taken, so nothing ever loads `Cold`
and the compile-time `new` resolver cannot name a class id for it.

```bash
javac -d . ColdNewJitProbe.java
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk25> -cp . ColdNewJitProbe 2000000
```

Two arms, identical except for whether `Cold` is loaded before the hot loop:

| arm | pre-fix | post-fix |
|---|---|---|
| default (`Cold` never loaded) | **8689 ms**, `hot` `tier_fail_count=3`, 1,999,988 interpreted invocations | **776 ms**, `hot` compiles, 1,140 interpreted invocations |
| `-Dcold.trip=true` (`Cold` loaded first) | 765 ms | 862 ms |

`acc=931093952` in all four — this was always a throughput gap, never a
correctness one. The control arm is what makes the measurement non-vacuous:
pre-fix, loading one otherwise-irrelevant exception class was worth 11x.

`CRATONVM_DBG_JITC=1` names the responsible resolver; pre-fix it printed
`resolver-bail site=new_resolve` + `compile-bail ... backend_attempted=false`
for `hot`, post-fix it prints neither.

The json-smart workload the gap was originally found on
(`net.minidev:json-smart:2.6.0`, `JSONParser.parse` over a mixed document set)
shows the same coverage collapse — 13 hot parser methods `compile-failed`,
14,874,779 interpreted invocations before, 0 and 108,840 after — but its wall
clock does not move, because those parser methods are not what bounds it.
