# ✅ FIXED — the Generational young sweep frees an object the INTERPRETER still holds

> **RESOLVED 2026-09-08.** The object was not lost by the sweep. It was lost by
> the NATIVE that was using it: `monitor_wait` parks, a peer's collection runs
> to completion inside that park, and nine natives went on using the pre-park
> address afterwards — including `MonitorTable::exit`, whose first act is
> `header_of(obj_ref)`. That is the SIGSEGV this page reports, and the
> `native_object_hash_code` / `get_array_element` sites below are the same
> family one call deeper: `native_lbq_put_blocking` handed `native_lbq_offer`
> its raw pre-park `args`, so the callee's receiver really was "dead on ENTRY,
> straight out of `args`" — this page's own words.
>
> The fix, the evidence, the two probes and the new gate rule that catches the
> shape are in
> `natives-hold-a-stale-reference-across-a-park-FIXED-20260908.md`.
> `probes/OldToYoungBarrierSweep.java` reproduces this page's crash in seconds
> on Windows, with and without the JIT, and is clean 35/35 after the fix.
>
> **What this page got right and is worth keeping**: `--nojit` reproduces (the
> defect is not the JIT); it is a FREE, not a move; the load gating; and the
> retraction of its own first revision. **What it got wrong**: the sweep. The
> root-in-dead-span invariant it never reached is unconditional and would have
> retained the span — these references were in no root set at all.
>
> The original page follows unchanged.

---

# The Generational young sweep frees an object the INTERPRETER still holds

| | |
|---|---|
| **Status** | OPEN, and re-scoped 2026-09-06 evening. The page's two headline claims are both wrong now: the JIT arm's test failure is LOAD-DEPENDENT and is not corruption, and the `--nojit` arm — recorded here as passing — is the one that corrupts. It SIGSEGVs in ~10 s. |
| **Scope** | `--XX:UseGc Generational`. **No JIT required** (`--nojit` reproduces it faster and more often than the JIT arm). HotSpot and CratonVM ZGC pass. |
| **Reproducer** | one test METHOD, **~10-30 s**, 4-7 runs in 10 |
| **Dominant site** | `native_object_hash_code` → `identity_hash_code` on a receiver whose header is in a span the collector proved dead |
| **Detector** | `CRATONVM_GEN_UNCOMMIT` (default ON since `dadef6dbd`, 14:39) — it decommits the dead span, so the stale read FAULTS instead of silently succeeding |

## RULED OUT: the stale argument snapshot (`14d9a50f4`)

The obvious candidate was another session's same-day fix:

> a native that re-enters Java holds its ARGUMENT SNAPSHOT across the
> collection that re-entry can trigger. `safe_native_call_impl` pins every
> argument but rebuilds the snapshot from those pins only for a collection it
> runs ITSELF, before the callback.
>
> — `internal/fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`

"The receiver is dead on ENTRY, straight out of `args`" is exactly what that
looks like from inside the callee, and the first revision of these arms was
measured on a binary built at 19:39, an hour before that fix landed at 20:42.
**It is not it.** Interleaved, one arm at a time, same host:

| binary | `--nojit` SIGSEGV, x8 |
|---|---:|
| dev tip, WITH `14d9a50f4` | **6** |
| the 19:39 binary, without it | 3 |

The defect survives the fix, so the census below is current. (Do not read the
6-vs-3 as a regression: this arm ran 2/5, 4/6 and 5/6 across the evening at
loads from 3 to 35. It is one workload's rate, not a controlled comparison of
the two commits.)

Re-censused on the tip binary under `gdb`, three crashes: two
`native_object_hash_code`, one `get_array_element` — same family, same
conclusion as the five below.

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

## The corruption is LOAD-GATED, and that governs every arm

Collected across 2026-09-06/07, `--nojit`, same reproducer:

| host load | SIGSEGV |
|---|---|
| ~1.3 | **0/4** |
| 3-6 | 2/5, 2/6 |
| 6-11 | 5/6, 4/6 |
| 20-35 | 3/3 |

**A quiet host reads zero.** Any arm measured below load ~3 is vacuous, in
either direction — which is also why the JIT arm above "passes" there.

**Do NOT amplify with CPU burners.** Six spin loops reach load 11 and the
broker then cannot start at all: 12/12 `rc=124`, stalled at
`BROKER_REGISTRATION ... node 0 disconnected`, zero crashes. That starves the
workload instead of exposing the defect, so the arm says nothing. The loads in
the table above came from other real work on the shared host, which is a
different kind of contention.

## Why `CRATONVM_DBG_SWEEP_ZERO` is silent here — and it is NOT the unmapping

The obvious explanation was that `CRATONVM_GEN_UNCOMMIT` unmaps the span, so
the detector's read of the zeroed header faults instead of returning zeros.
**Tested and refuted**: with `CRATONVM_GEN_UNCOMMIT=0` (span mapped, and the
crash gone), `RECLAIMED-LIVE` is still **0 in 12 runs**.

The real reason is structural, and it is a coverage gap rather than a bug:

* the sweep-zero ring records the **YOUNG sweep only** (`gen_heap.rs`'s own
  comment says so), and
* its consumer fires only when a zeroed object turns up as the receiver of an
  **interpreter INVOKE** with an all-zero header (`interpreter/invoke.rs`).

This page's victims fault inside a NATIVE (`native_object_hash_code`) and
inside `get_array_element`. Neither is that check, so the probe is blind here
by construction. Do not read its zero as evidence that nothing was reclaimed.

## A probe that can answer: `CRATONVM_DBG_DEADRECV`

Added 2026-09-07. At `identity_hash_code` it asks the two ALWAYS-ON
reclamation rings (`old_freed_lookup_covering`, `young_freed_lookup`) whether
the receiver is an address this process already freed — **before the first
dereference**, which is what no existing consumer does: a failed `checkcast`
reads the class id, the sweep-zero consumer reads the header, so neither can
speak when the read itself faults. Both lookups are keyed on the ADDRESS and
touch no heap memory, so they answer whether or not the page is still mapped.
On a hit it reports through `reclaim_guard` — original class, which sweep
freed the block, and which live object still holds the address — and returns
0 so one run names many victims instead of dying at the first.

**IT HAS NOT YET FIRED, and it has not yet had a fair chance.** Every run of it
so far either sat on a quiet host (0/4, nothing to catch) or under burner load
(12/12 broker-registration timeouts). It is landed because the instrument is
the blocker, not because it has produced a result. The next attempt wants real
mixed load on the host and enough reps to catch the 4-in-10 band.

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
