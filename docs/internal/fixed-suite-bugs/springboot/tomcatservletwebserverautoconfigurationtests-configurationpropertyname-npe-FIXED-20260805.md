# `TomcatServletWebServerAutoConfigurationTests` — `ConfigurationPropertyName` NPE — RESOLVED

**Status: FIXED (2026-08-05).** Not a malformed property name from an unusual
environment variable. It is the recycled-`JitInvokeInfo` dispatch aliasing
fixed by `383e7f5cf`; see
`flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
for the mechanism and the bisection.

## Original symptom (as filed)

8 of 18 tests failed. Binding `ServerProperties` (prefix `server`) threw
`ConfigurationPropertiesBindException`, and the exception's own
message-building threw a second, masking NPE:

```
java.lang.NullPointerException: Cannot invoke "…ConfigurationPropertyName$ElementType.isIndexed()"
  because the return value of "…ConfigurationPropertyName$Elements.getType(int)" is null
  ConfigurationPropertyName.isIndexed(ConfigurationPropertyName.java:121)
  ConfigurationPropertyName.buildDefaultToString(ConfigurationPropertyName.java:580)
  …
  BindException.buildMessage(BindException.java:81)
  Binder.handleBindError(Binder.java:419)
```

## Root cause

The filed hypothesis was that the `ConfigurationPropertyName` itself was
malformed — built from an environment variable or system property whose key
CratonVM exposes in an unusual shape (extra segment, wrong separator, empty
component) — and that the fix would start with dumping that name. **That was
wrong, and the suggested diagnostic would not have found it:** the name is
well-formed and `Elements.getType(int)` is ordinary Java.

`vm/src/jit/helpers.rs` keys its per-thread dispatch memos on
`(vm_identity, JitInvokeInfo pointer)`. Those boxes are freed with their
`CompiledMethod`, so a recycled address lets a compiled call site inherit the
previous site's resolution and return that site's value. `getType(int)`
returning `null` for an in-range index is one instance of it; so is
`Class.cast` returning its receiver, and a valid method descriptor being
rejected (see the Flyway page).

The doc was right that the NPE is **secondary** and masks the real
`server`-prefix bind error. It is secondary in a stronger sense than it
guessed: with the aliasing fixed there is no bind error underneath at all —
the `BindException` that the broken `toString()` was trying to describe was
itself produced by the same defect.

## Validation

Local Windows, one process per class, runner env vars, `--Xmx 2g`:

| Binary | Result | Wall |
|---|---|---:|
| 08-05 full-suite binary (no `383e7f5cf`) | **8/18 FAIL** | 36.4s (Azure) |
| current dev `96acd76ed` (has `383e7f5cf`) | **18/18 PASS** | 112.1s |
| HotSpot 25.0.3+9 control | 18/18 PASS | 6.7s |

None of `isIndexed`, `buildDefaultToString` or
`ConfigurationPropertiesBindException` appears anywhere in the fixed run's
stdout or stderr.

Azure Linux (`/data/sbrun.sh`, `--Xmx 4g`, worktree
`/data/data/wt-flywayfix-20260805` at `origin/dev`):

```
[1/1] jit TomcatServletWebServerAutoConfigurationTests rc=0 PASS 54s tests=18 failed=0 aborted=0 containersFailed=0
```

Recorded history on Azure: `08-02 PASS 78.050s` → `08-05 FAIL 8/18` → fixed
`PASS 54s`. It regressed inside the same 08-02→08-05 window as the Flyway and
`RabbitAutoConfiguration` classes and comes back at its 08-02 speed, well
inside the base 300s budget (HotSpot's Azure baseline is 24.0s).

## Affected classes

- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.autoconfigure.servlet.TomcatServletWebServerAutoConfigurationTests`
