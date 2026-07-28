# 2026-07-27 dev regression — 4 residual classes after the JIT repair — OPEN

**Status: OPEN — 4 classes.** Down from ~75. The bulk of the 2026-07-27 `dev`
regression is fixed (see the two commits referenced below); these four are what
survives, and each has a **different** root cause. Recorded here so the next
session starts from measured signatures rather than the class names.

## Context — what was already fixed

A `dev` regression between `60a710ad8` and `7924a4068` broke ~75 Spring Boot
classes. Two causes were found and fixed:

* **Buffer-overflow aborts** — 12 (dev found 13) `try_patch_i32` call sites in
  `jit/src/ir_lower.rs` used `.expect(...)` on a result that legitimately fails
  once `ExecutableBuffer` goes sticky-overflowed, aborting the VM before
  `lower_inner`'s `buf.overflowed()` bail could discard the compile. Landed on
  dev as `d14eb402f` / `d77d29d8a`.
* **A retired-but-load-bearing JIT ban** — `654dfb918` deleted ~189
  `is_known_miscompile` entries as "inert"; the
  `ConcurrentReferenceHashMap` family among them was not. Restored narrowly as
  `SPRING-RT-EQUALS.1` in `vm/src/jit/skip_list.rs`.
  **`SPRING-RT-EQUALS.1` has since been REMOVED (2026-07-28)**: the underlying
  defect was root-caused — every inline-cache guard selected a cached compiled
  callee by the 4-byte `ObjectHeader.class_id` alone, and a reference array
  stores its COMPONENT class id in that word, so a call site warmed on a `Foo`
  receiver dispatched a later `Foo[]` receiver into `Foo`'s own body. Fixed at
  all five guard sites; the witness class `ConditionalOnPropertyTests` is
  38/38 with the ban gone. See
  `docs/internal/resolvabletype-array-receiver-mic-guard-fixed-20260728.md`.

Sweep effect, 583 classes: **484 → 543 PASS, 77 → 28 FAIL, 11 → 0 CRASH.**
Of the 16 classes still regressed at that point, 12 now pass (the narrowed ban
plus ~54 commits of dev drift). These 4 remain.

## The 4 residuals

Measured on `cratonvm-m5.exe` (branch `fix/mockito-fallback-selectors-20260727`
at the merge-5 tree), full JIT, one process per class.

### 1. `NoSpringWebFilterRegistrationBeanTests` — 18 of 19 tests

```
java.lang.IllegalArgumentException: No enum constant TEST_LEVEL_DEFAULT
  org.mockito.quality.Strictness.valueOf(Strictness.java:40)
  org.mockito.internal.configuration.MockAnnotationProcessor.processAnnotationForMock(...:54)
```

Mockito's `MockAnnotationProcessor` does, in effect:

```java
Mock.Strictness s = annotation.strictness();          // nested enum on @Mock
mockSettings.strictness(s == Mock.Strictness.TEST_LEVEL_DEFAULT
        ? null : Strictness.valueOf(s.name()));       // org.mockito.quality.Strictness
```

`org.mockito.quality.Strictness` has no `TEST_LEVEL_DEFAULT` constant — that
value exists only on the nested `Mock.Strictness`. So reaching `valueOf` at all
means the **identity comparison `s == Mock.Strictness.TEST_LEVEL_DEFAULT`
returned false for the TEST_LEVEL_DEFAULT constant**: the annotation proxy
handed back an enum object that is not the canonical constant (a second copy,
or a freshly materialized instance).

**Not JIT** — reproduces identically under `--nojit`.

### 2. `MockWebEnvironmentServletComponentScanIntegrationTests` — 1 of 3

```
java.lang.ClassCastException: java.lang.IllegalStateException
    cannot be cast to [Ljakarta.servlet.DispatcherType;
  org.springframework.boot.web.server.servlet.context.WebFilterHandler.extractDispatcherTypes(...:62)
```

`@WebFilter`'s `dispatcherTypes` member is an **enum array**. The attribute
value came back as an `IllegalStateException` *object* rather than a
`DispatcherType[]` — i.e. an exception escaped into the value slot instead of
being thrown (Spring stores a failed attribute extraction and rethrows later,
so the cast is what surfaces).

**Not JIT** — reproduces identically under `--nojit`.

**Likely shared cause with #1.** Both are annotation element-value
materialization: #1 a scalar enum whose identity is wrong, #2 an enum array
whose value is an exception. `11bc9045d fix(annotations): resolve an annotation
array VALUE component through the declaring loader` is in the regression range
and changes exactly this code
(`native-builtins/src/lang_class.rs`, the array arm of the element-value
builder, plus `resolve_annotation_class_via_loader`). Note that helper invokes
`loader.loadClass` — a Java call that can throw; the `Err(_) => None` fallback
discards the error but it is worth checking whether a **pending exception is
left set** on that path, which would explain an exception object arriving where
a value was expected.

Start here next session: probe `@Mock`'s `strictness()` identity and
`@WebFilter`'s `dispatcherTypes()` against their canonical constants on both
VMs; then bisect `11bc9045d` specifically.

### 3. `SpringApplicationBuilderTests` — 1 of N (`profileAndProperties`)

```
expected: "file"
 but was: "C:\craton\CratonVM"          <- the process working directory
  SpringApplicationBuilderTests.profileAndProperties(...:93)
```

The test writes `application.properties` via `@WithResource` and asserts
`environment.getProperty("c")` is `"file"`. The sibling assertions in the SAME
test pass — `getProperty("a")` (`"default"`, from `.properties(...)`) and
`getProperty("b")` (`"profile-specific-file"`, from
`application-foo.properties`). So both resource files load and profile
resolution works; only `c` is wrong, and it resolves to the CWD.

**Ruled out** (probe `src/EnvProbe.java`, run on both VMs, byte-identical
output): it is NOT the environment (78 entries each, zero names containing
`=`; Windows' hidden `=C:` per-drive pseudo-variables are correctly absent on
both) and NOT system properties (`getProperty("c")` is null at startup on
both, no short-named properties on either). PASSES on real HotSpot under the
same harness, so it is a CratonVM defect, not a cwd-sensitive test.

Next step: dump the resolved `PropertySource` list and find which source
supplies `c`, rather than guessing at the producer.

### 4. `SpringApplicationTests` — HANG

Not yet characterized. Was 13 failures before the JIT repair; now hangs. Needs
`--stack-dump-on-timeout`.

## Reproduce

```bash
powershell -NoProfile -ExecutionPolicy Bypass -File C:\craton\CratonVM\apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -Vm craton -Jit on -Exe <exe> -JdkHome "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot" -ClassList C:\craton\mockfallback-20260727\remaining16.tsv -RunName rem16 -Parallel 4 -TimeoutSec 420
```

## Process note

Both root causes of the parent regression came from commits that verified
**statically**: `654dfb918` retired 189 JIT bans because a gate had drifted
(true) and concluded the list was inert (false for one family); `a50ce9348`
migrated 47 class-lookup call sites behind a `git grep` returning no output
plus 3 unit tests. Neither ran the suite, which takes ~25 minutes and would
have caught both. `11bc9045d`, the leading suspect for residuals #1/#2, is the
same shape.
