# Undocumented blanket `org/junit/` ban — a new, unexplored finding (not yet investigated)

**Status: still active, NOT removed, NOT further investigated this session — flagged for a dedicated future session.**

## What was found

While re-testing `TOMCAT-DOHEAD-JUNIT-ITERATOR.1`'s removal, discovered that
`vm/src/jit/skip_list.rs` has a separate, much broader blanket ban on the
**entire `org/junit/` package**:

```rust
if class_name.starts_with("org/junit/") && !package_allowed("org/junit/", allow_packages) {
    return Some(SkipReason::RustJvmTestFixture);
}
```

located a few hundred lines below `should_skip_jit_internal`'s start,
grouped alongside similar blanket bans for `net/bytebuddy/`,
`com/carrotsearch/randomizedtesting/`, and `org/apache/logging/log4j/` —
but unlike every one of those neighbors, **this one has no explanatory
comment of its own** describing why it exists, what miscompile it guards
against, or when it was added.

**It is gated entirely inside `if policy == SkipPolicy::Conservative { ... }`**
(opened well above it) — so it only applies under the default Conservative
policy; `SkipPolicy::Aggressive` bypasses it (and its neighboring blanket
bans) unconditionally, same as the rest of that block.

**It is liftable without a rebuild**: `package_allowed("org/junit/",
allow_packages)` returns true whenever any entry in `allow_packages`
prefix-matches the literal string `"org/junit/"` — i.e.
`CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` (or `CRATONVM_JIT=allow-packages`
in the newer flag spelling from today's JIT rework) lifts it at runtime.

## Where it came from

`git log -S'class_name.starts_with("org/junit/")' -- vm/src/jit/skip_list.rs`
shows exactly one commit ever touched this literal string:
`60ef90d4b`, dated 2026-07-05, titled **"Fix Elasticsearch postings FFM
checksum bridges"** — a large, generically-named commit touching 14 files
across `native-builtins`, `native-collections`, `native-io`, and
`vm/src/jit/skip_list.rs` (56 lines added there). The `org/junit/` ban is
almost certainly incidental collateral in that squash rather than a
deliberately, individually-justified addition — it rode in silently next
to genuinely-documented bans (`net/bytebuddy/`, log4j, randomizedtesting)
without picking up its own rationale comment.

## Why this matters

This ban silently shadows **any narrower, more specific ban on a class
under `org/junit/`** — meaning re-testing such a narrower ban without also
explicitly lifting this blanket one produces a false-clean result: the
probe runs, reports success, but the target method was never actually
JIT-compiled at all (it was still fully interpreted via this blanket
ban the whole time).

This exact trap caught two things this session:

1. **`TOMCAT-DOHEAD-JUNIT-ITERATOR.1`'s first re-test attempt** (this
   session, same day) — initially "confirmed clean" without
   `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` set, which proved nothing.
   Caught before landing; re-tested properly with the blanket ban also
   lifted (via `check_with`/env var), genuinely confirmed clean, and
   removed correctly this time — see the removal comment in
   `skip_list.rs` and `TomcatDoheadJunitIteratorProbe.java`.
2. **`JUNIT.1`'s earlier removal** (this session, already landed on `dev`
   before this was discovered) — its removal comment claimed
   "`JuintCore.main` is JIT-eligible unconditionally now," which is false:
   `org/junit/runner/JUnitCore` is also caught by this same blanket ban,
   and `JUnitCoreMainProbe.java`'s 110-run retest never set the
   allow-packages env var either. The comment has been corrected in
   `skip_list.rs` to accurately describe this as a safe-but-shadowed
   no-op removal (like `SPRINGBOOT-WITHOUT-JACKSON.2` and `HIB-ANTLR.1`),
   not an independently-verified unconditional fix.

## Recommendation for a future session

This ban is a high-value, unexplored target precisely because of its
scope: if it can be safely narrowed or removed, it restores real JIT
eligibility to the **entire JUnit test-running harness** across every
JUnit-based test suite this VM runs (Spring, Hibernate, Tomcat, and
everything else that uses JUnit4/JUnit5 internally) — a much bigger win
than any single narrow ban in this file.

Suggested approach:
1. Run a broad, real JUnit-based test corpus with
   `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` set — e.g. the Hibernate ORM
   harness at `apps/hibernate-orm-harness/` (JUnit5 Platform Launcher,
   hundreds of real test methods) or a Spring suite — and compare pass/
   fail counts and wall-clock behavior (hangs, OOMs, wrong results)
   against the same run without the env var.
2. If clean, bisect which specific `org/junit/` classes/methods (if any)
   still need a narrower, targeted ban, following this file's established
   pattern (see how `HIB-LONGTAIL.1`, `SPB.1`, etc. narrowed from an
   initial blanket ban to specific classes over time).
3. Do not assume "no regressions in a quick probe" is sufficient — this
   ban's total absence of a rationale comment means nobody currently
   knows what it was protecting against, so a real, broad test corpus
   (not a synthetic probe) is the right bar for removing or narrowing it.
