# `NullPointerException: Cannot read field "coder" because "this" is null` in `String.equals` from `HashMap.putVal` (2026-11-19)

Observed during the Quarkus 3.20.0 integration-test suite run (`826/970` classes, Linux host `64.177.43.150`): `3` occurrences in `--real-jdk` (default) and `12` occurrences in `--fast-boot` (`--compatible`).

## Symptom

Every observed failure has the identical `String.equals` -> `HashMap.putVal` tail:

```text
Caused by: java.lang.NullPointerException: Cannot read field "coder" because "this" is null
	at java.base/java.lang.String.equals(String.java:1924)
	at java.base/java.util.HashMap.putVal(HashMap.java:655)
	at java.base/java.util.HashMap.put(HashMap.java:619)
	at java.base/java.time.format.DateTimeTextProvider$LocaleStore.<init>(DateTimeTextProvider.java:560)
	at java.base/java.time.format.DateTimeFormatterBuilder.appendText(DateTimeFormatterBuilder.java:849)
	at java.base/java.time.format.DateTimeFormatter.<clinit>(DateTimeFormatter.java:1350)
```

Seen e.g. in `io.quarkus.it.main.GetExceptionsInTestResourceTestCase`, `io.quarkus.it.vertx.CertificateRoleOuMappingTest`, and `io.quarkus.it.vertx.CertificateRoleSanRfc822MappingTest`.

## Notes

- In JDK 25 `HashMap.putVal` (`HashMap.java:655`), `bci 84-91` is `aload_2; ifnull 104; aload_2; aload 11; invokevirtual java/lang/Object.equals:(Ljava/lang/Object;)Z`. `key` (`aload_2`) is explicitly null-checked before `invokevirtual`, and dispatch entered `java.lang.String.equals`, yet `this` (`local 0`) is `null` by `String.java:1924` (`this.coder == aString.coder` at `bci 26`).
- Standalone `ReproHashMapNpe` and polymorphic `ReproHashMapPoly` probes passed on both `--real-jdk` and `--fast-boot`.
- Next: reproduce under `GetExceptionsInTestResourceTestCase` (`--fast-boot`) and inspect JIT compilation/inlining/deopt of `HashMap.putVal` and `Object.equals` / `String.equals` (`JitIntrinsic::StringEquals`, `jit/src/ir_lower/character_to_chars_prefix.rs`, `jit/src/x64/op_invoke/walk_invoke_instance.rs`).
### Tomcat observation and JIT bisection (2026-11-19)

Also observed in Tomcat `--fast-boot` (`--compatible`):
- `org.apache.catalina.servlets.TestDefaultServletIfMatchRequests`: fails `testIfMatch[0 resource-strong [false], matchHeader [null]]` and `testIfNoneMatch[0 ...]` with `java.lang.NullPointerException: Cannot read field "value" because "this" is null` at `String.equals(String.java:1924)` (`value` field load at `bci 37`) from `HashMap.putVal(HashMap.java:655)` (`TomcatBaseTest.methodUrl:713`).
- `org.apache.catalina.servlets.TestDefaultServletRfc9110Section13`: `FAIL(ok=0,f=1210)` in `--fast-boot` rerun.
- Bisection on `TestDefaultServletIfMatchRequests` (`--fast-boot`, 248 tests):
  - `CRATONVM_JIT=-tiered`: `OK (248 tests)`
  - `CRATONVM_JIT_DENY=HashMap.putVal`: `OK (248 tests)`
  - `CRATONVM_JIT_DENY=String`: `OK (248 tests)`
  - `CRATONVM_JIT=-ir-elide-unnamed-homes`: `OK (248 tests)`
  - `CRATONVM_JIT=-ir-elide-homes-spill-at-inspection`: `OK (248 tests)`
  - `CRATONVM_JIT=-ir-site-named-loop-values`: `FAIL (2/248)` (still fails)
