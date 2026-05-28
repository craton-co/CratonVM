# Suite results — latest snapshot

Generated 2026-05-28. Times in seconds, wall-clock.

## Functional suites (pass/total)

| Suite                | CratonVM (rc, time)  | HotSpot (time)       | TornadoVM (time)     |
|----------------------|----------------------|----------------------|----------------------|
| bc-asn1-regression   | 58/58 — 141.9 s      | 58/58 — 0.9 s        | 58/58 — 3.4 s        |
| h2-driver-probe      | 1/1 — 6.4 s          | 1/1 — 1.0 s          | 1/1 — 1.5 s          |
| commons-math         | 3204/3204 — 154 s    | 3204/3204 — 96 s     | 3204/3204 — 118 s    |
| spring-boot-probe    | 1/1 — 2.6 s          | 1/1 — 1.4 s          | 0/1 — 0.9 s †‡       |
| bc-math-ec           | 7/14 — 491 s †       | 14/14 — 31.3 s       | 14/14 — 52.0 s       |
| h2-testall           | 0/1 — 122.1 s †§     | 0/1 — 212.1 s §      | (not tested)         |
| regression-pool      | 14/14 — ~20 s        | n/a                  | n/a                  |

† CratonVM `--nojit` required where noted.
‡ TornadoVM fails on `IncompatibleClassChangeError` in log4j2 lambda — Graal-specific quirk, not CratonVM.
§ Both CratonVM and HotSpot fail H2 TestAll: CratonVM hits the issue #23 JIT family;
HotSpot has 4 upstream JDK 25 incompat tests (TestFunctions / TestPreparedStatement /
TestCrashAPI / TestOutOfMemory — known per `continue_prompt.md`).

## Startup checks

| Suite                | CratonVM (rc, time)  | HotSpot (time)       | TornadoVM (time)     |
|----------------------|----------------------|----------------------|----------------------|
| eclipse-ecj-start    | starts† — 1.6 s      | ok=true — 1.2 s      | ok=true — 1.5 s      |
| tomcat-bootstrap     | partial‡ — 30 s      | clean — 1.4 s        | clean — 8.5 s        |

† `--nojit` required. With JIT, issue #23 fires inside `HashtableOfInt.put`.
‡ Catalina engine starts in 252 ms but HTTP Connector fails:
`LifecycleException: invalid Lifecycle transition [after_start] in state [STARTING_PREP]`
— `startInternal` threw and was swallowed before the state advanced.

## Benchmarks (wall-time only)

| Suite                | CratonVM             | HotSpot              | TornadoVM            |
|----------------------|----------------------|----------------------|----------------------|
| dacapo-lucene        | SEGV at 18.4 s †     | 23.5 s (pass 20.5 s) | 17.0 s (pass 14.1 s) |

† Inline-alloc header-init JIT bug (issue #23 family). `--nojit` runs ~18.8 s
but fails at reflective `LuIndex.iterate` dispatch (speculative
collection-layout probe).

## Headline numbers

**CratonVM passes** (matches HotSpot result):
- bc-asn1-regression 58/58 (140× slower than HotSpot)
- h2-driver-probe 1/1 (6.6× slower)
- commons-math 3204/3204 (1.6× slower)
- spring-boot-probe 1/1 (1.9× slower)

**CratonVM partial** (suite-specific limitations):
- bc-math-ec 7/14 (`--nojit`, residual EC arithmetic gaps)
- eclipse-ecj-start (`--nojit`)
- tomcat-bootstrap (engine starts, HTTP Connector fails)

**CratonVM red** (JIT issue #23 family):
- dacapo-lucene (benchmark)
- bc-math-ec (with JIT)
- eclipse-ecj-start (with JIT)
- h2-testall

## Open issues per suite

- **eclipse-ecj-start (with JIT)** / **dacapo-lucene** / **bc-math-ec** /
  **h2-testall** — all hit the same JIT inline-allocate-then-putfield
  miscompile family documented in `continue_prompt.md` issue #23 / #6.
  Manifests as `kind=Object && array_length=<garbage>` walker corruption;
  needs a Windows-side debugger watchpoint (lldb/windbg) to localise the
  responsible codegen path. Defensive headers + skip-list bandaids don't
  help — the corruption happens upstream of the failure site.
- **bc-math-ec (`--nojit`)** — even with JIT off, BC's `Mod.modOddInverse`
  (hand-rolled int[]-based modular inverse, bypasses BigInteger) produces
  wrong results for standard NIST curves. `bi_mod_str` sign-preservation
  fix unblocked the BigInteger path but the int[] arithmetic in
  `org.bouncycastle.math.raw.Mod` has its own gap. Next: bisect into the
  int[] primitives that `Mod` uses.
- **tomcat-bootstrap** — HTTP Connector startInternal throws something
  that gets swallowed. Catalina engine itself works. Investigation needs
  bytecode-level trace of NIO selector / SocketChannel native plumbing.
