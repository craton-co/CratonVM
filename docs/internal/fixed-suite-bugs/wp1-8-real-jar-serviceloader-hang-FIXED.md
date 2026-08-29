# WP1.8 ServiceLoader iterator hangs — FIXED

Status: fixed (root cause landed 2026-07-06 in commit `8a61e6f8`, merged via
`1b131502`; tests un-ignored + causally verified 2026-07-07)

Date observed: 2026-07-02
Original location: `docs/known-issues/wp1-8-real-jar-serviceloader-hang.md`

## Original report

`vm/tests/wp1_8_real_jar_serviceloader.rs::driver_discovered_from_jar_on_classpath`
hung when the fixture class and `../../../apps/META-INF/services/java.sql.Driver` descriptor
were loaded exclusively from a synthesized JAR classpath entry. The
directory-classpath companion
`vm/tests/wp1_8_serviceloader_e2e.rs::service_loader_iterator_discovers_driver`
hung in the same end-to-end `ServiceLoader.iterator()` path. The test process
stayed at 100% CPU for 10-15+ minutes and blocked `cargo test --workspace`;
both tests were marked `#[ignore]` as mitigation.

## Root cause

**`ArrayList$Itr` lastRet/cursor fallback field-slot collision** in
`../../../native-collections/src/lib.rs`. `al_itr_last_ret_slot`'s fallback (used
whenever the real `java/util/ArrayList$Itr` layout is not resolvable — the
common case for collections assembled by native code, exactly what
`native_sl_iterator` returns for `ServiceLoader.load(..).iterator()`)
defaulted to slot 1, colliding with `AL_ITR_FIELD_CURSOR` (also slot 1).
`native_al_itr_next`'s last two writes were `cursor = cursor + 1` then
`lastRet = <pre-increment cursor>` into the SAME slot, so `cursor` never
advanced past 0. `hasNext()` (`cursor < size`) stayed true forever and
`next()` returned the first element on every call. The WP1.8 fixture
(`Wp18ServiceLoaderE2E.serviceLoaderIteratorCount()`) drains the iterator
with a plain `while (it.hasNext()) { it.next(); }` loop → unbounded spin.

Fixed by `8a61e6f8` ("Fix ArrayList$Itr lastRet/cursor field-slot collision
causing infinite iteration loops"), which was root-caused independently from
the WildFly `org.wildfly.extension.core-management` clinit infinite loop —
same defect, different manifestation. It added a dedicated
`AL_ITR_FIELD_LAST_RET = 2` fallback slot (`AL_ITR_NUM_FIELDS` 2 → 3).
Regression-covered by `../../../vm/tests/al_itr_lastret_slot_regression.rs`.

## Causal verification (2026-07-07, Linux + Windows)

- Doc-era tree (`d9cb7be8`, the commit that added the `#[ignore]`s, plus
  two unrelated Linux-build-only overlays): `driver_discovered_from_jar_on_classpath`
  reproducibly hangs at 99.9% CPU (killed at 60-90s). gdb sampling shows the
  interpreter re-attempting class resolution
  (`resolve_class_loader_aware` → `load_class_concurrent`) on every spin
  iteration — that is the per-iteration hot spot in the samples, not the root
  cause; the loop itself never terminates because of the iterator slot collision.
- Same doc-era tree + ONLY the `8a61e6f8` `../../../native-collections/src/lib.rs`
  patch applied: passes in 0.01s. This isolates the fix to that single commit.
- Current dev (`745c75c0`): real-JAR test passes 5/5 consecutive runs on
  Linux (real-JDK jdk25 and synthetic fallback) and 5/5 on Windows
  (jdk-25 — the platform of the original report).
  `wp1_8_serviceloader_e2e` (`--features synthetic-jdk`) passes all 5 tests
  including the previously-ignored `service_loader_iterator_discovers_driver`.

## Resolution

Both `#[ignore]` markers removed
(`../../../vm/tests/wp1_8_real_jar_serviceloader.rs`,
`../../../vm/tests/wp1_8_serviceloader_e2e.rs`); the WP1.8 acceptance bar
("`ServiceLoader.load(java.sql.Driver.class)` finds a driver JAR via
`../../../apps/META-INF/services` on classpath") is enforced by default again.
