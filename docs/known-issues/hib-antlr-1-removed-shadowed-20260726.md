# HIB-ANTLR.1 — removed (redundant/shadowed by HIB-LONGTAIL.1), own claim does not reproduce

**Status: HIB-ANTLR.1's own check removed from `skip_list.rs`. Default
(Conservative) behavior for `org/antlr/v4/runtime/` classes is UNCHANGED —
they stay interpreted via the separate, already-confirmed-needed
HIB-LONGTAIL.1 ban, which covers the identical prefix.**

## What was tested

Real Hibernate ORM 8.0 test harness (see
`docs/known-issues/hib-temporal-1-still-needed-20260726.md` for the
fixture description). Ran with `org/antlr/v4/runtime/` JIT-allowed while
`org/hibernate/` stayed banned, isolating this one package:

| Test class | Methods | Result |
|---|---:|---|
| `org.hibernate.orm.test.hql.ASTParserLoadingTest` | 106 | 106/106 ok |
| `org.hibernate.orm.test.hql.HQLInsertAndUpdateTest` | 5 | 5/5 ok |
| `org.hibernate.orm.test.type.temporal.InstantTests` | 204 | 112 ok / 0 failed / 92 aborted (matches baseline exactly) |

0 failures anywhere — HIB-ANTLR.1's own specific claim (a full HQL parse
under JIT could leave `ATNState.transitions` null, corrupting the *next*
parse in the same process) does not reproduce across 315 real HQL-parsing
test methods.

## Why this is a shadowed removal, not an independent unban

`org/antlr/v4/runtime/` is **also**, separately, covered by
HIB-LONGTAIL.1 (`vm/src/jit/skip_list.rs`, same prefix,
`if (class_name.starts_with("org/h2/") ...) || (class_name.starts_with("org/antlr/v4/runtime/") ...)`),
which is independently confirmed still-needed via a real 218-class H2
suite run that found a `Schema not found` DB-reconnect corruption (see
`docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md`). That
bug is triggered by a different, more specific scenario (closing and
reopening a database connection, replaying metadata) that this HQL-parsing
test batch does not exercise.

**Net effect:** classes under `org/antlr/v4/runtime/` stay interpreted by
default after this removal, exactly as before — HIB-LONGTAIL.1 alone is
sufficient. This removal only deletes a redundant, now-unnecessary second
check; it does not change any observable JIT-eligibility outcome. Matches
the same pattern found earlier this session for SPRINGBOOT-WITHOUT-JACKSON.2
(see `docs/internal/jit-ban-remaining-sweep-20260726.md`).

## Related

- `docs/known-issues/hib-temporal-1-still-needed-20260726.md` — the
  HIB-TEMPORAL.1 (`org/hibernate/`) finding from the same investigation
  session, which IS a confirmed-still-live, non-shadowed bug.
- `docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md` —
  HIB-LONGTAIL.1's own confirming evidence (a prior session).

## Cross-session update (2026-07-26, same day)

A concurrent session rewrote HIB-LONGTAIL.1's own doc
(`docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md`,
commit `670983c71`) with fresh 218-class H2 suite evidence: the ORIGINAL
`Schema  not found` corruption this ban was written for is now fixed and
extinct (0/218 classes), but the ban stays because lifting it causes 9
OTHER, unrelated regressions in the H2 suite (`TestObjectDataType`,
`TestUpgrade`, etc.) -- a real, current reason to keep `org/h2/` banned.
That session's own doc explicitly says: **The `org/antlr/v4/runtime/`

## Cross-session update (2026-07-26, same day)

A concurrent session rewrote HIB-LONGTAIL.1's own doc
(`docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md`,
commit `670983c71`) with fresh 218-class H2 suite evidence: the ORIGINAL
`Schema  not found` corruption this ban was written for is now fixed and
extinct (0/218 classes), but the ban stays because lifting it causes 9
OTHER, unrelated regressions in the H2 suite (`TestObjectDataType`,
`TestUpgrade`, etc.) -- a real, current reason to keep `org/h2/` banned.
That session's own doc explicitly says: **"The `org/antlr/v4/runtime/`
half remains untested in isolation — the H2 suite never exercises it."**

This session's finding (above) is exactly that missing isolation test,
from a different real fixture (Hibernate ORM's own HQL suite, which does
exercise `org/antlr/v4/runtime/` directly and heavily): clean, 0
failures. Combined, there is now real evidence that HIB-LONGTAIL.1 could
be *narrowed* to `org/h2/` only, dropping its `org/antlr/v4/runtime/`
half as a second, independently-safe-to-remove component -- but that ban
entry is a different one (HIB-LONGTAIL.1, not HIB-ANTLR.1) and was being
actively edited by that other session at the moment this doc was written.
**Deliberately not touched here to avoid a same-file collision** -- left
as a concrete, well-evidenced recommendation for whoever next has
HIB-LONGTAIL.1 checked out.
