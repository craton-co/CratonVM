# HIB-CV-26 — `DriverManager` registration via `Class.forName` fails (HHH-7272)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** Medium — real correctness bug, **deterministic, reproduces under `--nojit`**, HotSpot PASS
**Status:** Confirmed

---

## Symptom

`org.hibernate.orm.test.connection.DriverManagerRegistrationTest`

```
testDriverRegistrationUsingClassForNameSucceeds:
  org.opentest4j.AssertionFailedError: Unanticipated failure according to HHH-7272
```

The test loads a JDBC `Driver` via `Class.forName(driverClass)` (whose static
initializer calls `DriverManager.registerDriver`), then asserts
`DriverManager.getDriver(url)` returns it. On CratonVM the `getDriver` lookup
throws (caught → `fail("Unanticipated failure …")`). HotSpot: PASS.

## Why it's a real CratonVM bug

- Deterministic, reproduces standalone `--nojit` (not the JIT family).
- HotSpot PASS.

## Root area

`Class.forName(name)` (the 1-arg form) must initialize the class, running its
static initializer that registers the driver with `DriverManager`. The failure
implies either:
- CratonVM's 1-arg `Class.forName` does not initialize the class (so the static
  driver registration never runs), or
- `DriverManager.getDriver` / the registered-driver list is not consistent with
  what was registered (e.g. caller-classloader visibility filtering in
  `DriverManager`).

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-DriverManagerRegistrationTest> 0
# testDriverRegistrationUsingClassForNameSucceeds -> AssertionFailedError (HHH-7272)
```

## Triage

Real, deterministic, independent of the JIT. Likely a `Class.forName`
initialization or `DriverManager` caller-visibility issue — tractable; hand off
or fix after the JIT.
