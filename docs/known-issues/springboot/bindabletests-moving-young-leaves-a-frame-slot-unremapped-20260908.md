# The MOVING young collector leaves an operand-stack slot naming a vacated address, and `ServiceLoader` reads it

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-08. Deterministic. Not root-caused. |
| **Scope** | `--XX:UseGc Generational`, JIT on, `CRATONVM_DBG_GC_STRESS <= 262144`. Passes at `>= 1048576`. |
| **Collector** | the **MOVING** (Cheney) young cycle — `moving=1203 non_moving=0` on every failing run. This is NOT the non-moving sweep. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64, ~9–20 s |
| **Found while** | chasing [the A5 non-moving reclaim](../../internal/springboot/bindabletests-bytebuddy-receiver-reclaimed-under-gc-stress-FIXED-20260908.md), which does not reproduce on this host. This is a different defect that does. |

## Repro

```bash
CRATONVM_DBG_GC_STRESS=262144 \
CRATONVM_GC_STATS=1 \
CRATONVM_DBG_VACATED_FRAMES=1 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

`CRATONVM_DBG_VACATED_FRAMES=1` is what turns this from "a wrong receiver" into
a named slot; without it the run still crashes, silently.

## The terminal

```text
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/util/Hashtable.openStream()Ljava/io/InputStream;"
  caller="java/util/ServiceLoader$LazyClassPathLookupIterator.parse(Ljava/net/URL;)Ljava/util/Iterator; @pc=22"
```

The `URL` argument reads back as a `java.util.Hashtable`: the **re-served**
face, not the all-zero one. `0x760530000000` is offset 0 of a young semispace,
so a reference left naming a from-space base sees whatever the next cycle
allocated first there.

Under `CRATONVM_NO_LOCAL_LIVENESS=1` the same call fails as
`java.lang.Object.openStream()` — the address was not re-served that run, but
the reference was still stale. **The per-bci local-liveness filter is therefore
not the cause**, even though the guard names it:

```text
ERROR cratonvm::gc::guard: …and the per-bci local-liveness filter DROPPED this
address from a root snapshot at the frame named here.
  obj="0x760530000000" site="invoke dispatch"
  filtered_at=jdk/internal/loader/URLClassPath.<init> pc=188 local[5]
```

That ledger is keyed by ADDRESS and is never invalidated when the allocator
re-serves one, so on a workload that recycles the front of a semispace 1203
times it attributes whatever was filtered there last. It now says so itself:

```text
  filtered_at=jdk/internal/loader/URLClassPath.<init> pc=188 local[5]
  filtered_on=1156 heap_collection=1200 collections_since=44
```

**44 collections stale.** The `URLClassPath` frame is about a different object
that held this address; the lead is dead, and the reproducer above is what
proved it (`CRATONVM_NO_LOCAL_LIVENESS=1` reproduces regardless).

## The mechanism, as far as the evidence goes

Eight `CRATONVM_DBG_VACATED_FRAMES` reports in one run, and they share a shape:

```text
ERROR cratonvm::gc::guard: a LIVE frame slot still names an address the LAST
collection moved an object away from — the frame remap did not reach this slot.
  obj="0x7605300000e8" site="running frame slot (safepoint)" tid=0 frame=1
  class=sun/nio/cs/StandardCharsets method=<clinit> pc=5 slot="stack[0]"
  slot_class=java/lang/String moved_to="0x7605100008d0"
  heap_collection=4 thread_last_heal=4
```

| # | frame | slot | `heap_collection` | `thread_last_heal` |
|---|---|---|---:|---:|
| 1 | `sun/nio/cs/StandardCharsets.<clinit>` pc=5 | `stack[0]` | 4 | 4 |
| 2 | `java/nio/charset/Charset.<clinit>` pc=14 | `stack[0]` | 7 | 7 |
| 3 | `sun/nio/cs/UTF_8.<clinit>` pc=3 | `stack[0]` | 8 | 8 |
| 4 | `sun/nio/cs/StandardCharsets.aliases_UTF_8` pc=4 | `stack[0]` | 9 | 9 |
| 5 | `java/nio/charset/Charset.cache` pc=10 | `stack[0]` | 15 | 15 |
| 6 | `java/lang/ref/Reference.<clinit>` pc=19 | `stack[0]` | 17 | 17 |
| 7 | `java/lang/ref/Reference.runtimeSetup` pc=3 | `stack[0]` | 18 | 18 |
| 8 | `java/lang/ThreadGroup.synchronizedAddWeak` pc=117 | `stack[0]` | 19 | 19 |

Three facts to reason from, and they do not obviously fit together:

* **`heap_collection == thread_last_heal` in all eight.** The thread was healed
  for the collection that vacated the address. The remap ran and did not reach
  the slot.
* **Always `stack[0]`.** `audit_thread_frames` indexes the operand stack with
  `peek_at`, which counts from the TOP, so every one of these is the most
  recently pushed operand. Eight for eight is not a coincidence.
* **Always object-TAGGED.** The audit only inspects
  `Value::Object(Some(_))` slots, so these are not lost-tag slots.
  `ValueStack::update_object_refs` and `ValueStack::scan_object_refs` were made
  symmetric deliberately (see their comments), and an object-tagged slot naming
  a `pointer_map` key is exactly what the update arm rewrites.

So either the address was NOT in that collection's `pointer_map` (it was
reclaimed rather than moved, and `was_vacated` is recording something else), or
the frame was not in the set the remap walked. Both are testable and neither
has been tested.

## What is ruled out

* **Not the non-moving sweep.** `[GC] decision histogram: moving=1203
  non_moving=0 moving-no-jit-frames-live=1203` on every failing run. No JIT
  frame is live at any collection in this workload on this host.
* **Not the local-liveness filter.** `CRATONVM_NO_LOCAL_LIVENESS=1` reproduces.
* **Not stress-threshold-independent.** The failure needs the *thrash* regime,
  where young survivors exceed the stress threshold so `needs_gc` is true at
  every allocation check:

| `CRATONVM_DBG_GC_STRESS` | young cycles | outcome |
|---:|---:|---|
| 65 536 | 1203 | CRASH |
| 131 072 | 1203 | CRASH |
| 262 144 | 1203 | CRASH |
| 1 048 576 | 18–19 | PASS 27/27 |
| 2 097 152 | 8 | PASS 27/27 |
| 3 145 728 | 5 | PASS 27/27 |
| 4 194 304 | 4 | PASS 27/27 |

  **1203 in every crashing run, across four thresholds and three binaries** —
  the workload is deterministic and the failure point is fixed, which is the
  best property this bug has. Use it: any change that moves that number has
  changed the workload, not the defect.

## The instrument was fixed first (2026-09-08)

`gc_quiescence`'s `LIVENESS_FILTERED` map is keyed by address with no
invalidation on re-serve, and `reclaim_guard` printed its hit as though the
filter's contract had been broken ("The filter guarantees such a slot is never
read again; it was"). On this workload that sentence was printed about an
address the filter had dropped for a DIFFERENT object hundreds of cycles
earlier — the attribution above.

Entries now carry the collection they were made on, and the report prints
`filtered_on`, `heap_collection` and `collections_since`. **Only
`collections_since=0` is a reading worth acting on**; anything larger says the
allocator may have re-served the address and the frame named is about another
object. Re-run this repro and check that field before spending anything on the
`URLClassPath.<init>` lead.

## Next

1. ~~Re-run with the age-stamped ledger and read `collections_since`.~~ Done:
   `collections_since=44`. The `URLClassPath` attribution is noise; do not
   spend anything on it.
2. Print `pointer_map` membership for the eight slots at the moment
   `was_vacated` fires — that separates "moved and not rewritten" from "freed,
   and the vacated ledger is lying".
3. If they ARE in the map: find which frame set the remap walked. All eight are
   bootstrap frames (`<clinit>` and JDK runtime setup) at `frame=0..7`, so a
   nested-initialization frame stack the remap does not reach is the first
   thing to check.
