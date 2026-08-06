# Mockito/ByteBuddy class-metadata-derived bytecode-generation failures — cluster — RESOLVED

**Status: FIXED (2026-08-05).** Not five distinct off-by-something bugs in
CratonVM's class-file bytes or its reflected method/parameter descriptors. All
five are the recycled-`JitInvokeInfo` dispatch aliasing fixed by `383e7f5cf`;
see `flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
for the mechanism and its bisection. Sibling pages retired against the same
commit: `jacksonautoconfigurationtests-disabledcondition-npe-FIXED-20260805.md`,
`rabbitautoconfigurationtests-lambda-nosuchmethoderror-FIXED-20260805.md`,
`tomcatservletwebserverautoconfigurationtests-configurationpropertyname-npe-FIXED-20260805.md`,
and the three other Mockito/ByteBuddy pages retired alongside this one.

## Original symptom (as filed)

Five Spring Boot classes failed, each inside Mockito's `InlineByteBuddyMockMaker`
or `SubclassBytecodeGenerator` while ByteBuddy was generating or retransforming
a mock/proxy class. Each showed a different concrete exception; none reproduced
on the HotSpot baseline:

1. `core/spring-boot` `EnableConfigurationPropertiesRegistrarTests` —
   `java.lang.NoSuchMethodError: java.lang.Integer.storeAt(I)Lnet/bytebuddy/implementation/bytecode/StackManipulation;`
   during `InlineBytecodeGenerator.triggerRetransformation`, followed by
   `retransformClasses0: UnsupportedClassRedefinitionError { ... "new bytes
   failed bytecode verification ... expected Float on stack, found Int" }`.
2. `core/spring-boot-autoconfigure` `CertificateMatcherTests` —
   `java.lang.NullPointerException: Cannot invoke "java.util.List.size()"
   because "this.parameterDescriptions" is null` inside
   `ParameterList$TypeSubstituting.size` → `MethodGraph$Compiler$Default`,
   at `tests=18 failed=0 containersFailed=1`.
3. `core/spring-boot-testcontainers` `ServiceConnectionContextCustomizerTests` —
   `java.lang.IllegalArgumentException: Lookup.defineClass: Linkage(ClassFormatError
   { class_name: "", message: "unexpected end of data at position 40" })`.
4. `loader/spring-boot-loader` `JarUrlConnectionTests` —
   `java.lang.NullPointerException: Cannot invoke
   "net.bytebuddy.implementation.bytecode.StackManipulation.isValid()" because
   "writeAssignment" is null` inside `Advice$OffsetMapping$ForReturnValue.resolve`
   (2/47 tests).
5. `module/spring-boot-grpc-server` `GrpcServerHealthAutoConfigurationTests` —
   `java.lang.ClassCastException: java.lang.Integer cannot be cast to
   net.bytebuddy.implementation.bytecode.StackManipulation` (1/30 tests).

## Root cause

`383e7f5cf` — *a recycled `JitInvokeInfo` address let one call site serve
another's dispatch*.

Every per-thread dispatch memo in `vm/src/jit/helpers.rs` is keyed on
`JitSiteKey = (vm_identity, JitInvokeInfo pointer)`. Those boxes are owned by
`CompiledMethod::_jit_invoke_infos` and are freed when the method drops, after
which the allocator can hand the same address to the next compile. The key then
names a **different call site** while the memos still hold the previous site's
answer, and only two of the eight were revalidated on a JIT generation change.

Two of those memos produce exactly the faces filed above:

* `VIRTUAL_TARGET_CACHE` holds the resolved dispatch **class name**, so the
  reused site resolves its own — correct — method name against the *previous*
  site's class. `java.lang.Integer.storeAt(I)L…StackManipulation;` is that, and
  nothing else: `storeAt` is a real `MethodVariableAccess` method and
  `java.lang.Integer` is the class the aliased site left behind. The
  independently reproduced `NoSuchMethodError: java.lang.Integer.iterator()Ljava/util/Iterator;`
  in `GrpcServerHealthAutoConfigurationTests` (below) is the same shape.
* `NATIVE_SITE_CACHE` holds a resolved leaf-native callback, so the reused site
  **calls the previous site's native and returns whatever that returns** — a
  wrong-typed object (`ClassCastException … Integer`), a `null` where the
  signature promises an object (`parameterDescriptions is null`,
  `writeAssignment is null`), or a short byte array (`ClassFormatError …
  unexpected end of data at position 40`).

The doc's "expected Float on stack, found Int" verifier rejection is downstream
of the same thing: ByteBuddy wrote the class it was able to derive from the
values it was handed, and CratonVM's verifier then — correctly — refused it.

## Why the filed hypothesis was wrong, and why its diagnostic would not have found it

The page proposed that `native-builtins/src/lang_class.rs`'s
`Class.getMethods()`/`getParameters()` had "a sibling gap in how it reports
parameter metadata or method bytecode length/offsets for these five specific
method shapes", by analogy with two already-fixed `lang_class.rs` bugs.

That is wrong in a way worth naming, because the suggested next step — reading
`lang_class.rs` and comparing its parameter metadata against HotSpot's — would
have found nothing and cost a full session. Three tells were already on the page
and each of them rules the hypothesis out:

* **The five shapes have no common data shape.** A metadata gap produces one
  wrong value with one face. A `NoSuchMethodError` for a synthetic ASM-internal
  helper, a null list, a truncated class file and a raw `Integer` are not five
  presentations of one wrong descriptor; they are five call sites that each got
  a value from somewhere else.
* **`java.lang.Integer` is not a value any class-metadata path produces.**
  Nothing in `getMethods()`/`getParameters()` can return a boxed `Integer` for a
  `String` or a `StackManipulation`. Its recurrence across unrelated frames is
  the signature of a *dispatch* fault, not a *data* fault.
* **The failures are not a property of the class being mocked.** Rerunning any
  of the five in isolation on the same pre-fix binary usually passes (see
  Validation) — a metadata gap for a fixed method shape would be deterministic.

## Validation

Fixture `/data/data/springboot-jsonreader-deprecation-20260718` on the Azure
Linux host, one CratonVM process per class via `SbRunner` with the module's own
`build/cratonvm-test-cp.txt` plus the JUnit-platform jars, `--Xmx 4096m`,
`-Dfile.encoding=UTF-8 -Djava.awt.headless=true`.

Two arms, because the defect is **timing-dependent** — whether a dropped
`CompiledMethod`'s info address is re-issued to the next compile depends on JIT
compile/drop timing, which single-class isolation reruns do not reproduce:

**Isolation (one class at a time, one sample each), all 13 classes across the
four retired Mockito/ByteBuddy pages:**

| Binary | Result |
|---|---|
| 08-05 full-suite binary (`1078f6f05c`, no `383e7f5cf`) | 11/13 classes clean — only `ServiceConnectionContextCustomizerTests` and `GrpcServerHealthAutoConfigurationTests` failed |
| dev `2ed2d65d4` (has `383e7f5cf`) | **13/13 clean** |

That 11/13 is the trap: on the pre-fix binary, isolation reruns clear four fifths
of the cluster and read as "not reproducible".

**Concurrent (all 13 classes launched at once, N rounds — the shape the
full-suite shard actually ran):**

| Binary | `383e7f5cf` | Failed class-runs |
|---|---|---|
| `1078f6f05c` — the binary all four pages were filed from | no | **16 / 78** |
| `5ead77835` | yes | **0 / 78** |
| `2ed2d65d4` | yes | **0 / 78** |
| `9e5192b74` (dev tip, also carries `d34bd57ba` + `faa523288`) | yes | **0 / 156**, then **0 / 78** |

**390 class-runs on binaries carrying `383e7f5cf`, 0 failures**, against 16/78
without it. Under the pre-fix per-run rate that is P ≈ 1e-38; even the single
78-run arm alone is P ≈ 1e-8.

`5ead77835` and `2ed2d65d4` still have the native site cache **enabled**, so the
middle two rows measure `383e7f5cf`'s generation-keyed memo invalidation on its
own, not a disable.

One caveat recorded rather than smoothed over: the tip's first 12-round arm
produced one non-result — `BatchJdbcAutoConfigurationTests` round 7 — which is
**not** a failure. Its log shows normal Spring progress lines right up to the
instant my harness's own `timeout 600` fired at 598s, during the one round that
overlapped the HotSpot control run (26 JVMs on 16 cores instead of 13). The
other eleven rounds of that class ran 34/34 in ~140s each. The 6-round rerun
above raised the cap to 900s with nothing else on the box and is 0/78.

**HotSpot control**, same fixture and classpath, `/home/victor/jdk25`: all 13
classes clean, and the per-class discovered-test counts are identical to
CratonVM's fixed arm — 6, 24, 2, 47, 30, 9, 32, 8, 9, 5, 34, 3, 6 — so the fixed
runs are not passing by discovering fewer tests.

**The pre-fix concurrent arm reproduces this page's own faces**, including one
byte-for-byte:

| Class | Reproduced on `1078f6f05c` |
|---|---|
| `CertificateMatcherTests` | `NPE: Cannot invoke "java.util.List.size()" because "this.parameterDescriptions" is null`, at `tests=18 failed=0 containersFailed=1` — the filed counts exactly |
| `EnableConfigurationPropertiesRegistrarTests` | `Could not modify all classes […]` → `IllegalStateException` → `ArrayIndexOutOfBoundsException`; and `NPE: … "candidateAnnotationType" is null` |
| `ServiceConnectionContextCustomizerTests` | `Could not modify all classes […]` → `IllegalStateException` → NPE, 2/2 tests |
| `JarUrlConnectionTests` | 8/47 `MockitoException` → NPE |
| `GrpcServerHealthAutoConfigurationTests` | `NoSuchMethodError: java.lang.Integer.iterator()Ljava/util/Iterator;` |

None of these strings appears anywhere in the fixed arms' stdout or stderr.

## Traps this cluster leaves behind

* **A JIT dispatch-aliasing bug reads as a data bug at the crash site.** Every
  one of the five filed hypotheses named the subsystem whose *value* looked
  wrong. None of them was the subsystem at fault. When an error names a
  `java.lang.Integer` (or any ubiquitous class) as the receiver or the value in
  code that never mentions it, suspect dispatch before suspecting the data.
* **Isolation reruns understate it 4:1.** Reproduce a suite-run failure under
  the suite's own concurrency before concluding it is not reproducible. See
  `negative-control-before-declaring-not-reproducible`.
* **"Different crash sites, same impossible invariant" is one bug, not several.**
  This page and its three siblings split one defect four ways because each
  grouped by *where* it landed. The grouping that would have worked is *what
  kind of wrongness*: a value of the wrong type, from a call that cannot produce
  it.

## Affected classes

- `core/spring-boot` — `org.springframework.boot.context.properties.EnableConfigurationPropertiesRegistrarTests`
- `core/spring-boot-autoconfigure` — `org.springframework.boot.autoconfigure.ssl.CertificateMatcherTests`
- `core/spring-boot-testcontainers` — `org.springframework.boot.testcontainers.service.connection.ServiceConnectionContextCustomizerTests`
- `loader/spring-boot-loader` — `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests`
- `module/spring-boot-grpc-server` — `org.springframework.boot.grpc.server.autoconfigure.health.GrpcServerHealthAutoConfigurationTests`
