# DaCapo avrora — latent `gen_heap::get_field` out-of-bounds field read (WARN)

## Symptom
`dacapo.jar avrora` runs to completion on CratonVM (reaches & executes the
benchmark — an improvement over the prior ClassNotFoundException-at-boot), and
the DaCapo run prints `PASSED`. BUT the CratonVM stderr contains a recurring
guard warning:

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
     (caller used slot index past receiver)
```

Observed on both CratonVM-CPU (5.6 s) and CratonVM-GPU (5.8 s).

Note: DaCapo's own `Validation FAILED`/`PASSED` line is a **non-signal** — it
SHA-1-digests stderr against the empty-string digest, and JDK-25 writes warnings
to stderr, so it "fails" on HotSpot and TornadoVM too (rc=127). Do not chase the
validation line. The genuine CratonVM-specific artifact is the `get_field` WARN.

## Why it's a real (latent) bug
The GC guard is *dropping* an out-of-bounds field read — i.e. some caller is
computing a field slot index past the end of the receiver object's field block.
The guard prevents a SEGV/heap-corruption by returning a dropped/zero value, but
the underlying slot-index miscompute is a correctness hazard: in another object
layout it could read a valid-but-wrong slot (the exact class of bug seen in the
URI getter and VarHandle static-slot fixes — see those references). Today it is
masked by the guard; it should be root-caused.

## What an agent should try next
1. Set `CRATONVM_SYMBOLIZE=1` (and the relevant DBG gate) to get the Java
   caller + receiver class when the guard fires (see `reference_crash_debug_tooling`).
2. Identify which class/field access computes the over-large slot index. Likely
   a native that reads raw slots matching a synthetic layout rather than the
   real-bytecode object layout (cf. the URI getFragment slot bug, the VarHandle
   static-slot bug — both were "raw slot N" vs "by-name").
3. Fix the slot computation to read by field name/real layout. Confirm the WARN
   disappears and avrora still PASSES.

## Reproduce
```
cd apps/_test-suites/dacapo
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 4g \
    -jar dacapo.jar avrora 2>&1 | grep -i get_field
```

## Out of scope
Not related to the EC JIT fix.
