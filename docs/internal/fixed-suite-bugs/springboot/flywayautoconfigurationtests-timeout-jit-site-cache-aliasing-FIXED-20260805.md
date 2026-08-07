# `FlywayAutoConfigurationTests` — 300s TIMEOUT — RESOLVED

**Status: FIXED (2026-08-05).** Filed the same day as a "silent hang after
HSQLDB `DbValidate`"; the hang was neither silent nor about HSQLDB. It was a
JIT dispatch-aliasing bug that made 39 of the class's 73 tests fail, and the
failure handling is what pushed the class past the suite's 300s ceiling.

## Original symptom (as filed)

`module/spring-boot-flyway`'s `FlywayAutoConfigurationTests` timed out at the
suite's 300s per-class budget (`TIMEOUT`/`HANG`, `rc` never returned) in the
2026-08-05 Azure full-suite run, while the HotSpot baseline passed the class
cleanly (`PASS 0/73`). The `.out.log` showed normal progress through several
embedded-DB migration cycles and then stopped after Flyway's HSQLDB
`DbValidate` reported zero migrations — ~212 seconds with nothing further on
stdout or stderr.

The original page read that silence as a genuine stall and named the
2026-07-12 `flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md` `org/hsqldb/`
JIT ban as the reason the HSQLDB path was interpreted. **Both readings were
wrong.** That ban had been deleted on 2026-08-01 (`d1979bec5`, "delete the
static ban machinery outright"), so HSQLDB was JIT-eligible again in the run
that hung — and the stall was not in HSQLDB at all.

## Root cause

`383e7f5cf` — *a recycled `JitInvokeInfo` address let one call site serve
another's dispatch*.

Every per-thread dispatch memo in `vm/src/jit/helpers.rs` is keyed on
`JitSiteKey = (vm_identity, JitInvokeInfo pointer)`. `JitInvokeInfo` boxes are
owned by `CompiledMethod::_jit_invoke_infos` and are freed when the method
drops, after which the allocator can hand the same address to the next
compile. `flush_raw_entry_dispatch_caches` cleared only `DISPATCH_CACHE` and
`VIRTUAL_DISPATCH_CACHE` on a JIT generation change — `NATIVE_SITE_CACHE`,
`VIRTUAL_TARGET_CACHE`, the two counters and the two object/integer native
dispatch caches were left holding the *previous* site's answer under a key
that now named a different site.

The 2026-08-04/08-05 leaf-native work is what made this reachable at scale
here: `836631dcc` ("site-cache EVERY native from compiled code, not just the
leaves") put a resolved native callback in `NATIVE_SITE_CACHE` for every
native call from compiled code, so an aliased key means the site **CALLs a
different native and returns whatever that returns**.

That is exactly the shape of every failure in this class. All of them sit in
Spring Framework 7's `java.lang.classfile`-based annotation metadata reader
(`org.springframework.core.type.classreading.ClassFile*`), which is
native-call-dense and runs once per `ApplicationContext` refresh — 73 times
in this class:

| Observed exception | What the aliased site returned |
|---|---|
| `IllegalArgumentException: Bad method descriptor: (L…;)Lorg/flywaydb/core/Flyway;` — a **valid** descriptor | a `String`/char accessor answered from the wrong native, so `ConstantUtils.skipOverFieldSignature` returned 0 mid-descriptor |
| `ClassCastException: class java.lang.Class cannot be cast to class java.lang.classfile.constantpool.PoolEntry` at `ClassReaderImpl.checkType` | `Class.cast(e)` returned the **receiver** (`cls`) instead of its argument |
| `NullPointerException: Class.isAssignableFrom: argument is null` (Jackson `ClassUtil.canBeABeanType`) | the site was entered with the argument slot empty |
| `ConstantPoolException: Bad CP index: 23296` (sibling doc, webmvc) | an index-returning native answered from another site |

The 08-02 full-suite run predates `836631dcc`, which is why the class was
green then (`PASS 220.799s`) and hung on 08-05.

## Why it read as a *silent* hang

It is not a stall. On the pre-fix binary the class still runs to completion —
it just does 39 failing `ApplicationContext` refreshes on top of the 34
successful ones, and SbRunner emits each failure's `SBRUNNER_FAILURE_DETAIL`
only in the end-of-run summary. During the failing stretch the only thing that
would print is Spring's own progress logging, which a context that dies during
`ConfigurationClassParser.parse` never reaches. Local runs show inter-line
gaps up to 12.3s on an otherwise busy box; the 08-05 shard ran 16 CratonVM
processes on 16 cores, which stretches that into the observed ~212s of quiet
before the 300s `timeout` kill.

## Validation

Fixture: `apps/spring-boot/module/spring-boot-flyway` (local Windows) and
`/data/data/springboot-jsonreader-deprecation-20260718` (Azure Linux), one
process per class, the runner's three env vars set.

**Local (Windows), `run-single-class.ps1` launch, `--Xmx 2g`:**

| Binary / arm | Result | Wall |
|---|---|---:|
| dev `4192dec7c` (pre-fix), default JIT | **39/73 FAIL** | 245.6s |
| dev `4192dec7c`, `--nojit` | 73/73 PASS | 250.5s |
| dev `4192dec7c`, `--Xmx 8g` | 44/73 FAIL | 227.9s |
| dev `4192dec7c`, `CRATONVM_ROOTSNAP_CACHE=0` + `…_SURVIVE_GC=0` | 35/73 FAIL | 420.5s |
| dev `4192dec7c`, `CRATONVM_JIT_DENY=jdk/internal/constant/` | 69/73 FAIL | 24.4s |
| **dev with `383e7f5cf` merged, default JIT** | **73/73 PASS** | 214.1s / 220.9s |
| HotSpot 25.0.3+9 control, same classpath | 73/73 PASS | 9.3s / 11.4s |

`--nojit` passing while default JIT fails is what identified this as a JIT
defect; heap size and the rootsnap cache are both ruled out (they change the
count, not the outcome). The `CRATONVM_JIT_DENY` arm is recorded because it
made things *worse*, not better — a reminder that a deny arm perturbs
compile/GC timing and is not by itself an exoneration.

Two isolated probes (`MethodTypeDesc.ofDescriptor` in a hot loop over the
exact failing descriptors, 120k iterations; `ClassFile.of().parse()` +
`methodTypeSymbol()` over the real `FlywayAutoConfiguration$FlywayConfiguration.class`,
192k iterations) both PASS on the pre-fix binary. The defect needs many
compiled methods being published and dropped to recycle an address, so a
single-shape hot loop cannot reach it — do not try to reduce this class to a
micro-repro.

**Azure Linux (where the hang was observed), `/data/sbrun.sh`, `--Xmx 4g`,
worktree `/data/data/wt-flywayfix-20260805` @ `2396b3685`:**

```
[1/2] jit FlywayAutoConfigurationTests rc=0 PASS 224s tests=73 failed=0 aborted=0 containersFailed=0
[2/2] jit FlywayAutoConfigurationTests rc=0 PASS 232s tests=73 failed=0 aborted=0 containersFailed=0
[1/2] hotspot FlywayAutoConfigurationTests rc=0      tests=73 failed=0 aborted=0 containersFailed=0
[2/2] hotspot FlywayAutoConfigurationTests rc=0      tests=73 failed=0 aborted=0 containersFailed=0
```

Both craton runs were taken at `load average: 79` on the 16-core host — well
past the contention of the 8-shard × 2-parallel suite that produced the
original HANG — and still finished 68-76s inside the 300s budget.

## The 300s margin is pre-existing, not a residual of this bug

Recorded times for this exact class on the Azure host:

| Run | Result | Seconds |
|---|---|---:|
| `craton-fullsuite-azure-20260802` (pre-regression) | PASS | 220.799 |
| `craton-fullsuite-azure-20260805` (this bug) | **HANG** | 300.136 |
| `hotspot-baseline-latest.tsv` | PASS | 27.780 |
| this fix, 2026-08-05 | PASS | 224 / 232 |

224-232s is statistically identical to the 220.8s the class took when it was
last green, so nothing here regressed its speed. The class simply is an
8x-HotSpot class that spends ~220s of a 300s budget — 73 full Spring
`ApplicationContext` refreshes — and has been for as long as it has been
passing. That standing margin belongs to the suite-wide Spring-bootstrap
throughput work, not to this page; it is called out here so the next timeout
on this class is read as "the margin finally ran out" rather than as a
recurrence of the dispatch bug.

## Also closed by the same fix

`classfile-annotation-metadata-corruption-FIXED-20260805.md` (filed the same
day for `WebMvcAutoConfigurationTests` 88/93 fail,
`WebMvcObservationAutoConfigurationTests` 12/13,
`ServletComponentScanIntegrationTests` 2/3, `WebTestClientAutoConfigurationTests`
3/11) is the same defect seen from the webmvc side. All four classes verified
green on the fixed binary: 93/93, 13/13, 3/3, 11/11.

## Regression guard

`383e7f5cf` ships `a_jit_generation_change_clears_every_site_keyed_memo` in
`vm/src/jit/helpers.rs`, which populates all eight site-keyed memos, forces
the generation to differ, and asserts each is emptied — asserting
non-emptiness first so it cannot pass on maps that were already clear. That
is the mechanism-level guard; the suite-level guard is this class's own
`PASS`/~220s row in the Spring Boot suite results.

## Affected classes

- `module/spring-boot-flyway` — `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests`

Original log:
`craton-fullsuite-azure-20260805-s5/all-jit/logs/module_spring-boot-flyway.org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests.{out,err}.log`

## 2026-08-07: recurred as a TIMEOUT on the Windows full suite — confirmed NOT this bug again

`FlywayAutoConfigurationTests` HANGed again (300.026s) on
`craton-fullsuite-windows-20260806-s3`. Checked before assuming a
regression: `383e7f5cf` is still an ancestor of `dev`
(`git merge-base --is-ancestor` confirms it), and the current `.out.log`
shows none of this bug's exception shapes (`ClassCastException`, `Bad
method descriptor`, the `Class.isAssignableFrom` NPE, etc.) — instead
continuous, unbroken progress through the normal migration-cycle logging
right up to the kill, exactly the "margin ran out" reading this doc's own
"300s margin is pre-existing" section anticipated, now also hitting
`IntegrationAutoConfigurationTests` in the same run for the same reason
(both are many-`ApplicationContext`-refresh classes running under this
run's 16-way parallel host load). Full writeup:
[`docs/known-issues/springboot/flyway-integration-autoconfigurationtests-300s-margin-exhausted-windows-20260807.md`](../../../known-issues/springboot/flyway-integration-autoconfigurationtests-300s-margin-exhausted-windows-20260807.md).
