# Suite results — latest snapshot

Generated 2026-05-28. Times in seconds, wall-clock.

## Functional suites (pass/total)

| Suite                | CratonVM (rc, time)  | HotSpot (time)       | TornadoVM (time)     |
|----------------------|----------------------|----------------------|----------------------|
| bc-asn1-regression   | 58/58 — 141.9 s      | 58/58 — 0.9 s        | 58/58 — 3.4 s        |
| h2-driver-probe      | 1/1 — 6.4 s          | 1/1 — 1.0 s          | 1/1 — 1.5 s          |
| commons-math         | 3204/3204 — 154.0 s  | 3204/3204 — 96.0 s   | 3204/3204 — 118.0 s  |
| bc-math-ec           | 7/14 — 491 s †       | 14/14 — 31.3 s       | 14/14 — 52.0 s       |
| regression-pool      | 14/14 — ~20 s        | n/a                  | n/a                  |

† `--nojit` required; with JIT, SEGV in inline-alloc header-init (issue #23 family).
Residual 6 errors + 1 failure trace to deeper EC arithmetic gaps after the
`bi_mod_str` sign fix.

## Startup checks

| Suite                | CratonVM (rc, time)  | HotSpot (time)       | TornadoVM (time)     |
|----------------------|----------------------|----------------------|----------------------|
| eclipse-ecj-start    | starts† — 1.6 s      | ok=true — 1.2 s      | ok=true — 1.5 s      |

† `--nojit` required (BatchCompiler reaches `compile(...)`; `ok=false`
because JRT FileSystem provider isn't wired). With JIT, issue #23
fires inside `HashtableOfInt.put` (upstream JIT bug, not in
HashtableOfInt itself).

## Benchmarks (wall-time only)

| Suite                | CratonVM             | HotSpot              | TornadoVM            |
|----------------------|----------------------|----------------------|----------------------|
| dacapo-lucene        | SEGV at 18.4 s †     | 23.5 s (passed 20.5s)| 17.0 s (passed 14.1s)|

† Inline-alloc header-init JIT bug (issue #23 family). `--nojit` runs
~18.8 s but fails at the reflective LuIndex.iterate dispatch (speculative
collection-layout probe).

## Open issues per suite

- **eclipse-ecj-start (with JIT)** / **dacapo-lucene** / **bc-math-ec** —
  all hit the same JIT inline-allocate-then-putfield miscompile family
  documented in `continue_prompt.md` issue #23 / #6. Manifests as
  `kind=Object && array_length=<garbage>` walker corruption; needs a
  Windows-side debugger watchpoint (lldb/windbg) to localise the
  responsible codegen path. Defensive headers + skip-list bandaids
  don't help — the corruption happens upstream of the failure site.
- **bc-math-ec (--nojit)** — even with the JIT off, BC's EC point
  multiplication produces incorrect coordinates for SECG/NIST standard
  curves. The `bi_mod_str` sign-preservation fix unblocked some paths
  but `BigInteger.modPow` (or another large-modulus operation) appears
  to have its own gap. Next: bisect with `BigInteger.modPow` /
  `BigInteger.gcd` regression probes.
