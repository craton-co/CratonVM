# A JNI critical pin makes an object immovable and nothing else

**Status:** open. Not currently reachable as a crash — see "Why this has not
bitten" — but it is a liveness contract the collector does not honour.

## The finding

`ZgcRealHeap::pin_critical` records an address in `critical_pins`.
`critical_pin_addrs()` is the only reader of that map, and its only caller is
inside `relocate_stw`, where it feeds the relocation-set filter.

So a pin does exactly one thing: it stops the compactor from MOVING the object.
The mark phase never sees those addresses. An object whose only remaining
reference is the raw pointer a native holds is therefore unreachable — it is
swept, free-listed, and, since the reserve/commit store shipped, has its whole
granules handed back to the OS. The native's next access faults on `PROT_NONE`.

And on a DEFAULT run the pin is inert entirely, because relocation is opt-in
(`CRATONVM_ZGC_RELOCATE`): nothing consults `critical_pins` at all.

## Why it is a defect

`GetPrimitiveArrayCritical`'s contract is that the array stays valid until the
matching `Release`. That is a LIVENESS promise, not only an immovability one.
The addresses belong in the root set as well as in the relocation filter.

## Why this has not bitten

JNI's ordinary array access does not depend on it. `Get<Type>ArrayElements`
hands native code a detached COPY and mints a global reference for the source
as a keep-alive root (`vm/src/native/jni.rs`, "GC-correctness (vm-jni-roots
#2)"). That path is correct. The exposure is confined to callers that take the
critical pin and then hold the raw pointer across a safepoint.

## The fix, and what it needs

Feed `critical_pin_addrs()` into the mark phase's root enumeration as well, so
a pin is a root. The test to write with it is the one the current code would
fail: pin an array, drop every Java reference to it, force a collection, and
assert the bytes are still readable and the object still registered — on a
DEFAULT (non-relocating) configuration, where the pin is currently a no-op.

## How a regression here will announce itself

`reservation::recent_decommit_covering` (2026-09-04) makes the crash
self-diagnosing: a fault inside a span the collector handed back now prints
`site=free-list-low` / `free-list-high` and "NOT re-committed since" in the
crash report. A missing root shows up as a `free-list-*` site; a cursor that
retracted over live bytes shows up as a `*-retract` or `unbumped-middle` one.
