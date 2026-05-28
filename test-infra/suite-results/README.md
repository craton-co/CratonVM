# Suite results

Persistent log of each gauntlet suite's runtime + pass/fail under CratonVM,
HotSpot (JDK 25 baseline), and TornadoVM (Graal + PTX argfile).

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
