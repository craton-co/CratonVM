# BUG-O — `LinkedHashSet` iterated in hash order, not insertion order

**Test:** `org.apache.tomcat.util.descriptor.web.TestWebXmlOrdering` (relative
fragment ordering). HotSpot: PASS. **Status: FIXED (the ordering bug); the test
itself is now blocked by a separate deeper issue — see below.**

## Symptom

```java
LinkedHashSet<String> s = new LinkedHashSet<>();
for (String x : {"f","d","b","e","a","c"}) s.add(x);
// CratonVM iterated "abcdef" (hash/bucket order); HotSpot: "fdbeac" (insertion)
```

`LinkedHashMap` itself was already correct — only `LinkedHashSet` was wrong.
`WebXml.orderWebFragments` (the servlet-spec relative `<ordering>` topological
sort) keys off `LinkedHashSet` iteration order for the `before`/`after` sets and
the result, so it produced wrong fragment orders (e.g. `afedcb`).

## Root cause

`LinkedHashSet` (and `CopyOnWriteArraySet`) share CratonVM's native `HashSet`
method surface, and `native_hs_init` backed all of them with a plain `HashMap`.
The shared `native_hs_iterator` walks the backing map, which for a `HashMap`
yields bucket order. Nothing made `LinkedHashSet` insertion-ordered (despite a
stale code comment claiming `native_hs_iterator` "already preserves" it).

## Fix

`native-collections/src/lib.rs`: back insertion-ordered set classes
(`LinkedHashSet`, `java.util.concurrent.CopyOnWriteArraySet`) with a
`LinkedHashMap` instead of a `HashMap` (`alloc_hs_backing`/
`hs_is_insertion_ordered`), applied to every init path
(default / capacity / from-collection). The shared iterator then walks the
LinkedHashMap via `lhm_collect_keys` → insertion order. Verified: the probe
iterates `fdbeac`; no regressions in the fast set (no FAIL/PASS transitions).
This is a general fix (any `LinkedHashSet` user).

## Merge status

The cipher regression it exposed is now fixed ([BUG-P](BUG-P-openssl-cipher-sort.md)
— `collect_collection_elements` didn't read a LinkedHashMap-backed set's
overlay), so O + P together are regression-free and also turn `TestResponseUtil`
and `TestOpenSSLCipherConfigurationParserOnly` green. Mergeable to dev.
`TestWebXmlOrdering` itself remains blocked by the separate JUnit-listener
dispatch corruption below (non-deterministic — the wrong receiver class varies
between builds, e.g. `UnmodifiableList`/`DirectJDKLog`.testFinished — only
reached now that the test runs all 720 permutations per method; a distinct deep
follow-up).

## Still blocking `TestWebXmlOrdering` — RE-DIAGNOSED 2026-06-12

The earlier "non-deterministic JUnit `RunListener` dispatch lands on
`cratonvm/internal/UnmodifiableList`, wrong receiver class varies per build"
theory was a **misdiagnosis**. Two unrelated things were conflated:

1. **Cross-session kill artifact (NOT a VM bug).** The "silent rc=1, no output,
   dies at a different method each build" symptom is the documented cross-session
   `taskkill /F /IM cratonvm.exe`: concurrent test sessions kill *every*
   cratonvm.exe by image name, including unrelated runs. Proven by running a
   **uniquely-named copy** of the binary — it never silently dies. (The
   `java.lang.Object.getOrder` / `UnmodifiableList.testFinished` wrong-receiver
   errors only appear under `CRATONVM_DBG_FORCE_MOVING=1`, a non-default
   collector with a separate root-update gap; the suite uses the non-moving
   sweep.)

2. **Native-collection overlay GC-root LEAK (the real VM bug — FIXED, branch
   `fix/lhm-overlay-gc-leak`, worktree `C:/craton/CratonVM-tomcat`).** Each
   relative-ordering method runs `WebXml.orderWebFragments` 720× over 7 fresh
   `WebXml` objects, each holding several `LinkedHashSet` fields. CratonVM backs
   `LinkedHashSet` with a `LinkedHashMap` whose head/tail/table live in the
   process-global `lhm_overlay`. `gc_scan_collection_overlay_roots` rooted
   *every* overlay entry's refs unconditionally and never pruned them, so every
   LinkedHashMap/Set ever created **pinned its backing forever** → the 64 MB
   young gen filled → `OutOfMemoryError: young gen exhausted` (deterministic;
   reproduced with a unique-named binary that cannot be taskkilled). Fix: since
   `lhm_set` already mirrors head/tail/size/table into the real heap fields, the
   overlay roots are redundant for live maps; skip rooting heap-backed entries so
   dead ones get reclaimed (`lhm_heap_backed`). Verified: WxoDriver passes all
   reached methods with correct assertions (was OOM); a 15k-churn probe keeps a
   kept LHM/LHS intact (no false-prune); bintrees16 checksum == HotSpot.

   **Residual (throughput, not yet green in-suite):** the fix stops the OOM but
   the overlay still *accumulates* dead entries (rooting skipped, entries not
   pruned), so each GC's overlay walk grows O(n) and later methods slow down;
   combined with interpreter throughput the full test runs ~minutes and exceeds
   the harness's 90 s per-class timeout. Pruning needs a safe owner-liveness
   signal across the non-moving sweep + the packed `(identity_hash, generation)`
   overlay key — a deeper GC change left as follow-up. Full notes:
   `apps/tomcat/.tooling/WXO_FINDINGS.md`.
