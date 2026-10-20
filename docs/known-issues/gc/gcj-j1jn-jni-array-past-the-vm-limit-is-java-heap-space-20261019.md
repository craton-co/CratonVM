# A JNI array request past the VM limit collects and reports `Java heap space`

> **STATUS (2026-10-19, gcj j2/jn2): residuals re-checked; the fix is unchanged (block AL passed on j1).** Open: (a) G1 and ZGC keep the pre-fix shape (the `is_generational()` gate of `jni_array_refused_for_its_length`), out of scope; (b) the String doors -- `NewString` / `NewStringUTF` past `2^30` UTF-16 units still reach the ladder and `Java heap space` (no probe: the C buffer alone is 2 GiB); recorded on `docs/internal/gc/gcj-j2jn2-jni-new-string-edges-are-not-hotspots-FIXED-20261020.md`, whose other two `NewString` edges (length 0 with a NULL pointer, a negative length) gcj j2/jn2 fixed.

> **STATUS (2026-10-19, gcj j1/jn): FIXED IN CODE, default ON on Generational (`CRATONVM_JNI_ARRAY_VM_LIMIT=0` kills), PENDING BUILD/PROBE.** Deciding rows: block AL of `docs/internal/gc-round-gcj-20261019/j1-jn-report.md` (`jn1_al_*`). G1 and ZGC unchanged (out of the gcj round's scope): their owners should take the same one-line gate off.

*Filed 2026-10-19 by gcj j1/jn, by reading.*

## What is wrong

`NewObjectArray` and `New<Type>Array` (`vm/src/native/jni.rs` `jni_new_object_array`, the
`new_prim_array!` bodies) handed every non-negative length to `jni_alloc_or_oom_sized`. For a length
above HotSpot's limit (`Integer.MAX_VALUE - 2` for every element type on 64-bit,
`gc_and_alloc::MAX_JAVA_ARRAY_LENGTH`) the allocator refuses at once
(`cratonvm_gc::vm_heap::VM_MAX_ARRAY_LENGTH`), and the JNI door then ran the whole native reclaim
ladder -- a forced young collection, the last-ditch reclaim and a second major, three collections a
`-Xlog:gc` run prints and a `GarbageCollectorMXBean` counts -- retried, and left
`OutOfMemoryError: Java heap space` pending.

HotSpot (`TypeArrayKlass::allocate_common` / `ObjArrayKlass::allocate` ->
`check_array_allocation_length`) throws `OutOfMemoryError: Requested array size exceeds VM limit`
before the heap is touched, with no collection (`report_java_out_of_memory` still runs:
`-XX:+HeapDumpOnOutOfMemoryError`, `OnOutOfMemoryError`, `ExitOnOutOfMemoryError`). The interpreter's
`newarray`, the JIT helpers and the VM's own native array doors already refuse such a length first
(`array_length_exceeds_vm_limit`); only the JNI doors did not.

## The fix

`jni_array_refused_for_its_length` runs first in both doors: on a Generational heap with the switch
on, a length above the limit leaves HotSpot's error pending through `raise_jni_oom_message` (the OOME
chokepoint with that message, which also keeps the native funnel's OOME debt unarmed: it arms only on
`Java heap space`) and the function returns `NULL`. A length inside the limit is unchanged.

## How to verify

Block AL (`Gcj1JnArrayLimitProbe`): `jn1_al_def` / `jn1_al_compat` SAME (six lines, four `gcs=0`);
`jn1_al_kill` (`=0`) DIFF with `Java heap space gcs=>0` on the four limit lines. Unit test:
`cargo test -p cratonvm-vm --lib gcj1jn_the_jni_array_length_refusal_is_hotspots_limit`.
