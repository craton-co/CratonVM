# Stub Census

A **stub** here is a registered native whose handler returns a constant — a
named helper (`native_noop`, `native_return_zero`, …) or an inline closure
whose whole body is `Ok(None)` / `Ok(Some(Value::Int(0)))` / etc.

This matters because **a registered native SHADOWS the Java bytecode** for that
class+method+descriptor. A no-op does not "leave the method unimplemented"; it
silently replaces a working method with one that does nothing.

## Enforcement: two gates

| Gate | Scope | Ceiling |
|---|---|---|
| `t9_stub_audit_counts_match_census` | the 6 named helpers, in 12 hand-listed `native-builtins/src` files | per-category, total **53** |
| `t9b_inline_constant_native_census` | **every** `.register*()` call in **every** `.rs` file in the workspace | total **332** |

Both live in `vm/tests/tier1_tests.rs`. Direction for both: counts may only go
DOWN or stay the same. A count going UP means a new stub was added, and the
gate failing is the intended signal to justify it here first.

`t9b` was added in the wave-3 sweep (2026-07-28) because `t9` alone was
measuring roughly an eighth of the problem — see "Why t9b exists" below.

## Current counts (2026-07-28, after wave 4)

Wave 3 triaged the whole surface and justified most of it. Wave 4 changed the
mandate to **implement**, with `KEEP` requiring evidence that the real JDK is
*also* constant there.

| | pre-wave-3 | after wave 3 | **after wave 4** |
|---|---|---|---|
| Constant-valued native registrations | 669 | 462 | **332** |
| — without a justification comment | 243 | 93 | **66** |
| Named-helper subset (the old `t9` number) | 84 | 67 | **53** |

### What the remaining 332 are made of

| shape | count | can it go to zero? |
|---|---|---|
| real methods returning a constant | 228 | partly — see below |
| `registerNatives` / `initIDs` family | 51 | **no**, the no-op IS the correct body |
| spec field constants (`HTTP_OK`, `Types.INTEGER`) | 35 | **no**, the constant IS the value |
| empty constructors | 19 | **no** where the real JDK ctor is empty |

So ~105 of the 332 are correct *by definition* and this number can never reach
zero. Of the 228 methods, a large share are verified against the real JDK body
(`ByteArrayOutputStream.close()` is `{ }`, `SimpleBeanInfo.getIcon()` is
`return null;`, `EmptyEnumeration.hasMoreElements()` is `return false;`,
`DatagramChannelImpl.validOps()` is a fixed 5). **Read the ceiling as "how much
of the native surface is constant-valued", not as "how many bugs are left".**

## A constant is not automatically a defect

A large share of the surface is spec-correct and deliberately kept:

- `registerNatives()V` / `initIDs()V` — HotSpot installs JNI pointers there;
  CratonVM binds in Rust at boot, so there is nothing to do.
- Genuine spec constants: `Types.INTEGER == 4`, `HttpURLConnection.HTTP_OK ==
  200`, `Cipher.ENCRYPT_MODE == 1`, `Collator.PRIMARY == 0`,
  `ZipEntry.DEFLATED == 8`, TLS record sizes 16384 / 16709.
- Methods whose real JDK body IS empty or IS a constant: `StringWriter.flush`,
  `CharArrayWriter.close`, `ByteArrayInputStream.close`,
  `SimpleBeanInfo.getIcon`, `Record.<init>`, `Policy.<init>`,
  `Socket.setPerformancePreferences`, `ConstantBootstraps.nullConstant`.
- Honest "unsupported" answers where the alternative is a lie:
  `CDS.isSharingEnabled0` (CDS is optional; `-Xshare:off` HotSpot agrees),
  `Continuation.yield` returning the spec'd "pinned" result,
  `NetworkInterface.getHardwareAddress` returning null for loopback.

**The gate counts these too.** That is deliberate: "how much of the native
surface is constant-valued" is cheap and reliable to measure, whereas "how much
of it is wrong" is a judgement call that cannot be automated. The ceiling stops
growth; the justification comment at each site records the judgement.

## Deciding what to do with a stub

Two facts decide every case.

**1. Native lookup keys on the DECLARING CLASS of the resolved method**
(`vm/src/vm/vm_exec.rs`; `vm/src/runtime/interpreter.rs`,
`try_stackless_invoke` step 6), and both then apply
`if declaring_is_interface && !is_static && !force_* { no native }`.

- A native on an **interface instance method** does not intercept user
  implementations — only CratonVM synthetic receivers. Exception: it IS
  reachable for a non-SAM method whose descriptor matches `(Liface;)Liface;`
  or `()Liface;` (the `Function.andThen` shape, `interpreter.rs` ~24454).
- A native on an **abstract or concrete CLASS** *does* intercept a subclass
  that doesn't override. This is where the real interception bugs live — wave
  3's widest finding was exactly this shape: constants on
  `org/jboss/logmanager/ExtHandler`, the *base* handler class, meant
  `publish`→no-op and `isLoggable`→false silently dropped all
  jboss-logmanager/WildFly/Quarkus handler output.
- A native on an **abstract method** is dead.

**2. There are two run modes.** Default is real-JDK. `--synthetic-jdk` (a Cargo
feature) loads no real class library — the synthetic natives *are* the class
library. So **deleting a registration fixes real-JDK mode and breaks synthetic
mode.** Default to implementing; delete only when provably dead in both.

Verdicts, in order of preference: **IMPLEMENT** → **THROW** the spec'd
exception → **KEEP + justify** → **DELETE**. Prefer a thrown exception over a
silent no-op whenever a real implementation is out of reach: a caller learning
the truth beats a caller silently getting nothing.

Prefer `null`/throw over a *fabricated plausible value*. Wave 3 removed an
all-zero MAC address from `getHardwareAddress`: `00:00:00:00:00:00` is
syntactically valid, so UUID-v1 generators and cluster-identity code accepted
it and every host derived the same identity, instead of taking their
documented "unavailable" fallback.

### Check for shadowing before you implement anything

Last-registration-wins. Roughly a dozen shadowing conflicts were found across
waves 1–3, several of which had silently nullified an *earlier wave's own fix*.
Before touching a site, grep the whole tree — not just the file, not just
`native-builtins` — for other registrations of the same triple:

    git grep -n '"theMethodName"' -- '*.rs'

A rival implementation can live in another crate under the same function name.
And verify a registrar you suspect is actually *called* (`git grep -c
register_fn_name -- '*.rs'`); a lone hit means it is dead code.

Wave 3's most consequential find was this shape: `jmx.rs` registered no-ops for
`System.loadLibrary` / `System.load` / `Runtime.loadLibrary0` / `Runtime.load0`
that **beat the real loaders**, so no JNI library could be loaded anywhere in
the VM.

Also check **reachability**: a registrar reached only from
`register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]` and does
nothing in the default build. Several wave-3 fixes landed in exactly such a
registrar and had to be duplicated onto the live path to have any effect.

## Why t9b exists

The 2026-07-27 revision of this file reported 67 stubs against a ceiling of 67
and read as near-complete. The real constant-valued surface at that commit was
**669**. `t9` was blind to:

- **Inline constant closures** — the same no-op written as
  `|_ctx, _args| Ok(None)` instead of as a named helper. This was ~86% of the
  surface and the gate never saw one.
- **Every file outside its 12-entry list**, and every crate outside
  `native-builtins` (`native-io`, `native-collections`, `native-awt`, `vm`).

It also **counted a dead file**: `tests_extracted.rs` had no `mod` declaration
anywhere in the tree, so its 5918 lines were never compiled and its 77
registrations never ran. It was padding the census with phantoms that could not
be fixed. Deleted 2026-07-28, along with a stale claim in
`intrinsics/system.rs` that it provided arraycopy test coverage.

If you extend `t9b`, note the two bugs its own development hit, both of which
made it silently *under*-count: rustfmt wraps registrations with a **trailing
comma**, so "text after the last top-level comma" is empty rather than the
handler; and blanking `#[cfg(test)]` bodies needs string-literal-aware brace
matching or it runs past a module's closing brace. A gate that under-counts is
worse than no gate, so cross-check any change against a second implementation.

## When you add a stub

Confirm it is genuinely spec-correct constant behaviour and not deferred work,
write the justification at the registration site, then raise the ceiling here
and in `tier1_tests.rs` in the same change.

## Known-open items (not stubs, but adjacent)

Recorded so the next sweep does not re-derive them. **Resolved in wave 4** and
struck from this list: the missing `is_class_initialized` accessor, the
unreachable finalization count, and the JMX lock-ownership gap (all three are
now implemented and probe-verified against HotSpot 25).

Still open:

- **`System.getLogger` mints an interface-classed object** in real-JDK mode, so
  every subsequent `Logger` call resolves to an abstract method:
  `isLoggable`/`getName` fail and the default `log` bodies hit
  `AbstractMethodError`. This is a registration-structure fix (drop the
  `getLogger` shadow, or mint a concrete synthetic class) — not a constant, and
  changing the `isLoggable` constant would not help.
- **`java/nio/charset/Charset.contains` throws `AbstractMethodError`** in
  real-JDK mode: the receiver resolves to the abstract `Charset` rather than a
  concrete `sun.nio.cs.*`. A class-identity bug.
- **Windows host-interface enumeration** needs `GetAdaptersAddresses`. The
  wave-4 `NetworkInterface` implementation reads `/sys/class/net`, so on
  Windows `getAll()`/MAC/MTU still degrade to loopback-only. Same for the
  Linux-only `TCP_KEEPIDLE`/`TCP_QUICKACK`/`IP_DONTFRAGMENT` socket options.
- **Arbitrary-thread CPU time** (`isThreadCpuTimeSupported`) needs
  `NativeContext::thread_os_tid`; `ThreadRegistry` already publishes `os_tid`.
- **`isCompilationTimeMonitoringSupported`** needs
  `NativeContext::jit_total_compile_time_ms`; `jit/src/tiered.rs` already keeps
  `CompilationStats::total_compile_time_ms` in the spec's own unit.
- **VirtualThreadScheduler counters** need one accessor returning
  `ForkJoinScheduler::{live_carriers, active_count, queued_len}` as a triple,
  so the three cannot be sampled inconsistently.
- **`isObjectMonitorUsageSupported` stays false deliberately.** Owned-monitor
  tracking fires only on the CONTENDED path, so `getLockedMonitors()` would be
  partial; claiming support and returning an incomplete list is worse than
  reporting the feature unsupported. Deadlock detection is unaffected and does
  work. Making it complete means recording every uncontended `monitorenter` —
  a hot-path registry write. HotSpot stack-walks at query time instead.
- **JFR event writing** has no route from `native-builtins` into the VM's
  recorder (the whole surface on `NativeContext` is one hard-coded
  `emit_virtual_thread_pinned_jfr`). `FlightRecorder.isAvailable()` is
  nonetheless correct as `true`.
- **`ResultSetMetaData.getColumnType` reports VARCHAR for every column** —
  both JDBC execute paths build `vec!["TEXT"; n]`. Needs rusqlite's
  `column_decltype` feature.
- **`URLClassLoader.close()`** cannot retract a dynamic-classpath entry:
  `register_dynamic_classpath` is append-only and returns no ids.
- `java/net/http/HttpClient` carriers exist in three incompatible shapes
  (9, 10 and 1 slots) across `net_phase_e.rs`, `http2.rs`, `net_channels.rs`.
- `System.setSecurityManager` now checks `RuntimePermission`, but CratonVM
  still models an installable SecurityManager, which JDK 24 (JEP 486)
  permanently disabled. Adopting JEP 486 would disable CratonVM's own
  `Runtime.exec`/Panama gating — a design decision, not a stub fix.

## A field-table entry that is too short fails SILENTLY

Wave 4 found `com/sun/net/httpserver/HttpExchange` declared 8 slots in
`classloading::synthetic_stub_fields` while `net_phase_e.rs` used
`HEX_NUM_FIELDS = 9` — so every write of the authenticated principal went
nowhere. `java/net/DatagramPacket` and `java/util/prefs/Preferences` had no
entry at all and fell to `_ => vec![]`, making every raw-slot native for them
inert. `set_field` past the end of a short object DROPS the write rather than
erroring, so this class of bug looks like a working implementation. If you add
a slot constant in a native file, add it to the field table in the same change.
