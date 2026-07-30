# HIB-BYTEBUDDY (`net/bytebuddy/`) — 2026-07-28 removal rationale (HISTORY ONLY)

**SUPERSEDED — do not act on this document.** The topic is closed; the
authoritative write-up is [`hib-bytebuddy-20260730-FIXED.md`](hib-bytebuddy-20260730-FIXED.md),
which carries the real root cause (a JIT code-lifetime defect on an older
runtime, not a Byte Buddy miscompile), the corrected account of the `--nojit`
HQL residual, and the final validation.

This file is retained only for what it documents about the *original* ban: which
call chain it covered, the 2026-06-13 symptom, and the (too narrow) 15-class
sample that motivated the first removal.

Everything below this line is the original 2026-07-28 text.

**Status:** removed (deleted, not commented out), re-verified with a real Hibernate ORM 8.0
fixture. `net/bytebuddy/` in `vm/src/jit/skip_list.rs` is JIT-eligible.

## What it banned

`net/bytebuddy/` (blanket package prefix) — ByteBuddy's runtime
class-build chain. This is distinct from the narrower `HIB-PROXY` ban on
`ByteBuddyState.make`, which only covers Hibernate's lazy-proxy path;
this ban covered the bytecode-*enhancement* path instead
(`EnhancerImpl.enhance` -> `ByteBuddyState.rewrite` -> `DynamicType...
make` -> `MethodRegistry.prepare` -> deep
`net/bytebuddy/description/type/TypeDescription*` resolution).

## Original symptom (2026-06-13)

`SimpleEnhancerTests` (`org.hibernate.bytecode.internal.bytebuddy.
SimpleEnhancerTests`) hung indefinitely (rc=124) once ByteBuddy's
type-description methods were JIT-compiled, with the stack spinning in
`TypeDefinition$Sort.describe`/`TypeDescription.represents`.
`CRATONVM_DISABLE_JIT=1` made the whole enhancer pass (ok=1). Believed
at the time to be the same "JIT'd build-chain receiver corruption / loop
never returns" miscompile family as `HIB-PROXY`.

## Why it was re-tested now

This ban was never re-verified after 2026-06-13, well before this week's
general JIT correctness fixes (loader_id encode/decode asymmetry fix —
see `docs/internal/configproxy-cglib-loaderid-fixed-20260727.md`
— and the atomic-array RMW fix in `ffb8dfa22`, among others). Age alone
made it worth re-checking; ByteBuddy is core to Hibernate's
bytecode-enhancement feature, one of the 5 apps this host currently
prioritizes (tomcat/hibernate/spring/spring-boot/h2).

## Re-test setup

Binary: fresh release build off `origin/dev`, `/data/tmp/cratonvm-
bytebuddy-reverify-20260728`. Fixture: the real Hibernate ORM 8.0
harness at `/data/data/apps/hibernate-orm-harness` (built 2026-07-17,
commit `171b6cb0d`), driven via its `CratonRunner` JUnit5-launcher
driver and `common.args` (which sets
`-Dhibernate.testing.bytecode.enhancement.extension.engine.enabled=true`
— enhancement is on for every entity test in this harness, not just the
ByteBuddy-specific ones).

### The named regression witness, both ways

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
echo 'org.hibernate.bytecode.internal.bytebuddy.SimpleEnhancerTests' > classlist.txt

# ban active (control)
TMPDIR=/data/tmp <binary> --java-home /home/victor/jdk25 --Xmx 1g \
  @common.args CratonRunner classlist.txt 0
# -> ok=1 failed=0 ms=2511

# ban lifted
TMPDIR=/data/tmp CRATONVM_JIT_ALLOW_PACKAGES=net/bytebuddy/ <binary> \
  --java-home /home/victor/jdk25 --Xmx 1g @common.args CratonRunner classlist.txt 0
# -> ok=1 failed=0 ms=1762 -- PASSES, and faster than interpreted. No hang.
```

### Broader 15-class A/B

Sampled 15 classes from `org/hibernate/orm/test/bytecode/enhancement/**`
(lazy loading, lazy proxies, merge, batching — a cross-section of
ByteBuddy-enhanced-entity code paths, not just the one probe class), ran
each individually, ban active vs. lifted:

| # | Class | Control (active) | Lifted |
|---|---|---|---|
| 0 | MergeDetachedToProxyTest | ok=2 failed=0 | ok=2 failed=0 |
| 1 | SetIdentifierOnAEnhancedProxyTest | ok=4 failed=0 | ok=4 failed=0 |
| 2 | MergeTest | ok=8 failed=0 | ok=8 failed=0 |
| 3 | BatchingTest | found=0 (harness discovery quirk, both arms identical) | found=0 |
| 4 | EntitySharedInCollectionAndToOneTest | ok=1 failed=0 | ok=1 failed=0 |
| 5 | LazyCollectionDetachWithCollectionInDefaultFetchGroupFalseTest | ok=3 failed=0 | ok=3 failed=0 |
| 6 | LazyToOnesProxyWithSubclassesTest | ok=5 failed=0 | ok=5 failed=0 |
| 7 | LoadANonExistingNotFoundBatchEntityTest | ok=3 failed=0 | ok=3 failed=0 |
| 8 | LazyCollectionDeletedTest | ok=1 failed=0 | ok=1 failed=0 |
| 9 | BidirectionalProxyTest | ok=1 failed=0 | ok=1 failed=0 |
| 10 | SimpleUpdateWithLazyLoadingWithCollectionInDefaultFetchGroupFalseTest | ok=3 failed=0 | ok=3 failed=0 |
| 11 | JpaConstructorInitializationAndDynamicUpdateTest | ok=8 failed=0 | ok=8 failed=0 |
| 12 | SharingReferenceTest | ok=2 failed=0 | ok=2 failed=0 |
| 13 | LazyOneToOneMappedByInDoubleEmbeddedTest | ok=2 failed=0 | ok=2 failed=0 |
| 14 | LazyCollectionDetachTest | ok=3 failed=0 | ok=3 failed=0 |

Byte-identical `found`/`started`/`ok`/`failed` counts in every class,
both arms. Class 3 (`BatchingTest`) reports `found=0` in BOTH arms — a
JUnit5 test-discovery quirk in the harness (likely a `@Nested`/parameter-
source shape `DiscoverySelectors.selectClass` doesn't expand), not a JIT
or ban-related issue, since it's identical either way.

No hangs, no crashes, no new failures in 16 total class-runs (1 probe +
15 sample) across both arms.

## Conclusion

`HIB-BYTEBUDDY`'s `net/bytebuddy/` guard in `vm/src/jit/skip_list.rs` is
REMOVED. The 2026-06-13 hang does not reproduce on current `dev`.

See also: `vm/src/jit/skip_list.rs`'s (now-removed) `HIB-BYTEBUDDY`
comment, `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`,
and `docs/known-issues/h2/h2-jitban-longtail1-residuals-20260728.md`
(the same-day re-verification methodology applied to `org/h2/`).
