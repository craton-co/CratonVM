# BUG-M — `StringBuilder.lastIndexOf(String)` always returned -1

**Test:** `jakarta.el.TestImportHandler` (`testResolveInnerClass`). HotSpot: PASS.
**Status: FIXED.**

## Symptom

`new StringBuilder("a.b.c").lastIndexOf(".")` returned `-1` (HotSpot: `3`).
This broke `jakarta.el.ImportHandler.resolveClass` for a nested-class import
(`org.apache.catalina.authenticator.DigestAuthenticator.AuthDigest`): its
`.`→`$` retry loop is driven by `StringBuilder.lastIndexOf(".")`, so with a -1
result the loop never ran and `resolveClass("AuthDigest")` returned null.

## Root cause

`StringBuilder.lastIndexOf` had no native, so it ran the real
`AbstractStringBuilder.lastIndexOf` bytecode — which reads the JDK
`value`/`count` fields. CratonVM stores StringBuilder content in a *synthetic*
representation those fields don't reflect, so the search saw an empty buffer and
returned -1 (the same synthetic-vs-real-layout mismatch behind the registered
`indexOf` natives).

## Fix

Add `lastIndexOf(String)` / `lastIndexOf(String,int)` natives
(`lang_string.rs`) mirroring the existing `indexOf` natives — read the synthetic
char buffer via `sb_read_chars` and search backward in UTF-16 space
(`u16_last_index_of`, including the empty-needle and fromIndex semantics).
Verified: `TestImportHandler` 17/17 (with [BUG-G](BUG-G-classforname-never-throws-cnfe.md)).

## Related (still open)
- `Class.getPackage()` returns a bare `Object` (not a `Package`) — surfaced in
  the same probe (`Integer.class.getPackage()` → `Object@…` vs `package
  java.lang`). Did not block this test (ImportHandler's `isExported` still
  worked), but worth a separate fix.
