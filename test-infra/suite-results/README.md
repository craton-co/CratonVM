# Suite results

Persistent log of each gauntlet suite's runtime + pass/fail under CratonVM,
HotSpot (JDK 25 baseline), and TornadoVM (Graal + PTX argfile).

## Default regression suite — `test-infra/regression-suite.sh`

The canonical regression gate for routine use. Runs two suites and appends one
`history.tsv` row per suite per run, flagging any suite >=20% slower than its
previous same-variant run (speed regression).

```
bash test-infra/regression-suite.sh                 # cratonvm (default)
bash test-infra/regression-suite.sh --variant hotspot
```

1. **BouncyCastle core** (functional). Green set gated here: `bc-math-raw`,
   `bc-util-encoders`, `bc-util-utiltest`, `bc-crypto-threshold` — all pass on
   both CratonVM (JIT) and HotSpot. Broader/known-failing BC suites
   (asn1-regression locale fails, crypto-prng HMacDRBG, crypto.test/math.ec/
   math/pqc timeouts) stay in `run-bc-core-local.sh` until green.
2. **Apache Commons Math** (numeric, full reactor via Surefire, ~3640 tests).
   Run under CratonVM via a generated JVM shim (`-Djvm=`); under HotSpot
   directly. NOTE: a few Commons Math 4.0-SNAPSHOT tests are tolerance/JDK-25
   sensitive (auto-retried as Surefire flakes; FastSineTransformer fails on
   HotSpot too) — so the bar is **CratonVM pass-count >= HotSpot pass-count**,
   not 100% green.

Needs `cargo build --release -p cratonvm-cli` first, and Maven (auto-detects the
IntelliJ-bundled mvn; override with `$MVN`). First Commons Math run is online to
warm `~/.m2` (incl. the Surefire provider); later runs reuse the cache.

## Files

- `history.tsv` — append-only log; one row per run.
  Columns: `iso_date  suite  variant  rc  pass  total  wall_s  notes`
  - `variant` ∈ {cratonvm, cratonvm-nojit, hotspot, tornadovm}
  - `pass/total` are blank for benchmark-style runs (DaCapo) and startup-only
    checks (Eclipse) where the pass/fail concept doesn't apply.
- `latest.md` — auto-regenerated table of the most recent run for each
  (suite, variant) pair, intended for quick at-a-glance comparison.

## Suites tracked

| Suite | Type | What it exercises |
|---|---|---|
| `bc-asn1-regression` | functional | BouncyCastle ASN.1 RegressionTest (58 SimpleTest classes) |
| `bc-math-ec` | functional | BouncyCastle math.ec JUnit-3 AllTests |
| `commons-math` | functional | Apache Commons Math 3204 JUnit tests via Surefire |
| `h2-driver-probe` | functional | H2 `DriverManager.getConnection` smoke probe |
| `eclipse-ecj-start` | startup | ECJ `BatchCompiler.compile` smoke |
| `dacapo-lucene` | benchmark | DaCapo Lucene indexer (no pass/fail; wall time only) |
| `regression-pool` | functional | 14-probe permanent suite (smoke gauntlet) |

## Convention

When fixing CratonVM-side failures: the cratonvm row goes into history *for
every* commit that changes its outcome (pass count or wall time ≥ 20%). The
HotSpot row is captured once after a CratonVM suite reaches its first
green/stable state, then re-captured only when CratonVM moves to a new
green/stable state (so the comparison is meaningful and ratios don't drift).
TornadoVM rows likewise — captured at green-stable points only.
