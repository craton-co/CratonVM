# `JdbcSessionAutoConfigurationTests` hangs: HSQLDB `RangeGroup$RangeGroupEmpty` out-of-bounds `get_field` loop

**Status: OPEN. NOT related to the `OnClassCondition`/annotation-array work** — found
incidentally while re-verifying [`onclasscondition-npe-cast-string-array-cluster-FIXED.md`](../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md).
Confirmed pre-existing on unmodified `dev` (reproduces identically on an
unrelated dev-tip build, commit `d94712f2a`), not a regression from any
recent work.

## Symptom

`module/spring-boot-session-jdbc`'s `JdbcSessionAutoConfigurationTests`
hangs (300s timeout, `-Parallel 1`, both with and without the
onclasscondition fix, JIT on) after CGLIB enhances
`AbstractSessionAutoConfigurationTests$SessionRepositoryConfiguration`.
The process spins, logging the same guard warning repeatedly (once every
few hundred ms, for minutes, never terminating on its own):

```
gen_heap::get_field: out-of-bounds field read dropped (caller used slot
  index past receiver's layout — class layout is correct; the bug is in
  the caller's slot computation, typically a speculative collection-layout
  probe dispatched on a non-matching receiver type)
  obj=0x1eca70d0 index=0 num_slots=0 class_id=ClassId(2859)
  class_name=org/hsqldb/RangeGroup$RangeGroupEmpty real_field_count=Some(0)
```

Note the guard's own diagnosis: `real_field_count=Some(0)` and
`num_slots=0` — the object's layout is genuinely correct (`RangeGroupEmpty`
declares zero fields, a singleton empty range group). The bug is a
**caller** repeatedly probing field index 0 on a receiver that legitimately
has no fields — most likely a speculative/duck-typed collection-layout
probe (e.g. something guessing whether this object is List/Map-shaped)
dispatched against the wrong receiver type, then retrying in a loop instead
of failing once. Same `obj`/address every time, suggesting a single stuck
call site retrying against a cached receiver rather than fresh dispatch per
iteration.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row: module/spring-boot-session-jdbc	org.springframework.boot.session.jdbc.autoconfigure.JdbcSessionAutoConfigurationTests> `
  -Start 1 -Count 1 -Parallel 1 -Exe <any current dev-tip cratonvm exe>
```

Confirmed identical on two independent dev-tip builds (this doc's
investigation build, and an unrelated `fix/string-dispatch-*` merge build,
commit `d94712f2a`) — same object address, same class, same guard message,
both hang at the 300s cutoff. Not yet bisected further; no `--nojit`
comparison done yet for this specific hang.

## Related

- Not the same corruption signature as
  [`onclasscondition-npe-cast-string-array-cluster-FIXED.md`](../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md)'s
  `java/lang/String`/`ClassId(6)` corruption (that one is fixed, `e7e3bb91f`).
  This is `org/hsqldb/RangeGroup$RangeGroupEmpty`/`ClassId(2859)`, a
  different class entirely, and the guard's own message explicitly says the
  layout is correct (caller bug, not allocation-size corruption).
- HSQLDB already has known JIT-related trouble in this codebase: `e7e3bb91f`
  added an `org/hsqldb/` JIT-deny-list entry for an unrelated Flyway/CGLIB
  SIGSEGV (`SPB-FLYWAY-HSQLDB.1`). Worth checking whether this hang is a
  sibling HSQLDB defect in the same dense add/update code path, or a
  genuinely different mechanism (the guard message's "speculative
  collection-layout probe" phrasing doesn't obviously match the flyway
  fix's SIGSEGV shape).
