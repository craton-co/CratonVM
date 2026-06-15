# spring-bug-09: OOB field read on `Collections$EmptyMap` (speculative collection-layout probe) → crash/hang

| | |
|---|---|
| **Category** | **VM-CRASH / VM-HANG** (GC / field access) |
| **Module** | spring-expression |
| **Test class** | `org.springframework.expression.spel.support.ReflectiveIndexAccessorTests` |
| **CratonVM** | CRASH (rc=139 SIGSEGV in batch) / TIMEOUT (rc=124 in isolation — guard drops the read, then spins) |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 (also reproduces on dev + my fixes) |
| **Status** | **CRASH FIXED on dev** — `5941addd` (merge `12c22ec5`). Residual *separate* hang remains (see below). |
| **Suggested owner** | me (crash done); residual hang likely the known perf pathology |

## Verified
After the fix: **0** `EmptyMap` OOB warnings; `HashMap`/`EmptyMap`/`singletonMap`/`LinkedHashMap`
`size`+`get` all correct (no regression). The SIGSEGV / memory-safety bug is gone.

**Residual:** `ReflectiveIndexAccessorTests` still TIMEOUTs (rc=124) — but now with **no OOB** — so
a *separate* slow-path/hang remains (HotSpot runs its 5 tests in well under a second). Most likely
the known interpreted-instance-method perf pathology (cf. memory `jit-instance-methods-no-invocation-tierup`),
not a second memory bug. Tracked as residual; the crash itself is resolved.

## ROOT CAUSE — PINPOINTED + fix applied
`native-collections/src/lib.rs` `map_state()` (the HashMap-layout reader returning
`(buckets, size, cap)`) is invoked on **any** Map receiver. For a non-synthetic JDK map with <3
slots — `java/util/Collections$EmptyMap` has only AbstractMap's 2 ref slots — two fallbacks read
**absolute slot 2** unguarded:
- the `size` "ancient fallback" `ctx.get_field(this, 2)`,
- the `cap` `ctx.get_field(this, MAP_FIELD_CAPACITY /*=2*/)` (taken because `buckets` is `None`).
Both OOB-read slot 2 on the 2-slot EmptyMap. The `gc::guard` drops the read in-process (→ the hot
SpEL `ReflectiveIndexAccessor` map-index path spins on the repeated bad read → TIMEOUT in
isolation), but under the batch JVM it reached a real `EXCEPTION_ACCESS_VIOLATION` (rc=139).

**Fix:** bound both slot-2 probes on `ctx.object_num_fields(this) > 2` (falling back to size 0 /
default capacity for maps without that slot — correct for EmptyMap). Mirrors the receiver-layout
guards already applied to the *collection* probes in the same file.

## Symptom
stderr floods with, then the process SIGSEGVs (or spins until the watchdog):
```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
  (caller used slot index past receiver's layout — class layout is correct; the bug is in
   the caller's slot computation, typically a speculative collection-layout probe dispatched
   on a non-matching receiver type)
  obj=0x… index=2 num_slots=2 class_id=ClassId(472)
  class_name=java/util/Collections$EmptyMap real_field_count=Some(2)
```
CratonVM reads **field slot index 2** of a `java/util/Collections$EmptyMap` instance that has only
**2 slots** (valid indices 0–1). The `gc::guard` catches and drops the read in isolation (so the
program loops on the repeated bad read → TIMEOUT), but under the batch JVM it reached a real
`EXCEPTION_ACCESS_VIOLATION` (rc=139). The guard's own message names the cause: a **speculative
collection-layout probe** (code that assumes a particular `Map`/collection field layout) is
dispatched on a receiver whose real type (`EmptyMap`) doesn't match that layout.

## Reproduce
```bash
SE=.../spring-expression
CP="$H;$(tr -d '\r' < $SE/build/cratonvm-testcp.txt)"
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.expression.spel.support.ReflectiveIndexAccessorTests
"$JDK\bin\java.exe" -cp "$CP" KRun ...ReflectiveIndexAccessorTests   # OK
```

## Suspected root cause / fix
A CratonVM fast-path that probes a collection's backing field by a fixed slot index (e.g. reading
`HashMap.table` / a `Map.Entry` field) is invoked with an `EmptyMap` (or other non-`HashMap` Map)
receiver and reads slot 2 it doesn't have. Find the caller behind the `gc::guard` message
(`gen_heap::get_field` OOB on a collection) — likely a `Map`/`Collection` intrinsic or an
inline-cache collection-layout assumption in the interpreter/native dispatch. Fix = verify the
receiver's actual class (or slot count) before the speculative read, falling back to the generic
path for non-matching types. The guard already prevents memory corruption in-process but not
reliably (batch run still crashed), so the real fix is at the caller.

## Investigation notes
The well-guarded `collect_collection_elements` (`native-collections/src/lib.rs:16835+`, every probe
checks `n_fields`) is **not** the culprit — the receiver is a **Map** (`EmptyMap`), so the OOB read
comes from a **Map-layout probe** (SpEL `ReflectiveIndexAccessor` indexing a map reads a `HashMap`
field slot — e.g. `table`/`size` at index 2 — on an `EmptyMap` that only has the 2 `AbstractMap`
slots). Pinpoint the exact caller by running with `RUST_BACKTRACE=1` and breaking on the
`gc::guard` OOB path, or grep map-indexing intrinsics for an unguarded `get_field(map, 2)`. Fix =
gate the map-layout fast-path on the receiver's real class / `object_num_fields` before the read
(same pattern already applied to the collection probes).

A genuine **crash** (top-priority). The repeated-read spin also makes it a hang. Related to GC/heap
field-access; cf. memory `precise-jit-maps-bk-status`. Good candidate for me or a GC-focused handoff.
