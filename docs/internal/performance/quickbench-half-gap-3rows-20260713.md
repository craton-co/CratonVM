# QuickBench three-row half-gap closure (2026-07-13)

Status: fixed and verified with default settings.

## Goal and acceptance rule

For at least three README rows, reduce the excess time over HotSpot by half:

`new CratonVM <= HotSpot + (old CratonVM - HotSpot) / 2`.

The original README targets were therefore 7,043.5 ms for Arithmetic, 6,047.5 ms
for Fibonacci, and 6,493.5 ms for Matrix.

## Runtime fixes

1. OSR bodies now have a cache independent from normal method-entry C1/C2 bodies.
   Publishing a method-entry upgrade can no longer evict the artifact a hot
   interpreter frame needs at its back edge.
2. Waiting for an off-thread OSR compile no longer consumes the frame's five
   permanent rejection attempts. The frame restarts its polling stride while the
   worker is pending; genuine compile failures enter the OSR deny list. A normal C2
   body also no longer suppresses a separately required OSR body.
3. A narrow, resolved `static int f(int)` self-recursion shape is admitted directly
   to optimized IR on its first background compile. Recursive edges call the same
   compiled entry directly. A 64 KiB native-stack boundary sample invokes the
   existing StackOverflowError guard with at least 960 KiB of its 1 MiB headroom
   remaining, avoiding a helper call in every ordinary recursive frame.

## Benchmark evidence

Host: Azure Linux `20.83.144.174`, logical CPU 14 (`taskset -c 14`). HotSpot:
Temurin 25.0.3 C2. CratonVM candidate binary:
`cratonvm-perf-osr-selfrec-75f11d95-20260713-004.bin`, SHA-256
`26d634e04cda92b2e5f0d67e8d9d18ea44cc51caed2657b9feeff995d8839536`.
Every measurement was a fresh process with no performance override variables.

| Row | HotSpot runs (ms) | CratonVM runs (ms) | Median pair | Original gap reduction | Result |
|---|---:|---:|---:|---:|---|
| Arithmetic (2B) | 2,079 / 2,071 / 2,075 | 4,190 / 4,161 / 4,160 | 2,075 / 4,161 | 76.4% | PASS |
| Fibonacci(44) | 1,443 / 2,538 / 2,486 | 4,393 / 4,280 / 4,283 | 2,486 / 4,283 | 72.6% | PASS |
| Matrix 1280x1280 | 2,089 / 2,099 / 2,104 | 5,916 / 5,882 / 5,909 | 2,099 / 5,909 | 50.8% | PASS |

Checksums were identical in every run:

- Arithmetic: `99414225882916859`
- Fibonacci: `701408733`
- Matrix: `173943680`

The same session measured Sieve at a 16,711 ms median versus HotSpot's 2,738 ms;
it is reported in the README refresh but was not counted as one of the three passing
rows. String/Regex was also rechecked and discarded as a candidate after the exact
10K probe reproduced around 329-340 ms rather than the earlier optimistic observation.

## Correctness and regression coverage

- JIT cache coexistence: an OSR artifact survives method-entry C2 replacement.
- Tier-manager independence: method-entry C2 still permits an OSR request.
- Pending-OSR accounting: polling restarts without spending a rejection attempt.
- Scalar self-recursion structural admission and direct-call execution tests,
  including int and long return paths.
- Full VM type-check.
- Unbounded optimized `(I)I` recursion completed by throwing and catching
  `StackOverflowError` (`STACK_OVERFLOW_CAUGHT`).
- Five consecutive default Arithmetic 300M stress runs completed in
  623-625 ms with identical checksums; the pre-fix race intermittently timed out.
