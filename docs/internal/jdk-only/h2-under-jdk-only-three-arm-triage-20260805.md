# H2 under `--jdk-only`: three-arm triage of the first 60 classes

| | |
|---|---|
| **Status** | OPEN — triage, not diagnosis. Three strict-only regressions named, nine both-mode failures separated out |
| **Opened** | 2026-08-05, L8 (criterion 6: the strict corpus is green) |
| **Scope** | `org.h2.test.*`, the first 60 classes of `meta/all-classes.tsv` (of 218) |

## Why three arms

A CratonVM failure that also fails under `--real-jdk` is a compatibility defect
that `--jdk-only` did not introduce; the wave-2 lane keeps rediscovering that
and it is the reason the protocol says HotSpot plus BOTH modes. Running only
strict-vs-HotSpot here would have produced twelve "strict-mode defects", nine
of which are not.

```sh
# all three, in one detached script; hotspot first so a broken control is
# noticed before anything is compared to it
CRATONVM_PREFIX_BIN=<binary> CRATONVM_PREFIX_ARGS=--jdk-only \
CRATONVM_BIN=scripts/cratonvm-prefix-args.sh \
OUTROOT=<out> apps/h2database-suite-runner/run-h2-suite.sh run --category all --count 60
```

`../../../scripts/cratonvm-prefix-args.sh` exists because the runner has no hook for VM
flags but does take the binary path from `CRATONVM_BIN`. **Verify the policy
actually reached the VM** — 60/60 strict logs carry the `--jdk-only` banner and
0/60 real logs do. A run.sh that stops honouring the variable turns this into a
compatible-mode run whose green says nothing, with no other symptom.

## Result

| arm | PASS | FAIL | HANG |
|---|---:|---:|---:|
| HotSpot 25 | 58 | 2 | 0 |
| CratonVM `--jdk-only` | 46 | 6 | 8 |
| CratonVM `--real-jdk` | 47 | 5 | 8 |

`TestFunctions` and `TestOutOfMemory` fail on HotSpot too, so they are upstream
H2 on this fixture and are not counted against the VM.

### Strict-only — the three this lane is actually about

| class | `--real-jdk` | `--jdk-only` | deepest frame in the strict log |
|---|---|---|---|
| `TestDuplicateKeyUpdate` | PASS | FAIL | `java/lang/StringLatin1.toUpperCase`, under `StringUtils.toUpperEnglish` from the tokenizer |
| `TestFullText` | PASS | FAIL | `MappedByteBufferIndexInputProvider.map` — Lucene's mmap `IndexInput` |
| `TestRights` | PASS | FAIL | `Database.checkWritingAllowed` raising `JdbcSQLNonTransientException` during `setMasterUser` |

These are the strict-mode work list. The `toUpperCase` one is the most likely
to be a small, shared root cause: it is a `java.lang.String` path, in a mode
that has just had its `String` natives reduced, reached from ordinary SQL
tokenization — so it will be hit by far more than this one test.

### Both modes — pre-existing, NOT strict-mode defects

`TestCases`, `TestCluster`, `TestIndex`, `TestLIRSMemoryConsumption`,
`TestLargeBlob`, `TestLob`, `TestMultiThread`, `TestOpenClose`,
`TestOptimizations`.

Several already have their own records (`TestIndex`/`TestMVStore`,
`TestMultiThread`). Nothing here says `--jdk-only` made them worse.

### Real-only, and why they are not being reported as strict-mode wins

`TestCompatibility` and `TestRunscript` passed strict and failed real. Both are
long-running (79 s and 173 s in the strict arm) and the real arm ran at a load
average above 250 on 16 cores, where the strict arm ran at 60–160 and HotSpot
at ~13. **Treat every HANG in this table as provisional for the same reason**:
a 180-second per-class bound measured at load 250 is measuring the host. The
FAIL rows carry stack traces and do not have that problem.

## Reproducing one class

```sh
cd $(mktemp -d)   # H2's TestBase.BASE_TEST_DIR is "./data" and error.lock is CWD-relative
<binary> --jdk-only --java-home $JDK25 --Xmx 1g \
    -c "$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)" \
    org.h2.test.db.TestDuplicateKeyUpdate
```

## What was not run

Hibernate, Spring Boot and Tomcat. Each needs its own three-arm pass; the
runners all resolve their binary from an environment variable
(`CRATONVM_EXE` for Tomcat, `CV_BIN` for Hibernate), so
`../../../scripts/cratonvm-prefix-args.sh` drives them the same way with no runner edit.
Do it on a quiet host, or the HANG column will again be about the host.
