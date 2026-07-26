# JIT ban sweep — 2026-07-25/26

Tracking doc for the goal "safely remove all app-specific JIT bans now that
the big JIT rework has landed; try every app with JIT and fix newly
discovered bugs." Source of truth for what's banned: `vm/src/jit/skip_list.rs`.
This file lives at `docs/internal/jit-ban-sweep-20260725.md` once committed;
this scratchpad copy is the working draft.

Worktree: `/data/data/wt-jitban-20260725`, branch `fix/jit-ban-sweep-20260725`.
Binaries: `/data/tmp/jitban-bins/cratonvm-jitban-<label>-<date>` (unique names
per the standing workflow, never reuse `target/release/cratonvm` directly for
timed runs — it gets clobbered by concurrent rebuilds).

**IMPORTANT — lane coordination:** another concurrent session on this host
owns `org/junit/runners/model/TestClass.collectAnnotatedMethodValues`
(TOMCAT-DOHEAD-JUNIT-ITERATOR.1, skip_list.rs ~L792). Do not touch that entry
from this branch.

## How the file is structured

- Lines ~490-915: targeted bans that apply under **both** `SkipPolicy::Conservative`
  and `SkipPolicy::Aggressive` (i.e. NOT liftable by flipping the policy —
  only by `CRATONVM_JIT_ALLOW_PACKAGES=<prefix>` matching the exact ban
  prefix string, or by editing the code).
- Lines ~920-2001: wrapped in `if policy == SkipPolicy::Conservative { ... }`
  — this is almost the entire per-framework blanket-ban list (springframework,
  jboss/wildfly, hibernate helpers, keycloak, reactivex, bouncycastle non-hotpath,
  intpoly, bytebuddy, log4j, junit-the-runner-itself, slf4j/logback/commons-logging,
  cglib, hsqldb, eclipse-jdt, hamcrest, minidev-json, unboundid-ldap, netflix,
  feign, elasticsearch(!), snakeyaml, sun/java beans...). **`jit_aggressive_compilation`
  has NO CLI/env wiring today** (only a `VmConfig` struct field, set `false` by
  default, flipped only in Rust unit tests) — so exercising this path from a
  suite run requires either adding CLI plumbing (`--jit-aggressive` flag or a
  `CRATONVM_JIT_AGGRESSIVE=1` env check in vm-cli) or testing package-by-package
  via the already-wired `CRATONVM_JIT_ALLOW_PACKAGES` env var. Went with the
  latter to avoid code-risk before any verification.
- Every ban site's comment documents its own `CRATONVM_JIT_ALLOW_PACKAGES=`
  lift recipe — use it verbatim to test.

## Host gotchas hit this session

- Root filesystem (`/`, `/tmp`) was **100% full (29G/29G, 0 avail)** —
  `cc`/rustc intermediate `.s` files failed with "No space left on device",
  masquerading as a build failure. Fix: `TMPDIR=/data/tmp` for cargo builds
  (`/data` has 143G free on nvme1n1). Also affects **test working directories**
  — H2 test classes write relative `./data/...` — must `cd` into a `/data/tmp/...`
  workdir before invoking the VM, or they hit the same ENOSPC.
- `cargo` isn't on PATH for non-interactive SSH — `source ~/.cargo/env` first.
- H2 test classes are their own JUnit-less main() entry points
  (`TestBase.createCaller().init().testFromMain()`) — invoke the class
  directly as the main class, NOT via `org.junit.runner.JUnitCore`.
- H2 classpath = `$H2_ROOT/target/classes:$H2_ROOT/target/test-classes:$(cat $H2_ROOT/craton-testcp.txt)`
  (`H2_ROOT=/data/data/h2database/h2`); the bare `craton-testcp.txt` alone
  is missing the H2 classes themselves.

## Status legend

TODO = not yet tested · TESTING = build/run in flight · KEPT = tested, still
needed (root cause not yet fixed, or newly-discovered bug found and fixed but
ban intentionally narrowed rather than removed) · REMOVED = ban deleted from
skip_list.rs, verified clean, committed.

## Targeted bans (apply under BOTH policies — need explicit per-prefix testing)

| Tag | ~Line | Prefix/Class.method | Notes | Status |
|---|---|---|---|---|
| (generic) | 494 | `java/util/Collections.indexedBinarySearch` | ES812 lambda-receiver residual | TODO (not app-specific, skip) |
| (generic) | 507 | `java/util/stream/MatchOps.{makeInt,makeRef,makeLong,makeDouble}` | ES-PERF uncommon-trap | TODO (not app-specific, skip) |
| SPRING-TESTCOMPILER.1 | 526 | `com/sun/tools/javac/api/JavacTool.getTask` | in-process javac miscompile | TODO |
| SPRING-TESTCOMPILER.2 | 561 | `com/sun/tools/javac/jvm/ClassReader.readClass` | ditto | TODO |
| SPRING-TESTCOMPILER.3 | 595,599 | `ClassFinder.{complete,fillIn}` | ditto | TODO |
| (javac family) | 603,607 | `ClassReader.{readInnerClasses,readAttrs}` | ditto | TODO |
| HIB-STOREDPROC-JIT.1 | 620 | `Symbol$ClassSymbol.complete` | H2 CREATE ALIAS javac | TODO |
| SPRING-TESTCOMPILER.4 | 629 | `org/springframework/javapoet/CodeBlock$Builder.add` | shaded javapoet | TODO |
| SPRINGBOOT-WITHOUT-JACKSON.2 | 648 | `ModifiedClassPathClassLoader.loadClass` | boot testsupport | TODO |
| (generic) | 740 | `java/math/MutableBigInteger` (whole class) | Hibernate AIOOBE | TODO (VM-wide, low priority) |
| ANTLR-COLDPATH.1 | 751 | PredictionContext cluster (fn `is_antlr_prediction_context_miscompile`) | stays banned even w/ groovyjarjarantlr4/ allowed | KEPT by design |
| PROXY-JITCALL.1 | 763 | `$ProxyN` dynamic-proxy classes (fn `is_generated_proxy_class`) | JIT-JIT call boundary register bug | TODO — good rework-relevance candidate |
| SPR-AOT-TESTNG-MAPS.1 | 784 | `org/testng/collections/Maps` | TestNG helper | TODO |
| TOMCAT-DOHEAD-JUNIT-ITERATOR.1 | 792 | `TestClass.collectAnnotatedMethodValues` | **OWNED BY ANOTHER SESSION — DO NOT TOUCH** | SKIP (lane conflict) |
| REACTOR-ADDCAP.1 | 814 | `reactor/core/publisher/Operators.addCap` | demand accounting | TODO |
| REACTOR-FLUXCREATE.1 | 822,825 | `FluxCreate$BaseSink.addCap`, `FluxCreate$BufferAsyncSink.drain` | ditto | TODO |
| JETTY-WSIO.1 | 836 | `org/eclipse/jetty/{websocket,io}/` | needs both compiled together to repro | TODO |
| **HIB-LONGTAIL.1** | 871 | `org/h2/`, `org/antlr/v4/runtime/` | **IN PROGRESS THIS SESSION** — perf-motivated, not correctness | TESTING |
| HIB-LONGTAIL.2 | 882 | `org/xml/sax/helpers/AttributesImpl.ensureCapacity` | anewarray corruption | TODO |
| HIB-LONGTAIL.3 | 892 | `GenerationTargetToScript.<init>` | uninit field | TODO |

## Conservative-only block (L920-2001) — liftable per-prefix via CRATONVM_JIT_ALLOW_PACKAGES

Not yet individually tested this session. Full prefix list (from grep, see
skip_list.rs L935-1990): eclipse-jdt parser+ast, elasticsearch(!), hamcrest,
wildfly-controller, minidev-json, unboundid-ldap, keycloak/picocli/smallrye,
rxjava3, bouncycastle (non-hotpath), intpoly, bytebuddy, randomizedtesting,
log4j, junit/+junit-the-jar, springframework.{util,core,boot.context.properties.bind,
boot.context,boot(!),cloud}, groovyjarjarantlr4, antlr-runtime(again, redundant
with HIB-LONGTAIL.1?), netflix-discovery, feign, jboss.{modules,as},
wildfly, jboss.msc, jboss.logging, slf4j, logback, commons-logging, cglib,
hsqldb, springframework.boot.loader, spring-web-reactive, spring-boot-web-reactive,
spring-beans-factory-support, spring-context-annotation, spring-context-support,
spring-core-io-support, sun-beans, java-beans, spring-beans-factory,
junit-platform-console-picocli.

**Not yet catalogued individually — next session(s) should read L920-2001
in full and expand this table before testing.**

## Session log — 2026-07-26 (first working session)

- Discovered `/data/data/wt-jitban-20260725` (branch `fix/jit-ban-sweep-20260725`,
  pre-existing from an earlier session) was **actively being edited by another
  concurrent session right now** — caught an uncommitted `if false &&` guard
  on the exact `TestClass.collectAnnotatedMethodValues` line the user told me
  to avoid. **Moved to a fresh, exclusively-owned worktree**:
  `/data/data/wt-jitsweep2-20260726`, branch `fix/jit-ban-sweep2-20260726`,
  branched from `origin/dev` @ `4493d9265`. Use this one going forward, not
  the `-2025` one.
- Built baseline binary (no code changes yet, just to test via
  `CRATONVM_JIT_ALLOW_PACKAGES`): `/data/tmp/jitban-bins/cratonvm-jitban-baseline-20260726`.
- **`git blame`/`-S` check on the springframework/jboss/slf4j/cglib/hsqldb
  blanket-ban block (L920-2001): every single entry (SPB.1 through SPB.9d,
  CGL.1, PIC.1 — ~25 distinct package bans) cites the identical
  "allocate-then-putfield" / callee-saved-GPR-clobber signature as its root
  cause, and traces back to commits from ~2026-05-05 (Session 111-118) —
  i.e. ALL predate the 2026-07-04 general fix** (`docs/internal/fixed-suite-bugs/jit-regalloc-callee-saved-clobber-family.md`,
  default-off callee-saved GPR local homes) that was specifically supposed to
  fix this exact symptom family. None of these ~25 bans appear to have been
  revisited since. **This is the single highest-value lead in the whole
  sweep** — if confirmed, it's a ~25-entry removal in one coherent batch, not
  25 separate investigations. BUT: unverified so far — the original crash
  fixture apps (SportMe, ms-course-youtube/admin-service, insurance-backend,
  eureka-server, msyt-admin, cglib_probe) are **not present on this host**,
  so verification needs either (a) a full app-suite run that exercises these
  packages (Spring Boot suite, WildFly boot — `/data/data/wildfly-dist-keep/wildfly-32.0.1.Final`
  is available) or (b) a hand-written minimal repro per package. Next
  session: try WildFly boot first (covers `org/jboss/modules,as/`,
  `org/wildfly/`, `org/jboss/msc/`, `org/jboss/logging/` — 5 of the ~25 in
  one shot) since WildFly-boot-under-CratonVM is well-precedented on this
  host (see `docs/internal/fixed-suite-bugs/wildfly/*`). Needs
  `CRATONVM_JAVA_HOME` set alongside any JAVA_HOME shim per
  `wildfly-jboss-modules-inputstreamreader-clinit-race.md`.
- **H2/ANTLR-runtime ban (HIB-LONGTAIL.1) — IN PROGRESS.** Single-class
  differential (`TestFileSystem`, lifted vs. baseline) was inconclusive: both
  hit the 400s timeout at the identical point (stuck after the RRWL CAS diag
  warnings, no further progress) — this class is independently known to be
  right at the perf ceiling regardless of this ban (see
  `[[h2-testfilesystem-testconcurrent-three-fixes-perf-gap-open]]`), and the
  host was extremely loaded (~70+ concurrent cargo/rustc processes from other
  sessions) making single-class wall-clock comparisons unreliable today.
  **Switched to a full-suite differential instead**: 4-way sharded
  `run-h2-suite.sh run --category all --shard i/4 --tag h2ban-lifted-si`
  with `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/,org/antlr/v4/runtime/`, output in
  `/data/tmp/jitban-h2-full/h2ban-lifted-s{1,2,3,4}-*/`. Compare final
  PASS/HANG/FAIL counts against the existing baseline in
  `apps/h2database-suite-runner/RESULTS-20260724.md`: **PASS 143 (65.6%) /
  HANG 56 (25.7%) / FAIL 19 (8.7%)** (with the ban in place, same host,
  similar contention level). Launched ~00:58 UTC 2026-07-26; **still running
  as of this doc update — check `/data/tmp/jitban-h2-full/*/summary.txt` for
  completion, or `results.tsv` line counts for progress.** First shard
  collision gotcha: giving all 4 shards the same `--tag` makes them share one
  output directory name (stamp-only, no shard index) and clobber each
  other's `results.tsv` concurrently — always pass a per-shard-unique
  `--tag`.

## WildFly boot smoke test (SPB.8/8b/8c family: jboss.modules, jboss.as, wildfly, jboss.msc, jboss.logging)

Direct `standalone.sh` boot against `/data/data/wildfly-dist-keep/wildfly-32.0.1.Final`
(fake JAVA_HOME wrapping the cratonvm binary as `bin/java`, `CRATONVM_JAVA_HOME=/home/victor/jdk25`
for real-class support — see `docs/internal/fixed-suite-bugs/wildfly/wildfly-gc-barrier-boot-hang-and-harness-fixes.md`
for why this shape is needed).

- **Baseline (ban in place, default):** boots cleanly to `WFLYSRV0025: WildFly Full
  32.0.1.Final ... started in 33227ms`, HTTP management interface up, deployment
  scanner polling normally. Genuinely working server.
- **Ban lifted (`CRATONVM_JIT_ALLOW_PACKAGES=org/jboss/modules/,org/jboss/as/,org/wildfly/,org/jboss/msc/,org/jboss/logging/`):**
  boots ~39 threads deep into real config parsing, then **FATAL WFLYSRV0056** —
  `NullPointerException: Cannot invoke "java.util.Set.iterator()" because
  "this.validTypes" is null`, in the `standalone.xml` EE-subsystem
  managed-executor-service parse path (`ModelTypeValidator.validateParameter` /
  `LongRangeValidator`/`NillableOrExpressionParameterValidator` chain — note
  CratonVM's own stack attribution across these is probably imprecise, matches
  the known "JIT loses/mis-attributes an inlined callee's frame" pattern seen
  elsewhere in this codebase, e.g. JASPER-JDT.3). **This is a real regression
  from lifting the ban, not a stale safety net for this particular repro.**
- **`--nojit` check in progress** (ban still lifted via env, but `--nojit` added)
  to confirm this is JIT-specific before spending more time on it — result
  pending as of this doc update.

**CONFIRMED via `--nojit` differential (2026-07-26 ~01:10 UTC): this is
JIT-specific, not an environment/harness artifact.** Ban lifted + `--nojit`
added boots cleanly (`WFLYSRV0025 ... started in 24042ms`, matches baseline);
only ban-lifted + JIT-on fails. **Verdict: KEEP `org/jboss/as/` (SPB.8b) —
it is protecting against a currently-live miscompile, not a stale one.**
Full writeup + repro: `docs/known-issues/wildfly/modeltypevalidator-validtypes-npe.md`
(committed). This is the sweep's first concrete "new bug found" per the
goal's own framing — a real, JIT-only `ModelTypeValidator`/`validTypes`
null-field bug with a fast (~10-20s) deterministic repro, not yet root-caused
to an exact codegen site. Good target for a dedicated follow-up session.

NOT proof that the *other* ~20 bans in the SPB/CGL/PIC family are still
needed — each needs its own check; this only confirms the family's root
cause pattern is still real *somewhere*, so don't assume the whole family is
safe to bulk-remove. The sibling bans in the same lift set this test used
(`org/jboss/modules/`, `org/wildfly/`, `org/jboss/msc/`, `org/jboss/logging/`)
are individually UNTESTED — this run never got far enough to exercise them
since it crashed first in `org/jboss/as/`. Re-test those once the `org/jboss/as/`
bug above is fixed (boot will get further and may expose or clear different
bans downstream).

Repro commands (host: victor@20.83.144.174):
```bash
mkdir -p /data/tmp/jitban-wf-javahome/bin
cp <cratonvm-binary> /data/tmp/jitban-wf-javahome/bin/java
chmod +x /data/tmp/jitban-wf-javahome/bin/java
mkdir -p /data/tmp/<rundir> && cp -r /data/data/wildfly-dist-keep/wildfly-32.0.1.Final/standalone/configuration /data/tmp/<rundir>/
JAVA_HOME=/data/tmp/jitban-wf-javahome CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  CRATONVM_JIT_ALLOW_PACKAGES='org/jboss/modules/,org/jboss/as/,org/wildfly/,org/jboss/msc/,org/jboss/logging/' \
  timeout 90 bash /data/data/wildfly-dist-keep/wildfly-32.0.1.Final/bin/standalone.sh \
  -Djboss.server.base.dir=/data/tmp/<rundir>
```
(standalone.sh isn't chmod +x in the dist — always invoke via `bash standalone.sh`.)

## H2/ANTLR-runtime ban (HIB-LONGTAIL.1) — STILL NEEDED, but the reason below is now WRONG

> **2026-07-26 update.** The `Schema  not found` cluster described in this
> section was root-caused and fixed (`13055f75c`): the inlined `java/lang/String`
> JIT intrinsics read `coder` and `hash` 4 bytes past their real addresses in a
> COMPACT instance, so `length()` computed `value.length >> (hash & 31)` for any
> receiver whose lazy hash cache was populated. H2's interned `"PUBLIC"` schema
> name is such a receiver, so it persisted `CREATE SEQUENCE ""."SEQ1"` into its
> own metadata and could never reopen the database. A second x64 defect behind
> it (a reload-elision mirror leaking across a control-flow join) made
> `ConnectionInfo.getProperty(key, default)` return null. **Both are general
> x64-backend bugs, not H2 bugs**, and the cluster is now extinct: 0 occurrences
> across a full 218-class lifted run. The ban still stays — a same-binary
> 218-class A/B gives PASS 158 (ban in place) vs 149 (lifted), with 9 enumerated
> regressions — but for those 9 classes, not for the systemic corruption below.
> See `docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md`, which
> has been rewritten with the A/B table and the residual list.

### Original 2026-07-25 finding (superseded, kept for the record)

The full-suite differential (99/218 classes completed before the background
job was interrupted mid-run — still a large, representative sample) found a
**systemic correctness bug**, not just the perf issue the ban's own comment
describes: 16/40 FAILs are the identical `Schema  not found` (blank schema
name) error, hit across 15+ completely unrelated test classes, all during
DB-reopen metadata replay (`Database.executeMeta` → `Parser.getSchema`).
HANG rate did drop as the ban's perf framing predicted (6% vs. baseline's
25.7%), but that's overshadowed by the new correctness failures. **Verdict:
KEEP.** Full writeup: `docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md`
(committed). This is the sweep's second concrete "new bug found" per
the goal's framing (after the WildFly `org/jboss/as/` one below).

## Coordination with other concurrent sessions (2026-07-26, mid-session)

The user flagged that other sessions are doing the same jit-ban-sweep work
in parallel on this host. Found their shared doc:
`/data/data/wt-jitban-20260725/docs/known-issues/jit-skip-list-open-bans-20260725.md`
(their worktree, not yet merged to dev as of this update) — read it in full,
key takeaways:
- They discovered `CRATONVM_JIT_DENY=<substring>` (forces interpretation,
  no rebuild) as a fast bisection tool — useful complement to
  `CRATONVM_JIT_ALLOW_PACKAGES` (lifts a ban, also no rebuild for most
  entries — their doc says ALLOW_PACKAGES "does NOT lift unconditional
  targeted bans", which is only partially true: I confirmed HIB-LONGTAIL.1,
  the whole SPB/CGL/PIC family, bouncycastle, intpoly, bytebuddy etc. all DO
  respond to `CRATONVM_JIT_ALLOW_PACKAGES` despite being "unconditional
  targeted bans" outside the `SkipPolicy::Conservative` gate — only the
  javac-family bans (JavacTool.getTask, ClassReader.*, ClassFinder.*,
  Symbol$ClassSymbol.complete, javapoet CodeBlock.Builder.add,
  ModifiedClassPathClassLoader.loadClass) and a few others with no
  `package_allowed()` check genuinely need a source edit).
- They landed **TYPES-ERASURE.1**: `com.sun.tools.javac.code.Types.erasure`
  was an undiscovered 8th javac-JIT-family miscompile (same family as
  SPRING-TESTCOMPILER.1-4 / HIB-STOREDPROC-JIT.1), bisected via
  `CRATONVM_JIT_DENY`, committed on their branch
  (`fix/jit-ban-sweep-20260725` @ `303121033`, not yet on `dev`). Their own
  next-step note: fixing/banning `Types.erasure` alone might make the other
  7 javac-family bans redundant — unverified, high-value follow-up for
  whoever gets there.
- They explicitly flagged the AQS/RRWL family (`is_known_miscompile_aqs_family`)
  and CLQ family as tied into a **separate, live H2 `testConcurrent` perf
  investigation** — "do not touch without reading that context first". This
  is DIFFERENT from `HIB-LONGTAIL.1` (the org/h2+antlr package ban I tested
  above) but still worth remembering if working AQS/RRWL-adjacent H2 code.
- Their doc independently recommends the exact same next step I'd already
  started (fresh bisection on an SPB.x ban to check if the
  allocate-then-putfield theory still holds) — my WildFly `org/jboss/as/`
  finding above directly answers this: **yes, it still holds, at least for
  that one ban.**
- User clarified (after initial confusion) that *I* am the session doing H2
  jitban work — no actual collision, just cross-session awareness-sharing.
  No lanes need to change based on this exchange.

## SPB.1 (`org/springframework/util/`) — priority item 2 from the shared
## coordination doc — RESULT: INCONCLUSIVE, ban KEPT

Full writeup: `docs/known-issues/spb1-springframework-util-investigation.md`,
repros in `docs/known-issues/repros/spb1-classutils/`. Short version: two
clean synthetic repros (single `ClassUtils.<clinit>` trigger, with/without
HashMap-machinery warmup) passed identically in both configs; a third,
GC-pressure + classloader-churn repro crashed BOTH baseline and lifted
(differently) — since baseline (ban active) also crashed, this can't be
cleanly attributed to lifting the ban, so no positive evidence to remove
it. The repro-3 crash itself is flagged as a separate, possibly-serious
open issue (GC-root/classloader-churn heap corruption) independent of
SPB.1, for anyone who wants to chase it separately.

**Running tally across this session's SPB/CGL/PIC-family tests:**
`org/jboss/as/` (real WildFly boot) and `org/h2/` (real 218-class H2 suite)
both confirmed still-needed via real-app testing; `org/springframework/util/`
(synthetic repros only, no fixture app available) came back inconclusive.
Lesson for future items in this family: prefer a real app/suite when one's
available — synthetic repros for this specific bug class have been hard to
construct faithfully so far (2/2 clean synthetic tests, 1/1 real-app tests
found real bugs).

## ANTLR.1 blocked, pivoted to TOMCAT-JNDIREALM — then discovered a
## same-time collision with the other session

Priority item 3 (ANTLR.1, `groovyjarjarantlr4/`) is blocked: exhaustively
searched every jar on this host, the shaded package doesn't exist anywhere
(checked groovy-3.0.21/3.0.8/4.0.22 directly, zero `antlr` entries — modern
Groovy apparently ships it in a separate, unresolved module). Didn't want
to speculatively fetch dependencies to chase it down. Left unclaimed in the
shared doc for whoever has a Groovy fixture.

Pivoted to `com/unboundid/` (TOMCAT-JNDIREALM-RDN.1/JIT.2) — real Tomcat
Linux fixture on this host, ban comment names an exact repro
(`TestJNDIRealmIntegration`, 76 cases). Ran it: baseline 76/76 pass, ban
lifted → **SIGSEGV**, stale-pointer/all-zero-header receiver corruption
during LDAP DN/RDN matching, exactly matching the documented bug. Clean,
decisive confirmation — **KEEP**.

**Then found the other session (`fix/jit-ban-sweep-20260725`) had
independently claimed and tested the exact same ban at essentially the same
timestamp (02:38-02:43 UTC), reaching the identical conclusion with slightly
more detail (they also ran the `--nojit` differential, confirming JIT-
specificity, and noted it hangs rather than just crashing at the 120s
mark).** Their writeup is the more complete one — see
`docs/known-issues/jit-skip-list-open-bans-20260725.md`'s
"TOMCAT-JNDIREALM-RDN.1 / JIT.2" section. Removed my own redundant doc file
rather than commit a duplicate. Independent cross-confirmation isn't
harmful, but it is wasted effort — **lesson: re-fetch the shared doc's
claim markers immediately before starting each new item, not just after
finishing the previous one**, since both sessions are now moving fast
enough that claims can land within minutes of each other.

Running tally: **4 real-app-confirmed still-needed bans this session**
(`org/jboss/as/`, `org/h2/`, `com/unboundid/` ×2 sessions independently),
1 inconclusive synthetic-only test (`org/springframework/util/`), 1 blocked
for lack of fixture (`groovyjarjarantlr4/`). Nothing in this family has
been confirmed safely removable yet by either session.

## Next steps

1. Finish H2/ANTLR-runtime test (this session) — compare `TestFileSystem`
   wall-clock lifted vs baseline, then run full 218-class suite if promising.
2. Expand the Conservative-block table above with exact line numbers.
3. Work app-by-app: H2 (this session) → Tomcat (JASPER-JDT.2/3, jetty-wsio,
   javac family via Hibernate-in-Tomcat paths) → Spring Boot (huge chunk of
   springframework.* + javac family + javapoet) → Keycloak (KC26-PIC,
   KC26-RX, intpoly, smallrye/picocli) → Elasticsearch (whole-package
   Conservative ban + hamcrest + randomizedtesting) → WildFly (jboss.*,
   wildfly-controller) → Kafka.
4. For each verified-safe removal: delete the ban's code + its doc comment,
   update/retire any `docs/known-issues/*` or `docs/internal/*` doc that
   referenced it as open, commit on this branch, periodically merge to `dev`
   and push (don't wait for 100% completion — batch verified wins).
