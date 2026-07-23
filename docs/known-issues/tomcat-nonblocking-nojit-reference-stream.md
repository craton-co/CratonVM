# Tomcat `TestNonBlockingAPI` no-JIT reference-stream materialization

## Status

Unresolved as of 2026-07-23. This is independent of the JIT HashMap cache
remap fixed in `968bb19b7`: the remaining failure is interpreter-only.

## Reproduction

Run Tomcat suite index 195 with a fresh CratonVM executable:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File `
  C:\craton\CratonVM\apps\tomcat-suite-runner\run-tomcat-suite.ps1 `
  -Category all -Start 195 -Count 1 -Jit off -Jdk real -Vm craton `
  -Parallel 1 -TimeoutSec 300 -Exe <cratonvm.exe>
```

`testNonBlockingReadWithDispatch` returns HTTP 500 because
`ApplicationHttpRequest.<clinit>` throws `NoSuchElementException` at:

```java
specialsMap.keySet().stream().mapToInt(String::length).min().getAsInt()
```

Fresh verification on 2026-07-23 with the clean audit executable recorded
`PASS` in 225.3 seconds with JIT and the expected no-JIT failure in 243.8
seconds (`expected:<200> but was:<500>`).

The fixture probe `apps/tomcat/.suite/HashMapClinitProbe` confirms that the
real `ReferencePipeline$Head` counts its twelve source keys, while its
`mapToInt(...).min()` is empty and `sum()` is zero only under `--nojit`.
The same probe returns `31` and `414`, respectively, under JIT.

## Narrowed cause

The VM's real-reference-stream materializer (`stream_elements`) calls the
pipeline's object `toArray()`. The interpreter loses the real pipeline source
on that path. Calling `AbstractPipeline.spliterator()` via `invokespecial` and
draining it likewise returned no elements in the focused probe, so it is not a
safe replacement. Do not force `Stream.mapToInt` to the eager native bridge
until its source extraction preserves real `ReferencePipeline` elements.
