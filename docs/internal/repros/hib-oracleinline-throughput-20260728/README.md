# `OracleInlineMutationStrategyIdTest` throughput — measurement kit

Tracked copies of everything used to close the throughput residual of
[`../../fixed-suite-bugs/hibernate/h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`](../../fixed-suite-bugs/hibernate/h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md).
`apps/` is gitignored, so without this directory none of it survives.

| file | what it is |
|---|---|
| `H2InsertRateProbe.java` | Hibernate-free JDBC probe: replays the test's 4400 single-row `INSERT`s (three JOINED-inheritance tables, one transaction), then its bulk `UPDATE`/`DELETE`. Isolates "what H2 costs" from "what Hibernate adds". Witness for the **331x per-insert** finding. |
| `CratonRunnerTimed.java` | `CratonRunner` plus a per-test-method `@@METHOD … ms=…` line, and support for `pkg.Class#method` selectors so one method can be A/B'd in ~4 min instead of a ~19 min class. |
| `run-oracle-onemethod.ps1` | One-shot lever sweep (action queue, H2 JIT ban, SQL echo, TRACE logging, all together). |
| `run-oracle-repeat.ps1` | **Interleaved** repeat pass over the three decisive configurations. Use this, not the one-shot sweep, for any claim: the default configuration alone spreads ±8 % run-to-run, which is enough to manufacture two of the four "levers" the one-shot pass appeared to find. |
| `log4j2-quiet.properties` | Drop-in `-Dlog4j2.configurationFile=` target that silences Hibernate's shipped test TRACE loggers. |

## Run

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner        # for @common.args and @javac-cp.args
javac @javac-cp.args -d . H2InsertRateProbe.java CratonRunnerTimed.java

# insert-rate probe, both VMs
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" @common.args H2InsertRateProbe 1100
<cratonvm.exe> --java-home "<jdk25>" --Xmx 1500m @common.args H2InsertRateProbe 1100

# per-method A/B (needs oracle-inline-one.txt containing
# org.hibernate.orm.test.bulkid.OracleInlineMutationStrategyIdTest#testDeleteFromPerson)
pwsh ./run-oracle-repeat.ps1 -Rounds 2
```

Always raise `-Djunit.jupiter.execution.timeout.default` past 120 s for these runs, or
the harness truncates the method and you measure the cap instead of the method.
