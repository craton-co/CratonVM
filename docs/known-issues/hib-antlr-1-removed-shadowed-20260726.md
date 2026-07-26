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
