# Hibernate / logging — format placeholders (`%s`) not expanded

## Status
**OPEN** on `target-bench` (2026-06-05).

## Severity
**MEDIUM** — logging incorrect; may indicate broader `String.format` / formatter bugs.

## App / suite
- **Suites:** `HibernateSmoke`, `HibernateProbe` (CratonVM stderr)
- **Logs:** `test-infra/suite-results/apps-three-20260605-141545/`

## Symptom

Hibernate and JBoss Logging output shows **literal format tokens** instead of values:

```
INFO [org.hibernate.Version] HHH000412: Hibernate ORM core version %s
INFO [org.hibernate.jpa.internal.util.LogHelper] HHH000204: Processing PersistenceUnitInfo [name: %s]
WARN [org.hibernate.jpa.boot.internal.PersistenceXmlParser] HHH015018: Encountered multiple persistence-unit stanzas defining same name [%s]; …
```

HotSpot prints actual version string and PU name.

## HotSpot behavior

SLF4J → JUL or Log4j backend expands `{}` / `%s`-style messages using `String.format` or `MessageFormat` internally. User-visible logs contain real values.

## CratonVM behavior

Format string passed through **unchanged** or only partially formatted. Observed on both CratonVM-native log lines and Hibernate’s JBoss Logging backend.

## Root cause (suspected)

Same class of defect as WildFly Bug WF-2 ([bug-wildfly-string-format-positional-index.md](bug-wildfly-string-format-positional-index.md)):

- `String.format` / `Formatter` mishandles `%s`, `%d`, or positional `%N$` specifiers
- JBoss Logging `Logger.logf` / `printf`-style path broken

**Suspect file:** `native-builtins/src/lang_string.rs` (`native_string_format`).

Note: Positional `%2$d` fix on branch `fix/wildfly-enum-constants` may **not** be merged into `target-bench` — ordinary `%s` may still be wrong on main build.

## Impact

- Harder to diagnose other Hibernate bugs (logs misleading)
- Any app using `String.format` / SLF4J parameterized logging may log wrong text
- `HibernateProbe` still **PASS** (does not assert log content)

## Reproduce

```bash
# CratonVM
target-bench/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  -cp "<hibernate-probe-cp>" HibernateProbe 2>&1 | grep '%s'
```

Or run three-apps suite and inspect hibernate stderr.

Minimal Java:

```java
System.out.println(String.format("version %s", "6.5.2"));
```

## What to fix

1. Audit `native_string_format` for all common specifiers (`%s`, `%d`, `%n`, positional, flags).
2. Merge WildFly positional-index fix if not already on main.
3. Add regression tests mirroring WildFly’s 11-case format suite + simple `%s` cases.
4. Re-run `HibernateSmoke` and confirm log lines show real version/PU name.

## Related

- [bug-wildfly-string-format-positional-index.md](bug-wildfly-string-format-positional-index.md)
- [bug-hibernate-jpa-persistence-xml-properties.md](bug-hibernate-jpa-persistence-xml-properties.md)
