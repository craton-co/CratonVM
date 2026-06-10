# JUnit-Platform console launcher — junit-help FAIL + Commons Math reactor crash

## Symptom
Two related failures, same root (the JUnit-Platform console launcher path):

1. **extras / junit-help** — `cratonvm -jar junit-platform-console-standalone-1.10.2.jar --help`
   - CratonVM-CPU: rc=0 but **state FAIL** (14.6 s) — the `Usage: junit …` help
     banner is never emitted; harness requires both a clean rc AND the banner.
   - CratonVM-GPU: same FAIL (13.9 s).
   - HotSpot / TornadoVM: OK 0.4–0.5 s, prints `Usage: junit [OPTIONS] [COMMAND]`.

2. **commons-math full reactor (CratonVM column)** — rc=127, **state CRASH**,
   tests=NA. The JUnit-Platform console launcher cannot drive the suite. (The
   harness notes the known blocker `NoSuchMethodError java/io/BufferedWriter.write([BII)V`.)

Reproduce (help path):
```
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 4g \
    -jar .bench-cache/junit-platform-console-standalone-1.10.2.jar --help
```

## TODO: capture the exact stack
The harness deletes per-test temp logs, so the precise exception was not saved
in `run-full-20260604.log`. Re-run the command above and capture the first
exception. Prior art points to **`NoSuchMethodError: java.io.BufferedWriter.write([BII)V`**
(a missing/incorrectly-shaped intrinsic or missing real-bytecode method on the
BufferedWriter → Writer chain) and/or a `Charset` index-out-of-bounds in the
launcher's ANSI/encoding setup (see `reference_junit5_console_launcher`:
"Commons-Math/JUnit5 console path no longer CRASHES (rounds 1-2) but jupiter
discovery silently finds 0 tests"). The two observations may have diverged —
re-establish the current exact failure.

## Likely root cause
`java.io.BufferedWriter.write(char[]|byte[], int, int)` — the 3-arg bulk write
on the buffered writer used by the console launcher's `PrintWriter`/`System.out`
path resolves to a native/intrinsic that does not match the real JDK-25 method
shape (descriptor `([BII)V` vs `([CII)V`), so the launcher aborts before
printing. No synthetic stub allowed — fix the underlying method resolution /
add the correct real intrinsic so real `BufferedWriter` bytecode runs.

## Why it matters
This is the single blocker preventing CratonVM from running the **Commons Math
JUnit5 reactor at all** (3204 tests) — the suite passes 3204/0-fail on both
HotSpot and TornadoVM. Fixing the launcher unlocks a large correctness signal.

## What an agent should try next
1. Re-run the `--help` path, capture the exact `NoSuchMethodError` / exception
   + the Java stack (`--stack-dump-on-timeout 0` already set; it prints on
   uncaught throw).
2. Trace the `BufferedWriter.write` resolution: is a native overriding the real
   method? Is the descriptor wrong? (grep native-io / native-builtins for
   `BufferedWriter` / `write`.)
3. Verify against the real JDK-25 `BufferedWriter`/`Writer`/`PrintWriter` method
   table. Prefer running real bytecode; only add an intrinsic if proven needed.
4. Then re-attempt the JUnit5 discovery (the "finds 0 tests" secondary issue may
   resurface — track separately).

## Out of scope
Not related to the EC JIT fix.
