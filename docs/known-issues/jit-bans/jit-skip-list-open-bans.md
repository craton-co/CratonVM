# Active JIT Skip-List Bans (Updated 2026-07-27)

This document catalogs all **currently open/active** JIT correctness bans in `vm/src/jit/skip_list.rs` as of the July 2026 sweep. All resolved, lifted, or dead-code bans from prior investigations have been completely removed from this list.

---

## ⚠️ Critical Testing Methodology & Traps

Before attempting to resolve any of the open bans below, you must account for these established traps:

1. **The Virtual Dispatch Trap (Discovered 2026-07-27):**
   You **MUST** test with `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=1` (or verify it is enabled in your build). If this flag is off, compiled-to-compiled virtual dispatch is skipped entirely, meaning tests will silently pass because JIT-compiled callers are falling back to the interpreter. (This trap falsely cleared several bans previously).
2. **The `cargo test` Trap:**
   Running `cargo test` does **not** rebuild the `cratonvm` executable. Always run `cargo build --release` and check the binary's `mtime` before trusting a "still crashes/now passes" result against a real reproduction.
3. **No-Rebuild Bisection (First Move):**
   You can bisect hypotheses against an already-built binary using env vars:
  * `CRATONVM_JIT_DENY=<substring>`: Force specific `Class.method` substrings to interpret.
  * `CRATONVM_JIT_BISECT_SKIP=<Class.method>,...`: Exact method pairs to interpret.
  * `CRATONVM_JIT_BISECT_ONLY=<prefix>,...`: Only listed prefixes stay JIT-eligible.

---

## 1. Confirmed Still Needed (Requires Root Cause / Fix)

These bans have been recently re-verified against real applications or reproducers. They are active correctness (or throughput) bugs in the JIT.

*   **`TYPES-ERASURE.1` (`com/sun/tools/javac/code/Types.erasure`)**
  *   **Status:** Verified active. Fails deterministically with `NullPointerException: ... "type" is null` from javac's own `Lower.boxIfNeeded` after ~8 iterations of compiling trivial `@Deprecated` classes.
  *   **Note:** Specifically confirmed to be independent of the other javac-family bans.
*   **`SPRING-TESTCOMPILER.1-4` & `HIB-STOREDPROC-JIT.1` (javac family)**
  *   **Status:** Verified active. 7 specific bans in `com/sun/tools/javac/{jvm,code}/*`.
  *   **Note:** A consolidation hypothesis (that fixing `Types.erasure` would subsume these) was directly **refuted**. Lifting these while keeping `Types.erasure` banned reproduces the original `ClassReader.readClass` NPE. `ClassFinder.complete` is identified as load-bearing but not fully isolated.
*   **`HIB-BIGINTEGER-AIOOBE.1` & `.2` (`java/math/BigInteger`, `java/math/MutableBigInteger`)**
  *   **Status:** Verified active. Real correctness/performance tradeoff for widely-used JDK classes. A deterministic reproducer exists.
  *   **Note:** Can be narrowed if someone root-causes the exact multi-method interaction (e.g., constructor + some combo of `trustedStripLeadingZeroInts` / `destructiveMulAdd` / `checkRange` / `parseInt`).
*   **`HIB-TEMPORAL.1` (`org/hibernate/`)**
  *   **Status:** Verified active. Tested against a real Hibernate ORM 8.0 test harness.
  *   **Note:** Bug is *more* severe than originally documented: lifting it causes a full `StrategySelectionException` Hibernate bootstrap failure, not just a narrow DDL-descriptor NPE.
*   **`JASPER-JDT.2` (`org/eclipse/jdt/internal/compiler/parser/`) & `JASPER-JDT.3` (`.../ast/`)**
  *   **Status:** Verified active.
  *   **Note:** Temporarily removed but **RESTORED 2026-07-27** when it was discovered that without `direct_virtual_compiled_callee_entry_enabled()`, the defect was hidden. With the flag on, real Tomcat `jakarta.el.TestOptionalELResolverInJsp` fails (HTTP 500 / `ClassCastException`).
*   **`JASPER-JDT.3` Residual Bug (`org/apache/catalina/webresources/AbstractResourceSet.checkPath`)**
  *   **Status:** Open bug (not just a ban to lift).
  *   **Note:** Throws an independent JIT-only `IllegalArgumentException: The requested path ... must begin with /` for paths that *do* start with `/` (i.e. `path.charAt(0) != '/'` evaluates true incorrectly). Requires real Tomcat call context to reproduce; standalone probes do not trigger it.
*   **`ANTLR-COLDPATH.1` (ATN config-context)**
  *   **Status:** Verified active. Narrow 7-method guard covering shaded and unshaded runtimes.
  *   **Note:** Kept specifically for correctness safety-net. (The broader package-level throughput bans around it have been resolved/removed, but these 7 specific methods must remain interpreted).
*   **`WILDFLY-CONTROLLER-JIT.1` (`org/jboss/as/controller/`)**
  *   **Status:** Kept pending its own dedicated re-test.
*   **`org/bouncycastle/` (BC-JIT family)**
  *   **Status:** **MUST STAY, permanently.**
  *   **Note:** Kept for a genuine throughput wall (SPHINCS-256 requires near-HotSpot crypto throughput the JIT cannot currently deliver). Do not re-litigate without massive JIT optimizations.
*   **`LUCENE-POSTINGS.1` (`org/apache/lucene/*`)**
  *   **Status:** Ban stays for a residual issue post-MatchOps fixes.
*   **AQS / RRWL / CLQ Families**
  *   **Status:** Kept active.
  *   **Note:** Tied into a separate, live H2 `testConcurrent` performance investigation. Do not touch without consulting that context first.

---

## 2. Unverified / Blocked Backlog (Needs Environment / Fixture)

These bans remain active because the original environments/fixtures used to reproduce them are currently missing from the test hosts, or they have not yet been reached in current testing sweeps.

*   **SPB Package-Level Family (`SPB.2`, `.4`, `.4b`, `.4c`, `.5`, `.6`, `.7`, `.8`, `.9`, `.9b`, `.9c`, `.9d`, `CGL.1`, `PIC.1`)**
  *   **Status:** Blocked.
  *   **Note:** This is the remaining "allocate-then-putfield" hypothesized bug cluster (e.g., `org/springframework/core/`). The original crash-fixture apps (`SportMe-master`, `ms-course-youtube`, `insurance-backend`, `eureka-server`, `msyt-admin`, `cglib_probe`) have been double-confirmed **absent** from the host.
  *   **Next Steps:** Requires fetching one of these checkouts or building an equivalent robust Spring Boot fixture. Before assuming a JIT miscompile, verify the GC-root-scanning gap (`gc/src/vm_heap.rs::metadata_pin_deferrable`) if the target package's classes are loaded via a user-defined `ClassLoader`.
*   **`HIB-LONGTAIL.3` (`GenerationTargetToScript.<init>`)**
  *   **Status:** Blocked.
  *   **Note:** Needs a real `org.hibernate.tool` (hibernate-tools) jar to test.
  *   **Next Steps:** Check whether this constructor classifies as `InitComplexity::Trivial`. If so, it might already be safely shadowed by the generic non-trivial-constructor JIT gate (meaning the specific ban could be safely removed as redundant).
