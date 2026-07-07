# Hibernate `type.temporal.*` — JDBC parameter placeholders duplicate in generated SQL (`values (??,???)`)

**Status:** 🔴 **OPEN** — newly surfaced 2026-07-07, untriaged.
**Severity:** Medium (mass test failures in the affected classes; no crash/abort).
**Mode:** observed in default (JIT) real-JDK mode on the Linux probe host; not yet tried `--nojit`/HotSpot-diff.

## Context

Found while validating the (now archived) GC stale-local crash doc
[`../internal/hib-temporal-gc-lambda-native-stale-local.md`](../internal/hib-temporal-gc-lambda-native-stale-local.md):
after the 2026-07-06 GC fixes (`3240cb75`), the 5 `org.hibernate.orm.test.type.temporal.*`
classes no longer crash (0 stale-pointer / SIGSEGV / `Object.<sam>` markers) — they run to
completion but fail en masse on a **different, functional** bug.

## Symptom

`LocalDateTimeTest`: `found=162 started=162 ok=18 failed=72 aborted=72` — the failures are all

```
org.hibernate.exception.SQLGrammarException: Could not prepare statement
[Syntax error in SQL statement "insert into entity_tbl (value_col,id_col) values (?[*]?,??)" ...]
```

(`[*]` is H2's error-position marker.) The INSERT's JDBC `?` placeholders are **duplicated**, and
the duplication **grows across successive statements in the same run**: `values (??,??)` →
`values (??,???)` → `values (???,???)`. That growth pattern suggests a string/char-array builder
whose buffer or length is not reset between renders (Hibernate renders each placeholder via its
`ParameterMarkerStrategy` / `StringBuilder` appends), i.e. a CratonVM `StringBuilder`/`AbstractStringBuilder`
or `char[]`-copy defect on that path — NOT a GC issue (zero corruption markers in the same runs).

## Repro (Linux probe host)

```bash
cd <hibernate-orm-harness>/hib-suite-runner
# args copy with host-correct classpath roots; see /tmp/hib-args-fixed.args pattern
echo org.hibernate.orm.test.type.temporal.LocalDateTimeTest > /tmp/one.txt
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm> --java-home <jdk25> \
  "@args" CratonRunner /tmp/one.txt 0
# grep the log for: values (?? — any doubled placeholder is the bug
```

## Next steps

1. Reduce: capture the exact SQL Hibernate hands to `PreparedStatement` vs what H2 receives;
   binary-search whether the duplication happens in Hibernate's SqlAstTranslator append loop
   (CratonVM `StringBuilder.append` defect) or later.
2. Diff `--nojit` and HotSpot to confirm CratonVM-only and JIT-independence.
3. Check the obvious suspects: `StringBuilder.append(char)`/`insert`/`setLength` natives and any
   `AbstractStringBuilder` shadowing on the real-JDK path.
