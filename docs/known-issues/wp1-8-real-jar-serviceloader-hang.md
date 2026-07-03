# WP1.8 ServiceLoader iterator hangs

Status: open

Date observed: 2026-07-02

## Summary

`vm/tests/wp1_8_real_jar_serviceloader.rs::driver_discovered_from_jar_on_classpath`
hangs when the fixture class and `META-INF/services/java.sql.Driver` descriptor
are loaded exclusively from a synthesized JAR classpath entry.

The directory-classpath companion
`vm/tests/wp1_8_serviceloader_e2e.rs::service_loader_iterator_discovers_driver`
also hangs in the same end-to-end `ServiceLoader.iterator()` path. Narrower
WP1.8 closure tests still run by default, but the iterator VM invocations are
ignored until the path is bounded and made deterministic.

## Repro

```powershell
cargo test -p cratonvm-vm --test wp1_8_real_jar_serviceloader -- --ignored --nocapture --test-threads=1
cargo test -p cratonvm-vm --test wp1_8_serviceloader_e2e -- --ignored --nocapture --test-threads=1
```

Observed behavior: the test process remains in
`wp1_8_real_jar_serviceloader-*.exe` or `wp1_8_serviceloader_e2e-*.exe` for
more than 10-15 minutes and blocks `cargo test --workspace`.

## Current Mitigation

The hanging acceptance tests are marked `#[ignore]` so default local CI can
complete. Remove the ignores only after the ServiceLoader iterator path either
passes or fails with a bounded timeout.
