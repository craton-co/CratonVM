# H2 — MVStore `SYS` table lock timeouts

## Status
**FIXED** (dev `ec4f328`, 2026-06-05) — root cause was synthetic `ArrayDeque.remove(Object)` / `removeFirstOccurrence` / `removeLastOccurrence` being **unregistered**, so they fell through to the real-JDK `delete()` bytecode (updates head/tail, oblivious to our phantom `size` slot) → permanent desync → corrupted `MVTable.waitingSessions` so `doLock1`'s `waitingSessions.getFirst() == session` guard never held. Fixed by registering the 3 object-removal natives operating on the synthetic circular-buffer (`native-collections/src/lib.rs`, `ad_remove_at_logical`). Verified: **0** `Timeout trying to lock` in `TestScript --nojit` (was 141+); also fixes the column-count and table-not-found symptoms (same root cause).

## Severity
**HIGH** — correctness and/or performance; DDL-heavy tests fail or stall.

## App / suite
- **App:** H2 Database (`apps/h2database`)
- **Suite:** `org.h2.test.TestAll` → `org.h2.test.scripts.TestScript` (DDL: `DropTable`, `CreateTable`, `CreateFunctionAlias`, …)
- **Logs:** `test-infra/suite-results/apps-four-20260604-233229/h2-testall.log`

## Symptom

```
org.h2.jdbc.JdbcSQLTimeoutException: Timeout trying to lock table "SYS"
```

During meta-catalog DDL while running script tests.

## HotSpot behavior

Not reached in the same bounded HotSpot run. On HotSpot, MVStore catalog locks are acquired and released within the timeout window.

## CratonVM behavior

Repeated lock timeouts on table **`SYS`** (H2’s internal catalog). May be:

1. **Correctness:** lock never released (`Object.wait` / monitor bug)
2. **Performance:** interpreter so slow that 2 s (or configured) timeout fires before lock acquisition
3. **Interaction** with column-count bugs leaving locks held

## Root cause (suspected)

**Suspect stack:** `org.h2.mvstore.db.MVTable.doLock1` → monitor wait/notify.

- Broken `Object.wait`/`notify`/`notifyAll` timing
- Thread identity / lock owner tracking wrong under CratonVM
- Reentrant lock count incorrect on `MVTable`

## Impact

- DDL script tests fail or flake.
- Contributes to corrupted test DB state when combined with Bug H2-2.

## Reproduce

Run `TestAll` and grep:

```bash
grep -i 'lock table "SYS"' h2-testall.log | wc -l
```

After SHA1PRNG fix, bisect to first `TestScript` DDL that times out.

## What to fix

1. Reproduce with a minimal two-thread MVStore lock probe if possible.
2. Compare lock hold time CratonVM vs HotSpot on identical DDL sequence.
3. Fix monitor/lock implementation or MVTable lock path.
4. Confirm timeout count drops to zero on `TestScript`.

## Related

- [bug-h2-prepared-statement-column-count.md](bug-h2-prepared-statement-column-count.md)
- `continue_prompt_h2_testall.md` (historical notes)
