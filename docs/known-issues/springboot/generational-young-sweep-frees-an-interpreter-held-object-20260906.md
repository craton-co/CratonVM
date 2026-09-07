# The Generational young sweep frees an object the INTERPRETER still holds

| | |
|---|---|
| **Status** | OPEN, and re-scoped 2026-09-06 evening. The page's two headline claims are both wrong now: the JIT arm's test failure is LOAD-DEPENDENT and is not corruption, and the `--nojit` arm — recorded here as passing — is the one that corrupts. It SIGSEGVs in ~10 s. |
| **Scope** | `--XX:UseGc Generational`. **No JIT required** (`--nojit` reproduces it faster and more often than the JIT arm). HotSpot and CratonVM ZGC pass. |
| **Reproducer** | one test METHOD, **~10-30 s**, 4-7 runs in 10 |
| **Dominant site** | `native_object_hash_code` → `identity_hash_code` on a receiver whose header is in a span the collector proved dead |
| **Detector** | `CRATONVM_GEN_UNCOMMIT` (default ON since `dadef6dbd`, 14:39) — it decommits the dead span, so the stale read FAULTS instead of silently succeeding |

## FIRST: every measurement below predates `14d9a50f4`, and that may be the bug

While this revision was being measured, another session root-caused a defect
with a mechanism that fits the dominant crash here exactly:

> a native that re-enters Java holds its ARGUMENT SNAPSHOT across the
> collection that re-entry can trigger. `safe_native_call_impl` pins every
> argument, so nothing is collected, but it rebuilds the snapshot from those
> pins only for a collection it runs ITSELF, before the callback. A young
> collection inside the callback relocates the object and the snapshot keeps
> naming the old address.
>
> — `internal/fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`
> (`14d9a50f4`, 20:42)

**The binary every arm below was measured on was built at 19:39 and does not
contain it.** "The receiver is dead on ENTRY, straight out of `args`" is what
a stale argument snapshot looks like from inside the callee, so the crash-site
census here may be describing a defect that is now fixed. Re-measure on a tip
that includes `14d9a50f4` before acting on any of it — the arms are cheap now
(§ Repro: 10-30 s each), which is the one thing this revision definitely
improves over the last.

That session's reproducer is also cheaper than a Kafka broker start:
`GpuResidencyGc 0 1024 800` under `-XX:+UseGenerationalGC`, ~20 s, no GPU and
no Azure. Calibrate against it first.

## What changed on this page

The first revision attributed the Kafka failure to a live `FileChannelImpl`
zeroed by the young sweep. **That half is fixed** (`3950eed48`: the channel was
never rooted during construction) and the reclaim is gone — `RECLAIMED-LIVE` is
0 in every run below. What was left was called "a second cause". This revision
identifies it, and in doing so retires two of this page's claims.

### RETRACTED: "Generational + JIT FAILS, `--nojit` PASSES"

That is a property of HOST LOAD, not of the JIT. Same binary, same day, four
arms round-robin one process at a time:

| arm | at load ~20-35 | at load ~5 | wall (quiet) |
|---|---|---|---|
| Generational + JIT | FAIL 3/3 | **PASS 4/4** | 29-41 s |
| Generational + JIT, `CRATONVM_GEN_UNCOMMIT=0` | — | PASS 4/4 | 25-36 s |
| Generational `--nojit` | SIGSEGV 3/3 | **SIGSEGV 2/4** | 8-11 s |
| ZGC (shipped default) | PASS 3/3 | PASS 4/4 | 16-17 s |

The JIT arm's failure is always Kafka's own deadline —
`TimeoutException: Topic testRetryTopic not present in metadata after 60000 ms`
— with the topics ALREADY CREATED (`TOPIC_ALREADY_EXISTS` in the broker log,
27 times) and `UNKNOWN_TOPIC_OR_PARTITION` in the metadata responses. That is
an external timeout on a broker that has not converged, not a wrong answer.
Generational+JIT runs about **2x ZGC's wall** here, which fits inside a 60 s
budget on a quiet box and does not on a loaded one. The page's own table has
the JIT arm SLOWER than `--nojit` (108-586 s against 68-80 s), which is
backwards for a JIT and is the part of it worth keeping.

**No measurement of this test means anything without a same-time control arm
and the load printed beside it.** Both of this page's earlier tables lacked the
second.

### The real defect: `--nojit`, and it is a FREE, not a move

`--nojit` was recorded here as PASS (68, 80 s). It SIGSEGVs. Six reps,
round-robin, one binary:

| arm | SIGSEGV |
|---|---|
| Generational `--nojit`, default | **4/6** |
| `CRATONVM_NO_MOVING_YOUNG=1` | 2/6 |
| `CRATONVM_GEN_UNCOMMIT=0` | **0/6** |

`NO_MOVING_YOUNG` does NOT stop it, so relocation is not necessary — the object
is FREED, not moved. `GEN_UNCOMMIT=0` takes it to zero and that is MASKING, not
a fix: the give-back's own commit describes it as *the detector that made a
silent stale read into a SIGSEGV*. It went default-ON at 14:39 on 2026-09-06,
which is almost certainly why this page recorded `--nojit` as passing a few
hours earlier — that PASS was a masked failure.

The fatal report says so directly:

```text
fault addr is inside a RECENTLY DECOMMITTED heap span:
  base=0x76cd5ae00000 len=0x10200000 site=unbumped-middle
  *** and NOT re-committed since. Something TOUCHED a span the collector
      proved dead ***
```

### Where it faults

Five crashes collected under `gdb -batch`, `--nojit`:

| crashes | site |
|---:|---|
| 4 of 5 | `native_object_hash_code` (`native-builtins/src/lib.rs`) → `identity_hash_code` → `java_identity_hash` |
| 1 of 5 | `execute_invoke_kind` (`interpreter/invoke.rs`) |

`native_object_hash_code` does nothing before the fault but
`ctx.identity_hash_code(this)`, on the receiver taken straight out of `args`.
**So the receiver is already dead when the native is entered**, with no
compiled frame anywhere in the process and no native-local window to blame.
An object reachable from an interpreter frame was freed by the young sweep.

## Repro

```bash
# azureuser@20.80.105.49 — run it ALONE (it binds a broker port), and print
# the load beside every result.
cd /data/cratonvm/apps/spring-boot/module/spring-boot-kafka
CP="../../sb-runner:$(tr '\n' ':' < build/cratonvm-test-cp.txt)"
$CVM --java-home $JDK --Xmx 2g --add-opens=java.base/java.net=ALL-UNNAMED \
     --stack-dump-on-timeout 0 --XX:UseGc Generational --nojit \
     -cp "$CP" SbRunnerMethod \
     org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests \
     testEndToEndWithRetryTopics
# rc=139 in 8-30 s, 4-7 times in 10. Wrap it in `gdb -q -batch -ex run -ex "bt 30"`
# for the site; the first crash usually arrives inside two reps.
```

## What was fixed on the way, and did NOT move this

Four stale-receiver defects in `native-builtins/src/properties_sidetable.rs`,
found while chasing the first backtrace (which landed in that file). They are
real — the file DOCUMENTS the discipline on `ordered_snapshot_kv` and violates
it in four places — and they are **not this crash**. One binary,
`CRATONVM_PROPS_UNROOTED_RECEIVERS=1` restoring all four:

| | fixed | lever (old behaviour) |
|---|---:|---:|
| `--nojit` SIGSEGV, x10 | 7 | 6 |

Flat, twice (an earlier pair covering two of the four sites read 6 against 5).
The crash-site census above is why: 4 of 5 crashes are not in that file. They
are landed on their own merit, with this null result stated rather than a
mechanism asserted — see `internal/fixed-bugs/`.

## Next

The receiver is dead on entry to a native with the interpreter as the only
mutator. That is a root-coverage or sweep-liveness question and nothing to do
with the JIT, which is what makes it much cheaper to chase than this page's
original framing:

* `CRATONVM_DBG_SWEEP_ZERO=1` names a zeroed victim by class and cycle — it
  reads 0 on these runs, so whatever is freed is not being re-invoked through
  the path that probe watches. Find out why it is silent here; that is the
  first instrument to trust or discard.
* `CRATONVM_GC_RESERVE=0` keeps the granules mapped, converting the SIGSEGV
  back into a silent stale read — useful to confirm the same defect is present
  and merely quiet in every arm that "passes".
* The victim's class is unknown. `native_object_hash_code` has the receiver in
  hand; a census of `class_id_of_object` at the moment `identity_hash_code`
  faults would name it.
