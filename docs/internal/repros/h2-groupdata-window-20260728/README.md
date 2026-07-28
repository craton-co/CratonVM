# `ExpressionColumn.getValue` groupData bypass — Hibernate-free regression witnesses

Standalone JDBC probes for the defect written up in
[`../../fixed-suite-bugs/hibernate/h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`](../../fixed-suite-bugs/hibernate/h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md):
CratonVM's Rust override of `org.h2.expression.ExpressionColumn.getValue` skipped H2's
`SelectGroups` prologue, so in a grouped or windowed query every emitted row read the
**last scanned source row** instead of its own buffered value. Seven Hibernate
known-issue docs were filed against that one defect.

Neither probe needs Hibernate — only the real `h2-2.4.240.jar` that is already on the
suite classpath.

| file | what it covers |
|---|---|
| `H2OsaWindowProbe.java` | 16 shapes: ordered-set aggregates (`percentile_disc`, `listagg`) as window functions, `rank`/`dense_rank`/`row_number`/`lag`/`avg`/`count` with and without `PARTITION BY`, a `ROWS BETWEEN` frame, a filtered window aggregate, `GROUP BY` + `row_number()`, and the `bulkid` `INSERT ... SELECT ... row_number() over()` |
| `H2WindowScanProbe.java` | the original narrowing probe: which `SELECT` shapes yield wrong per-row values |
| `expected-output.txt` | `H2OsaWindowProbe` reference output from real HotSpot JDK 25 — CratonVM must match it line for line |
| `prefix-broken.txt` | the same probe on a **pre-fix control binary** (dev `d0a6c7987`, only the delegation disabled): all four symptom shapes plus the `bulkid` PK collision in one annotated run |

## Run

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner        # for @common.args (classpath + h2 jar)
javac @javac-cp.args -d . H2OsaWindowProbe.java

# HotSpot reference
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  @common.args H2OsaWindowProbe

# CratonVM — must match the above line for line
<cratonvm.exe> --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args H2OsaWindowProbe
```

Any line that differs is this defect (or a regression of it). The tell is a value
that is stuck on one row of the scan — the last row for `GROUP BY`/unpartitioned
windows, the first partition's value for `PARTITION BY`.
