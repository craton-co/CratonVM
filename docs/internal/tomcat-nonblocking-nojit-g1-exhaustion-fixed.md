# Tomcat `TestNonBlockingAPI` no-JIT G1 exhaustion (fixed)

## Failure

The Group 16 no-JIT run could abort partway through the 44-method
`TestNonBlockingAPI` class with:

```text
FATAL: G1: out of heap space for array allocation (168 bytes)
```

The failure was specific to allocations made inside native callbacks.
`NativeContext::new_array` cannot safely collect while such a callback is
active because its native locals are not all GC roots, while the normal
interpreter allocation path was not reached to request collection.

## Resolution

`NativeContextImpl` now records a threshold crossing after a native array
allocation in interpreter-only mode. `safe_native_call_impl` consumes that
signal at the next native-call boundary, where its Java arguments are pinned
and remapped around the orchestrated collection. The signal is deliberately
disabled for JIT mode: JIT allocation uses its own safe collection paths, and
forcing collection at every native boundary regressed that path.

## Validation (2026-07-23)

The final binary was built from audit commit `21e8dfbcc` (including
`56306a25b`) with unique local and Azure target directories.

- Windows real-JDK Tomcat runner: JIT passed all 44 methods in 175.5 seconds.
  No-JIT completed all 44 methods in 145.525 seconds without a G1 abort.
- Azure real JDK 25 fixture: JIT passed all 44 methods in 242.561 seconds.
  No-JIT completed all 44 methods in 172.884 seconds without a G1 abort.

The Azure output is retained under:

```text
/data/data/tomcat-dohead-fixture-20260717/.suite/results/
g16-native-signal-azure-r2-jit-20260723/TestNonBlockingAPI.log
g16-native-signal-azure-r2-nojit-20260723/TestNonBlockingAPI.log
```

The no-JIT class still has the separate, documented
`ApplicationHttpRequest.<clinit>` `NoSuchElementException` reference-stream
residual (the expected HTTP 200 becomes 500). It is not an allocation failure.
