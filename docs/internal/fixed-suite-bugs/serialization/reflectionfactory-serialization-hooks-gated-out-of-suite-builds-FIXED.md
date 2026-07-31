# FIXED — `ReflectionFactory` serialization hooks were present in `cargo build --workspace` and absent from `cargo build -p cratonvm-cli`

## Status
**FIXED 2026-07-31**, branch `fix/serloader-residual-20260731` (merged to
`dev`). Retired from `docs/known-issues/serialization/`.

Filed as the follow-up residual of
[`bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md`](../h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md)
— the *other* thing the `experimental-serialization` gate turned out to be
hiding. That report deliberately left this alone because flipping it looked like
a behaviour change rather than a link fix. It was. The resolution turned out to
be the third option: **delete it**, on evidence.

## What the defect was
`native-builtins/src/lib.rs`, inside `register_essential_natives_with_shims`
(the **real-JDK** registration path):

```rust
// Real-JDK mode does not call the full synthetic/experimental
// `register_builtins` surface, but JBoss Marshalling calls
// `sun.reflect.ReflectionFactory` directly for serialization hooks.
#[cfg(feature = "experimental-serialization")]
serialization::register_reflection_factory_serialization(registry);
```

That registrar installs 18 overrides across `sun/reflect/ReflectionFactory` and
`jdk/internal/reflect/ReflectionFactory`: `getReflectionFactory`,
`newConstructorForSerialization` (both arities),
`newConstructorForExternalization`, `readObjectForSerialization`,
`readObjectNoDataForSerialization`, `writeObjectForSerialization`,
`readResolveForSerialization`, `writeReplaceForSerialization`,
`hasStaticInitializerForSerialization`.

The comment claims a **real-JDK-mode** role while the line sits behind a feature
that is default-off for both `cratonvm-vm` (`default = ["awt", "management"]`)
and `cratonvm-cli`. So the two builds of the same commit ran different
serialization code:

| invocation | `experimental-serialization` on `cratonvm-native-builtins`? |
| --- | --- |
| `cargo build --workspace` / `cargo test --workspace` (CI's Build & Test job) | **YES** — via `libcratonvm` |
| `cargo build --release -p cratonvm-cli` (every suite runner) | **NO** |

Verified with `cargo tree -e features -i cratonvm-native-builtins` run both
ways. `experimental-aot` leaks identically; `experimental-debug` reaches
`cratonvm-vm` the same way.

`libcratonvm/Cargo.toml` is the sole remaining source: it depends on
`cratonvm-vm` with `experimental-serialization`/`-aot`/`-debug` listed
explicitly, and Cargo unifies features across every member built in one
invocation. (The original report also named `cratonvm-embed`; that was already
stale — its defaults were cleaned up 2026-07-30, `default = []` with the
experiments moved to an opt-in `vm-experimental` group.)

**A smaller invocation reproduces it exactly**, which is the cheapest way to see
the fork without building the whole workspace:

```bash
cargo tree -e features -p cratonvm-cli               -i cratonvm-native-builtins   # feature absent
cargo tree -e features -p cratonvm-cli -p libcratonvm -i cratonvm-native-builtins  # feature present
```

## The history, which inverts the obvious reading
`git log -S` puts the call at `364c469c4d` (2026-07-09, *"Fix WildFly domain
managed-server startup"* — "completing the ReflectionFactory serialization
path, guarding real-layout Constructor access, neutralizing missing ObjectStream
hook MethodHandles"). It was `#[cfg]`-gated **in that same commit**.

So the registration was added to fix a WildFly failure, and was compiled out of
every `-p cratonvm-cli` build from the moment it landed — including the WildFly
suite runner's own build. Whatever fixed WildFly in that commit, it was not this
line. This is the same shape as its sibling bug: *a comment asserting real-JDK
necessity on a line that no real-JDK suite build ever compiled.*

## Why deleted rather than ungated — the evidence
The original report set the decision rule: "Either the JBoss Marshalling case is
real, in which case ungate and re-run the Hibernate/WildFly suites to price the
blast radius; or it is not, in which case delete the call."

Settled with a differential probe rather than by reasoning about the comment.
[`repros/ReflFactoryProbe.java`](repros/ReflFactoryProbe.java) covers the entire
surface those 18 methods touch:

1. **the internal path** — plain `ObjectInputStream.readObject()` on a
   `Serializable` class with *no no-arg constructor* (so deserialization can only
   work through `newConstructorForSerialization`), on one with private
   `writeObject`/`readObject`/`readObjectNoData`/`readResolve`/`writeReplace`
   hooks, and on an `Externalizable`;
2. **the direct path the comment cites** — `sun.reflect.ReflectionFactory`
   (a real class in JDK 25, exported by `jdk.unsupported`) called by hand:
   `newConstructorForSerialization` in both arities, plus
   `newConstructorForExternalization` — the JBoss Marshalling pattern;
3. **the private-hook accessors** — all five `*ForSerialization` MethodHandle
   accessors and `hasStaticInitializerForSerialization`, including a negative
   control (a hook-free class must answer `null`, proving the accessors
   discriminate rather than always answering).

Three arms, 16 assertions each, `--java-home /home/victor/jdk25 --nojit`:

| arm | resolve | overrides | result |
| --- | --- | --- | --- |
| reference | — | HotSpot JDK 25 | — |
| A | `-p cratonvm-cli` (release) | **absent** | **identical to HotSpot** |
| B | `-p cratonvm-cli -p libcratonvm` | **present** | **identical to HotSpot** |

So the overrides are **behaviour-neutral where they were active** and
**unnecessary where they were not**. The real JDK bytecode is correct on this
whole surface, including the cited JBoss Marshalling case.

Given that, deleting the call converges both builds on the JDK's own bytecode.
Ungating would have converged them the other way — replacing working `java.base`
bytecode with Rust across all serialization, which no test asks for and which
the original report warned against. Deleting also cannot regress any suite:
every suite runner already built without these overrides.

## The fix
1. `native-builtins/src/lib.rs` — the `#[cfg]` + call removed from
   `register_essential_natives_with_shims`, replaced by a comment recording why
   this path deliberately does *not* override `ReflectionFactory`, the history
   above, and the instruction that if a JBoss Marshalling case ever does need
   one of these, it must be re-added **ungated and with a failing test**.
2. `register_reflection_factory_serialization` itself is **kept** — the
   synthetic path (`register_serialization_natives`) still calls it, and there
   it is the implementation, since synthetic mode has no JDK bytecode to run.
3. The old `essential_registers_reflection_factory_serialization_hooks` test
   asserted the *presence* of these registrations under
   `#[cfg(feature = "experimental-serialization")]`. That test could only run in
   a resolve no shipping CLI build uses, so it green-lit a registration compiled
   out of every suite binary. Replaced by
   `essential_path_does_not_override_reflection_factory_serialization`, which
   asserts **absence** and carries **no `#[cfg]`** — the assertion has to hold in
   both resolves, which is the whole point.

## Guard against the whole class of bug
The two sibling bugs were both "a `#[cfg]` on a native registration silently
forks CI's binary from every suite runner's binary". That is now a ratchet
rather than a thing to remember:

`vm/tests/t14_system_conformance.rs::t14_gated_registrations_are_declared`
scans every `#[cfg(...)]`-gated native registration in
`native-builtins/src/lib.rs` and fails unless it appears on an explicit
allow-list with a written reason. Gating is not forbidden — synthetic-only
natives are legitimate, since in synthetic mode the Rust code *is* the
implementation — **undeclared** gating is. It is a source-text scan, so it is
feature-independent and cannot be fooled by the resolve it runs under, and it
steps into `#[cfg(...)] { ... }` braced blocks (a gate wrapping a block of
registrations would otherwise read as "not a registration" and be skipped —
exactly the blindness the test exists to remove).

It found one gate on its first run that a hand audit had missed:
`#[cfg(feature = "management")] register_jmx_natives(registry)`. That one is
benign and is now declared as such — `management` is a *default* feature of
`cratonvm-vm`, so it is on in both resolves; it is opt-**out** (for musl/minimal
builds via `--no-default-features`), not opt-in, and so cannot fork CI from a
suite build the way the `experimental-*` features do.

The allow-list is also checked for staleness: if an entry stops matching any
source line the test fails, so a deleted gate cannot leave behind an entry that
would silently bless a future gate of the same shape.

Current declared set (6): `synthetic-jdk` on `System.initPhase1/2/3` and on
`wildfly_naming` (synthetic JNDI — must not shadow real provider selection),
`experimental-serialization` on the synthetic `ObjectInputStream`/
`ObjectOutputStream` surface, `experimental-aot` on the AOT and CDS surfaces,
and `management` on JMX.

## What was NOT changed, and why
`libcratonvm`'s explicit `experimental-*` features stay. Its own comment calls
this out as intentional — "what a distributed binary artifact contains is a
release decision, not a naming cleanup" — and the shipped `.so`/`.dll` has
always carried them. The divergence is therefore accepted rather than removed,
and *tested* instead: CI's `-p`-scoped step now runs the **whole**
`cargo test -p cratonvm-native-builtins --lib` suite rather than a single test
module. The bug was never that one specific test was missing; it was that no
test ran in the suite runners' resolve at all.

## Verification
**Post-removal convergence — the point of the change.** Both binaries rebuilt
from the final source and re-probed:

| binary | resolve | probe vs HotSpot 25 |
| --- | --- | --- |
| `-p cratonvm-cli` release | overrides absent (as before) | **identical** |
| `-p cratonvm-cli -p libcratonvm` | overrides now also absent | **identical** |

So the two builds no longer run different serialization code, and the code they
now share is the JDK's own. `[NativeBridge] unregistered native methods` count is
0 in both.

**The sibling bug's own repros, re-run on the final source** (this branch also
carries the `latestUserDefinedLoader0` ungate, so both fixes are verified
together on one binary):

| suite | classes | result |
| --- | --- | --- |
| H2 | `TestPreparedStatement`, `TestObjectDataType`, `TestSampleApps` | exit 0, 0 `UnsatisfiedLinkError`, stdout **identical** to HotSpot 25 |
| Spring | `MethodMatchersTests`, `AopUtilsTests`, `JdkDynamicProxyTests`, `CglibProxyTests`, `StaticApplicationContextTests`, `TransactionInterceptorTests`, `JCacheJavaConfigTests` | **7/7 `status=OK`**, 0 mentions of the native |

That Spring set is a cross-module sample of the 63-class blast radius, run A/B
against a fix-free control built the same day: **7 FAIL → 7 OK**, 3–5 mentions of
`latestUserDefinedLoader0` per class in the control and 0 in the fixed run. It
independently corroborates the sibling report's full 63-class run.

- `cargo test -p cratonvm-vm --test t14_system_conformance` — 12/12 pass.
- `cargo test -p cratonvm-native-builtins --lib` (default resolve, the one
  suites get) — **3147 passed, 0 failed**.
- `cargo test -p cratonvm-native-builtins -p libcratonvm --lib` (feature-unified
  resolve, the one CI gets) — **3457 passed, 1 failed**:
  `xnio_io_thread::tests::t19_7_c_execute_after_respects_deadline`, which asserts
  a timer fires inside a 450–900 ms window. The host's load average was 76 at
  the time (many concurrent sessions); it passes 3/3 in isolation and has
  nothing to do with serialization. Load flake, not a regression.
- Feature-combination compile matrix (the registrar keeps a caller only on the
  synthetic path, so the removal had to be checked against every combination):
  default, `experimental-serialization` alone, `synthetic-jdk` alone, and both
  together — all clean, plus `clippy -- -D warnings` on the default resolve and
  on the `t14_system_conformance` target.

## Related
- [`bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md`](../h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md)
  — the sibling this was split out of, and where the feature-unification
  analysis was first done.
- One more instance of the same comment/reachability mismatch is already
  recorded in place, in `native-builtins/src/lib.rs` at the
  `serialization::register_byte_array_output_stream(registry)` call: its comment
  claimed it "must always win over the real bytecode" for real-JCA DER output,
  while it sits inside the `synthetic-jdk`-gated `register_synthetic_overrides`
  and so is absent from every real-JDK build. That correction is annotated at
  the call site and left as a separate job — verifying the `DerOutputStream`
  case on the real-JDK path is its own investigation, not this one.
