# GC: moving collector misses a lost-tag interpreter operand-stack/local root (teardown "all-zero header")

**Status:** 🔴 **OPEN** (benign in practice, but a real GC-correctness defect). Found 2026-06-20 while
finishing the ES `RestClientSingleHostIntegTests` suite (branch `fix/es-restclient-gc-safety`).
`--nojit` (the **moving** Cheney young collector), GC-pressure-dependent.

## Symptom

Under `-Xmx1g` GC pressure, during the multi-threaded suite **teardown**, the VM logs a burst of:

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped … obj=0x… index=0
     num_slots=0 class_id=ClassId(0) class_name=java/lang/Object real_field_count=Some(0)
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver
     (ptr=0x…, all-zero header) — falling back to CP class java/lang/StringBuilder
```

i.e. a live object's header has been **zeroed** (`class_id=0 num_slots=0 array_length=0`) while a frame
slot still references it. It is usually **benign**: the `gen_heap::get_field` guard drops the
out-of-bounds access and the run stays green (the suite was green 12/12 even with 10 such corruptions in a
run). It becomes visible as the `StringBuilder.flush` `NoSuchMethodError` and the
`RandomizedContext.randomnesses` / `Thread.group` NPEs that `ThreadLeakControl.formatThreadStacks` emits
when it happens to fire *during* a leak report — those are downstream of this corruption, not separate bugs.

## Root cause (localized deterministically)

A live young object is **not marked** by the moving collector (its only reference is a frame slot whose
**tag is not `Object`** at snapshot time, so `scan_local_objects` / `ValueStack::scan_object_refs` skip it),
so it is not copied; the young-from semispace is then reset over it → zeroed header → the still-live
reference reads zeros.

It is **NOT** the `rs_cache` frozen-frame snapshot cache (it persists with `CRATONVM_ROOTSNAP_CACHE=0`) and
**NOT** a missed *remap* (the initiator-only `verify_no_stale_refs` never fires for it). A new gated
per-PARKED-thread verifier (see below) pins it to a single, **100% consistent** site:

```
POST-GC ZERO-HEADER PARKED tid=2 frame[13]
    com/carrotsearch/randomizedtesting/RandomizedRunner.invoke local[3] pc=72   (15/15 identical)
```

So `RandomizedRunner.invoke`'s `local[3]` holds a live Object that the moving collector failed to scan as a
root at some collection. (Post-GC the slot reads back as `Value::Object`, so the *value* is an object ref;
the lost tag is transient at the marking snapshot — a `astore`/dup/stack-shuffle that left the slot tagged
non-Object at a safepoint boundary is the suspect.)

## Diagnosis tooling added (`fix/es-restclient-gc-safety`)

`CRATONVM_GC_VERIFY_STALE=1` previously only checked the **GC initiator's** frames
(`verify_no_stale_refs`). It now also walks every **non-initiator** thread's own frames as it resumes from
the STW barrier (`apply_pointer_map_to_thread`) and prints
`POST-GC ZERO-HEADER PARKED tid=… frame[…] CLASS.METHOD local[…] pc=…` for any Object slot whose header is
zeroed (commit `diag(gc): per-parked-thread post-GC zeroed-header verifier`). That is what localized this.

## Reproduce

```bash
cd apps/elasticsearch/client/rest
export CLASSPATH="$(cat build/craton-testcp.txt)"
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_GC_VERIFY_STALE=1 \
  cratonvm.exe --nojit -Xmx1g org.junit.runner.JUnitCore \
  org.elasticsearch.client.RestClientSingleHostIntegTests 2>&1 | grep "POST-GC ZERO-HEADER PARKED"
```
~50–70 % of `-Xmx1g` runs print it (`-Xmx6g` never does — no young GC).

## Next step

Decompile `RandomizedRunner.invoke` around pc=72 (`local[3]`) and find the bytecode that leaves the slot's
compact tag non-`Object` at the safepoint where the collection's snapshot is taken. The fix is in the
interpreter's tag bookkeeping (or a conservative-scan fallback that, unlike the JIT one, is safe on the
moving path only if the value really is a heap object — which `is_object_address` can confirm). Cf. the
"lost-tag interpreter local" noted under Family-A **A4** (different context — FJP/non-moving sweep — but the
same *class* of bug).

## Related

- The reactor-thread leak it co-occurs with is a SEPARATE bug: [gc-rscache-reactor-shutdown-timing-race.md](gc-rscache-reactor-shutdown-timing-race.md).
- Family-A GC-root coverage (JIT non-moving sweep): this README's Family-A section.
