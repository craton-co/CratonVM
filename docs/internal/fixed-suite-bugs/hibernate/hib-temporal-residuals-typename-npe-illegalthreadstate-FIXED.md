# Hibernate `type.temporal.*` residuals — `DdlTypeImpl.getRawTypeName` NPE cluster + empty `IllegalThreadStateException` pair

**Status:** FIXED on 2026-07-08 by the conservative Hibernate JIT guard in `../../../../vm/src/jit/skip_list.rs` plus the final `cratonvm-jit::try_compile` gate. The guard keeps `org/hibernate/` bytecode interpreted under the default conservative policy, with `CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/` (or `org.hibernate.` for dotted names) available for bisection.

**Original severity:** Medium (37 + 2 tests in `InstantTests`; 2 in `LocalDateTimeTest`; other temporal classes unmeasured for these signatures).

**Fix summary:** cold `H2Dialect.columnType(3003)` matched HotSpot, while `CRATONVM_TIER_ENABLED=0` removed the `typeNamePattern` signature, proving this was JIT-induced. Narrow package bisection removed the DDL NPE but exposed intermittent adjacent temporal failures; the stable control was `CRATONVM_JIT_DENY=org/hibernate/`, so the production fix makes that fail-closed guard the default conservative behavior.

## 1. `NullPointerException: Cannot invoke "String.indexOf(int)" because "typeNamePattern" is null` (37× in `InstantTests`)

Thrown from Hibernate's `DdlTypeImpl.getRawTypeName(String)`
(`hibernate-core .../type/descriptor/sql/internal/DdlTypeImpl.java:275`) — some `DdlType` was
registered/queried with a **null type-name pattern**. Only `InstantTests` shows it (its
`Environment`s remap `TIMESTAMP`/`TIMESTAMP_UTC` sql-type codes via `AbstractRemappingH2Dialect`,
a path the other temporal classes don't exercise the same way).

**Pre-existing, NOT from the placeholder/deopt fix:** on the UNMODIFIED pre-fix baseline binary
(dev `86f37f84`) `InstantTests` shows the IDENTICAL 37 occurrences
(`found=204 ok=54 failed=75` = 36 placeholder + 37 this + 2 ITSE); on the fixed binary
`ok=90 failed=39` = 37 this + 2 ITSE. HotSpot passes these tests, so a CratonVM defect sits
somewhere under the dialect/DDL-type registration for the remapped codes (suspects: whatever
native/registry path feeds `DdlTypeRegistry` for the remapped `TIMESTAMP_UTC` descriptor —
untraced).

Repro: `../../../../apps/hib-suite-runner`, `CratonRunner` with a list file containing
`org.hibernate.orm.test.type.temporal.InstantTests`; grep the log for `typeNamePattern`.

## 2. Empty-message `java.lang.IllegalThreadStateException` (2× per class run; seen in both `LocalDateTimeTest` and `InstantTests`)

`@@FAIL ... :: java.lang.IllegalThreadStateException:` with an EMPTY message — the JDK's
`Thread.start()` on an already-started thread throws exactly this shape. The temporal tests'
`Timezones.withDefaultTimeZone` calls
`SharedDriverManagerConnectionProvider.getInstance().onDefaultTimeZoneChange()` around every
test, which resets shared connection state — a thread restart in that machinery is the likely
site, but the failing frame was not captured (the harness only records the summary line).

Also pre-existing (present in the baseline runs' failure count). NOT the already-fixed
`Thread.getState()`-always-NEW bug (`16d23e7b`) — that one is confirmed fixed; this is a
different, unidentified thread-lifecycle interaction. Next step for whoever picks this up:
rerun one class with a patched `CratonRunner` that prints the full stack trace of failures
(the JUnit listener currently truncates to the message line), then chase the `Thread.start`
call site.


## 2026-07-08 fix verification

Built `/data/data/bin/cratonvm-hib-temporal-residuals-20260708-170734-fixed` from branch `codex/hib-temporal-residuals-20260708-170734`.

- `cargo test -p cratonvm-jit --lib hibernate_temporal_jit -- --nocapture`: 2/2 passed.
- `cargo test -p cratonvm-vm --lib hibernate_temporal_residual -- --nocapture`: 3/3 passed.
- `rustfmt --edition 2021 --check vm/src/jit/skip_list.rs`: passed.
- `InstantTests` final run 0: `found=204 started=204 ok=112 failed=0 aborted=92`, with 0 `typeNamePattern`, 0 `IllegalThreadStateException`, 0 H2 `<local4>`, 0 `NoSuchMethodError`.
- `InstantTests` final run 1: `found=204 started=204 ok=112 failed=0 aborted=92`, with the same zero-signature counts.
- `LocalDateTimeTest`: 0 `IllegalThreadStateException`; the separate DST/H2 residuals are now fixed and retired to `hib-temporal-localdatetime-dst-h2-local4-residuals-FIXED.md`.
