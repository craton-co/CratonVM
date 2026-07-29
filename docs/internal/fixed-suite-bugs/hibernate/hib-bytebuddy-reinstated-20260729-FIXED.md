# HIB-BYTEBUDDY blanket ban — the 2026-07-28 removal was premature; re-instated 2026-07-29

**Status:** FIXED — by re-instating the ban (not a new fix; a revert of an undersampled removal).

## Timeline

1. **2026-06-13** — `net/bytebuddy/` blanket-banned from JIT after `SimpleEnhancerTests` hung
   (rc=124), stack spinning in `TypeDefinition$Sort.describe`/`TypeDescription.represents`.
2. **2026-07-28** — ban removed. Verification was a **15-class A/B sample** across
   `org/hibernate/orm/test/bytecode/enhancement/**` (byte-identical results both arms) plus the
   `SimpleEnhancerTests` regression witness passing faster than interpreted. See (now superseded)
   `docs/known-issues/hibernate/hib-bytebuddy-removed-20260728.md`.
3. **2026-07-28, later** — a full 4548-class suite run (`categorize-20260728-183439`, with the
   separate `org/hibernate/` JIT ban already lifted per HIB-TEMPORAL.1's escape hatch) surfaced
   **302 CRASH-status classes**, spread across dozens of unrelated packages (`action.queue`,
   `annotations`, `batchfetch`, `bootstrap`, `collection`, `cut`, `discriminatedcollections`,
   `entitygraph`, `filter`, `hql`, ...) — only 12 of which were literally under
   `bytecode.enhancement`. That breadth is consistent with ByteBuddy's proxy/lazy-init machinery
   being invoked implicitly by ordinary entity mapping throughout the suite (Hibernate uses
   ByteBuddy-generated proxies for lazy loading by default), not just by tests that explicitly
   exercise "enhancement" — the 15-class sample undersampled the real blast radius.
4. **2026-07-29** — re-instated the ban verbatim (same `net/bytebuddy/` prefix, same
   `package_allowed` escape hatch) in `vm/src/jit/skip_list.rs`. Rebuilt (worktree
   `CratonVM-hib-local-0712-v3`, commit `77389fa06` + this one change) and reran all 302
   previously-crashing classes, 2 shards, JIT on, no `CRATONVM_JIT_ALLOW_PACKAGES` override:

```
run: run-20260728-231947-passed
classes=302  wall=53m35s
status: PASS=298  ABORTED=3  HANG=1
```

**298/302 (98.7%) now PASS, zero crashes.** The remaining 4:
- 3 `ABORTED` — all in `org.hibernate.orm.test.action.queue.*`, matching the already-documented,
  unrelated `Assumptions.abort("Skipping GRAPH test with non-GRAPH queue type")` self-abort
  behavior (see `docs/known-issues/hibernate/actionqueue-graph-default-tests-legacy-tradeoff-20260727.md`)
  — not new, not caused by ByteBuddy.
- 1 `HANG` — `org.hibernate.orm.test.hql.ASTParserLoadingTest`, the already-characterized
  "JAXB reflection storm, CPU-bound not deadlocked, known slow" class seen repeatedly across this
  investigation — not new.

## Conclusion

The 2026-07-28 removal of the ByteBuddy JIT ban was the root cause of the 302-class crash spike.
Re-instating it (reverting to the pre-2026-07-28 state) resolves it completely — no residual
ByteBuddy-attributable failures remain. **Lesson for future ban removals in this codebase:** a
15-class targeted sample is not sufficient evidence to lift a ban on a dependency (like ByteBuddy)
that's transitively exercised by a large fraction of the suite rather than only by tests that name
it explicitly — verify against a broad, blind sample (or the full suite) before declaring such a
ban safe to remove.
