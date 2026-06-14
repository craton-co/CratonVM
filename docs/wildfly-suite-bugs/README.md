# WildFly testsuite on CratonVM — crash collection

Running the **WildFly 41.0.0.Beta1 testsuite** (`apps/wildfly/testsuite`, ~1,600
test classes) under **CratonVM** to collect as many distinct VM crash reports as
possible. Methodology mirrors the kafka-clients sweep: **one CratonVM JVM per test
class** (via the `KRun` programmatic JUnit-Platform launcher) so a crash/hang in one
class cannot abort the others. Never stops on failure — every class is accounted for.

## Harness (worktree `C:\craton\CratonVM-wildfly`, branch `test/wildfly-suite`)
- `wildfly-suite/KRun.java` — per-class launcher; prints one flushed
  `RESULT <class> found=.. succ=.. fail=.. status=..` line per class, plus
  `FAILCAUSE`/`LOADERR` detail. Engine-agnostic (vintage = JUnit4, jupiter = JUnit5).
- `wildfly-suite/run-wildfly.sh` — driver. `GRAN`=class, batched per module with
  crash-recovery (a crashed batch JVM → remaining classes re-run individually).
  Boots **JDK 25** (`--java-home`) because CratonVM on a boot JDK <19 silently
  no-ops `new Thread(runnable)` → false hangs. External `timeout` is the sole hang
  detector (`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`). Resumable (skips classes already
  in `results.tsv`).
- `wildfly-suite/harness-cp.txt` — fixed junit platform/vintage/jupiter classpath.
- `wildfly-suite/gen-cp.sh` — emits per-module `target/cratonvm-testcp.txt` deps.
- `wildfly-suite/results/` — `results.tsv`, `crashes.log`, `failcauses.log`,
  `progress.log`, `summary.txt`.

## Status classes
`OK` (all pass) · `FAIL` (ran, ≥1 test/container failure) · `LOADERR` (class load /
discovery threw) · `EMPTY` (no tests found) · `ABEND` (JVM died with no RESULT —
**CratonVM crash**) · `TIMEOUT` (external timeout — **hang**) · `POST-RESULT` (result
emitted but JVM then crashed).

The **ABEND / TIMEOUT / POST-RESULT** classes are the crash reports of interest;
each distinct signature is written up as `bug-NN-*.md` here.

## Live status
See [STATUS.md](STATUS.md) for current exact numbers (updated as the run proceeds).

## Live-container reruns
See [live-container-smoke.md](live-container-smoke.md): with a live managed WildFly server
the smoke group goes from all-FAIL (no container) to **111/111 pass** on HotSpot. Running the
Arquillian **client under CratonVM** is currently blocked by two CratonVM gaps (Surefire
java-CLI args; null `Enumeration` in the client) — both documented there.

## Bugs
| # | Title | Sev | Status |
|---|-------|-----|--------|
| [01](bug-01-stream-foreachordered-abstractmethoderror.md) | `Stream.forEachOrdered` → `AbstractMethodError` "no Code attribute" (CratonVM-only; 27+ classes) | High | **FIXED** — repro'd + verified |
| [02](bug-02-zipfile-entries-null.md) | `ZipFile.entries()`/getName/size return null/0 (CratonVM-only; NPEs ShrinkWrap's package scanner — blocked the live-container client) | High | **FIXED** — repro'd + verified |
