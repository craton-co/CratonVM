# `ByteArrayMappingTests` SIGSEGV — Stream layout-probe OOB escapes the gen_heap guard

**Severity:** High (hard crash, `EXCEPTION_ACCESS_VIOLATION` / SIGSEGV, rc=139).
**Status:** 🔴 OPEN — deterministic, reproduces solo on latest dev. Handoff/investigate.
**Mode:** Interpreter (JIT-off; `CRATONVM_DISABLE_JIT=1`).
**HotSpot (JDK 25):** PASS.

## Symptom

`org.hibernate.orm.test.mapping.basic.ByteArrayMappingTests` crashes the VM:

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF720686994
```

The crash is preceded by a **flood** (hundreds) of identical gen_heap guard WARNs:

```
WARN cratonvm::gc::guard: gen_heap::get_field / set_field: out-of-bounds field read/write dropped
  (caller used slot index past receiver's layout)
  obj=0x… index=1 num_slots=1 class_id=ClassId(275) class_name=java/util/stream/Stream real_field_count=Some(0)
```

i.e. something repeatedly reads/writes **slot index 1 on a `java/util/stream/Stream` object that has 0/1
slots** ("speculative collection-layout probe dispatched on a non-matching receiver type"). The
`gen_heap::guard` *drops* each individual OOB access, but the underlying mis-dispatch eventually performs a
raw access that the guard does **not** intercept → hard SIGSEGV. The hs_err frame shows a ShadowStack /
native-frame context (VM internals), not a Java NPE.

## Repro

```
CRATONVM_DISABLE_JIT=1 cratonvm --java-home <jdk25> @common.args -Dcraton.batch=1 \
  CratonRunner <list-with-only ByteArrayMappingTests> 0
```
Reproduces **strictly alone** (census fully drained, 0 other CV procs) → rc=139. Not contamination.
DDL + first inserts run (`create table EntityOfByteArrays`, `EntityOfByteArrays` insert) before the fault,
so the crash is during entity/Stream processing, not bootstrap.

## Hypothesis / next step

A native collection/stream method is dispatched on a `java/util/stream/Stream` receiver as if it had a
backing-array/extra slot (slot index 1 on a 1-slot object). The `gen_heap` guard masks the symptom but a
sibling raw-pointer path (likely the same native, or a JIT/interp fast-path) dereferences the bad slot and
SIGSEGVs. Trace: find the native that reads slot 1 of `java/util/stream/Stream` (ClassId 275) — the WARN
fires from `gen_heap::get_field`/`set_field`; instrument the caller (a "speculative collection-layout probe")
to identify which Stream op on `byte[]`/`Byte[]` mapping triggers it, then make the probe receiver-type-aware
(bail when `real_field_count==0`) instead of relying on the post-hoc guard.

## Related

Distinct from the JSON-function `al_state` foreign-receiver SIGSEGV (fixed this session) — that was a native
reading ArrayList slots off a non-ArrayList; this is a Stream layout-probe. Same *family* (native slot
computation on a non-matching receiver), different native. The sibling crash
`onetoone.nopojo.DynamicMapOneToOneTest` exits **rc=127** (abnormal exit, not SIGSEGV) and also reproduces
solo — likely a separate dynamic-map (`Map`-backed entity, no POJO) issue; not yet triaged.
