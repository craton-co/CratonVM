# Fixed suite bugs (archive)

**Non-normative history.** These are the original per-suite bug/crash reports for
defects that are now **FIXED on `dev`**, moved here out of the active trackers so
that `docs/known-issues/` holds only **open** items. Kept for traceability (repro
+ root cause + fix commit); do not cite as current behaviour.

| Doc | Fix |
|---|---|
| `bug-03-priorityqueue-boxed-ordering.md` | `936b8e19` — verified vs HotSpot |
| `crash-01-arraylist-capacity-oom-abend.md` | `fix/oom-array-alloc-abend` (base `0e3f0398`) |
| `crash-02-native-capacity-ctor-abort-family.md` | `da58ff4e` — verified vs HotSpot |
| `crash-03-rsocket-payloadutils-jit-sigsegv.md` | `957270c8` — verified |
| `spring-bug-02-kotlin-reflect-builtins-null.md` | `cf58ea48` (SB-15) — symptom resolved |
| `spring-bug-03-stream-iface-no-code-attribute.md` | `4c5a4d09` (merge `d75f7716`) — verified |
| `spring-bug-04-junit-timeout-interceptor-double-proceed.md` | `fc20e970` — `compare_via_compare_to` throws CCE for non-`Comparable`; `ValueCodeGeneratorTests` 41/41 vs HotSpot. "Interceptor invoked twice" was a mask, not threading/MH. |
| `spring-bug-05-dynamic-proxy-module-system.md` | `16f832c0` (merge `d75f7716`) |
| `spring-bug-12-hashmap-view-spliterator-int16.md` | `fix/spring-bug-10-11` — validated |
| `bug-A-arraylist-sublist-copy-not-view.md` | `9a535e85` (off dev `b8908…`) |

**Not moved — partial fixes with open residuals** (left in their suite folders):
- `spring-suite/crash-reports-2026-06-16/bug-05-generics-fieldtypesignature-cce.md`
  — wildcard-bound `Type[]` fixed (`d01345d1`); real-`ParameterizedType` residual open.
- `spring-suite/bugs/spring-bug-09-collection-layout-probe-oob.md`
  — crash fixed (`5941addd`); a separate residual hang is still open.
