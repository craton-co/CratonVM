# W7-50 — the `synthetic-jdk` build's strict arm, and the six vectors only it fails

**Status: source landed, UNVERIFIED against a VM.** Nothing below has been
built.

> **RE-VERIFIED IN TREE 2026-08-12.** All three fixes are still present; line
> numbers have drifted and are corrected here:
> `native-builtins/src/lib.rs:9478` and `:9541` (defect A, `OutputStreamWriter`
> / `BufferedInputStream`); `native-io/src/lib.rs:6474-6475` and `:6566-6567`
> (defect A, `InputStreamReader` / `BufferedReader`);
> `native-io/src/lib.rs:6802-6805` (defect B, `register_nio_natives`). Defect C
> is confirmed by *absence*: both
> `register_management_factory_platform_server_stub`
> (`native-builtins/src/jmx.rs:1418`) and `register_mbean_server_factory_synthetic`
> (`:6871`) have **zero** callers in `vm_init.rs` — only the dated tombstones at
> `vm/src/vm/vm_init.rs:2381-2409` and `:2421-2434`, and the latter's sole
> surviving caller is the unit test at `native-builtins/src/jmx.rs:7222`,
> exactly as §7 predicted. The precedent guard cited as
> `native-io/src/lib.rs:5797` is now `:5844`; the
> `set_drop_real_layout_synthetic(true)` pair cited as `vm_init.rs:1659`/`:2192`
> is now `vm/src/vm/vm_init.rs:1918` and `:2482`.
>
> **Baseline drift:** §1's 63/7 and §9's predicted 69/1 are both superseded.
> Both arms now measure **69 passed / 2 failed**
> (`RETIREMENT-20260812B.md:3-5`, `HANDOFF-20260812.md:18-19`) — the §9
> prediction was one vector optimistic. The six vectors themselves are green in
> the default build in both modes, which is §1's load-bearing claim, and it
> held.
>
> **One residual has moved out:** the `bb_state` direct-buffer arm (§5's
> "related-but-untouched half") is **FIXED**, closed by W7-58 —
> `native-io/src/lib.rs:7772`, with a regression test
> `bb_state_resolves_a_direct_buffer_through_address` at `:24058`.
> `HANDOFF-20260812.md:173` records the transfer. Strike it here so it is not
> re-derived. It is the one residual that was reachable from Compatible and so
> could be closed rather than sealed behind the mode gate.
>
> **Still open, verbatim in tree:** the hard-coded `to_be_bytes` endianness
> family (`native-io/src/lib.rs:9456`, `bigEndian` never read); the
> `native_br_read_line` slot-0 `Int` fd read (`native-io/src/lib.rs:2650-2653`);
> the JMX bind-by-name hard-coded return descriptors
> (`native-builtins/src/jmx.rs:6747-6751`); and the `System.initPhase1/2/3`
> override still guarded on the feature while its own comment says "mode"
> (`native-builtins/src/lib.rs:11149`). The baseline measurement in §1 is quoted as measured, on a binary built
`--features synthetic-jdk -p cratonvm-cli` and run `--jdk-only`. Everything
downstream of it — the root causes, the call chains, the arm placements — is
read from source and is stated as such. The one structural claim that was
mechanically checked rather than eyeballed is the brace depth in §5, because
the whole fix turns on it.

## 0. What `synthetic-jdk` is, because the name misleads

> **CORRECTED 2026-08-12.** The paragraph that stood here overshot in both of
> its sentences, and the correction matters because the rest of the record is
> about a mode/feature confusion. `synthetic-jdk` names **two** things:
>
> * a build-time Cargo feature, `--features synthetic-jdk` (CI exercises this
>   at `.github/workflows/ci.yml:447-480` — `cargo check` and `cargo test` only,
>   it never launches the binary); **and**
> * a runtime class-library flag, `--synthetic-jdk`, parsed at
>   `vm-cli/src/main.rs:312` (`#[arg(long = "synthetic-jdk", conflicts_with =
>   "real_jdk")]`), with a second parser for the embedding path at
>   `libcratonvm/src/lib.rs:710`.
>
> So there are **three** runtime selections, not two, and `--jdk-only` is not a
> peer of `--real-jdk`: it is a *compatibility policy* that **implies**
> `JdkMode::Real` (`vm-cli/src/main.rs:3327`, `args.real_jdk || args.jdk_only`).
> Authoritative resolution is `resolve_jdk_mode`, `vm-cli/src/main.rs:2172-2209`.
>
> Defaults: the launcher defaults to **`JdkMode::Real`**
> (`vm/src/config.rs:214`, `LAUNCHER_DEFAULT_JDK_MODE`); the *embedded* default
> is `JdkMode::Synthetic` (`vm/src/config.rs:225`). The two names are coupled
> one way only — `--synthetic-jdk` on a binary built **without** the feature is
> a hard launch error (`vm/src/config.rs:1515`, `require_synthetic_jdk()`,
> gated on `SYNTHETIC_JDK_COMPILED_IN`) — and that asymmetry is precisely where
> this record's five `cfg` defects lived.
>
> The record's *operative* point is unaffected and still correct: **a `#[cfg]`
> cannot see `config.use_synthetic_jdk`**, so a site that needed to ask which
> class library is loaded must not ask what was compiled.

`synthetic-jdk` is a build-time Cargo feature **and** a runtime mode flag of
the same spelling. A binary built with the feature still runs every mode. The
feature adds CratonVM's own synthesised stand-in classes and the natives
written against their layouts.

Every defect in this record is a consequence of that distinction being lost at
a decision point: a site that needed to ask *which class library is loaded*
asked *what was compiled* instead. Five of them asked it with a `cfg`; one
asked it by sitting in the wrong arm of an `if`.

## 1. The corrected baseline

The tracked figure for this arm was **48 passed / 6 failed**. That is **stale**
and should not be worked from. Measured this session, strict corpus,
synthetic-jdk binary, `--jdk-only`:

**63 passed / 7 failed.**

The seven, with their real first failure — not the last line of output, the
first assertion that actually went red:

| vector | first failure |
|---|---|
| `RJdkHello` | `AssertionError: PrintStream reported an error` |
| `RJdkServices` | `AssertionError: discovered providers: []` |
| `RJdkNio` | `AssertionError: little-endian layout` |
| `RJdkNet` | `AssertionError: echo reply: null` |
| `RJdkJmx` | `NoSuchMethodError RJdkJmx$Counter.getAttribute(Ljava/lang/String;)Ljava/lang/Object;` (+2 more, one call site, `registerAndInvoke()V` @pc=129) |
| `RJdkDefineClass` | `internal error: ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef)` |
| `RJdkLogging` | not in scope — the `Formatter.formatMessage` half landed on dev after this measurement; the `inferCaller` half is tracked separately in W7-56-infercaller-strict.md, which fixed it (its predecessor record was retired to retired/jdk-only-jul-logrecord-infercaller-SUPERSEDED-20260812.md) |

**All six in scope pass on the DEFAULT (non-synthetic) binary, in both modes.**
That is the load-bearing fact and it shaped every fix here: the question was
never "what is broken in the VM", it was "what does this feature substitute on
this path, and why is the substitute wrong". Correspondingly, **every change in
this record is convergent by construction** — it makes the feature build's
real-JDK arms register exactly what the default build's real-JDK arms register,
and leaves the synthetic arm bit-identical to what it was.

## 2. Six vectors, three defects

The six are not six problems.

| defect | vectors | mechanism |
|---|---|---|
| **A** — java.io Reader/Writer overrides on real classes | `RJdkHello`, `RJdkNet`, `RJdkServices` | `cfg` used as the mode guard |
| **B** — `register_nio_natives` on real ByteBuffers | `RJdkNio`, `RJdkDefineClass` | `cfg` used as the mode guard |
| **C** — synthetic `MBeanServer` installed in the real-JDK arm | `RJdkJmx` | two calls in the wrong arm |

A and B are the same species and share a precedent; C is a different mechanism
reaching the same shape. The `UnmodifiableMapEntry` refusal that precedes
`RJdkDefineClass`'s failure is **not a defect at all** — see §6.

## 3. The species, and the precedent that already existed

The correct guard was already in the tree, in the same file as defects A and B,
at `native-io/src/lib.rs:5797`:

```rust
#[cfg(feature = "synthetic-jdk")]
if !registry.drops_real_layout_synthetic() {
```

It was written on 2026-08-07 for the `FileOutputStream` block, and its comment
names the species exactly: the cfg "asks what was COMPILED; what decides
whether a real `FileOutputStream` is on the other end is which CLASS LIBRARY
was LOADED, i.e. the launcher flag." That block's symptom then was a
zero-length file from a `write()` that had become a silent no-op.

`set_drop_real_layout_synthetic(true)` is set in `vm_init` at exactly the two
places that matter — `vm/src/vm/vm_init.rs:1659` (feature build, real-JDK arm)
and `:2192` (default build) — and in neither synthetic arm. So the flag means
"a real JDK class library is loaded", in both builds, in both real-JDK modes.

Five sites needed it and did not have it. What made them reachable is that
**both** enclosing registrars run in **both** arms:

* `register_io_natives` — `vm_init.rs:1581` (synthetic) and `:1898` (real-JDK)
* `register_essential_natives_with_shims` — `vm_init.rs:1701` (real-JDK), and
  the synthetic arm reaches it via `register_builtins` →
  `register_essential_natives` → itself

so a `cfg` inside either one is true in a feature build regardless of mode.

## 4. Defect A — the java.io Reader/Writer overrides

Sites, all now carrying the runtime guard:

* `native-io/src/lib.rs:6418` — `InputStreamReader` `<init>` ×4, `read`, `close`
* `native-io/src/lib.rs:6482` — `BufferedReader`, `OutputStreamWriter`,
  `BufferedWriter`
* `native-builtins/src/lib.rs:9222` — a second `OutputStreamWriter` surface

The block comment at `:6478` already said these "corrupt state when invoked on
real JDK instances (BufferedReader: in + cb + nChars + nextChar + ...). Keep
them gated." They were gated — by the wrong thing.

### `RJdkNet` and `RJdkServices` — one line, two vectors

`native_br_read_line` (`native-io/src/lib.rs:2636`) opens:

```rust
let fd = match ctx.get_field(this, 0) {
    Value::Int(fd) => fd as FdId,
    _ => return Ok(Some(Value::Object(None))),
};
```

On a real `java.io.BufferedReader`, slot 0 is `in` — a reference. The match
falls to `_`, and **`readLine()` returns Java `null` on the first call, with no
I/O attempted and nothing thrown.**

* `RJdkNet` reads its echo with `BufferedReader.readLine()` (fixture line 162).
  The connect had already succeeded — the fixture asserts `client.getPort() ==
  port` at line 152 and passes — so this was never a socket defect. The socket
  surface is in fact identical in both builds: every synthetic `java/net/Socket`
  and `java/net/ServerSocket` registrar either early-returns on
  `real_net_sockets` or is dropped centrally at `native-api/src/registry.rs:5884`.
  The `SO_RCVTIMEO` / `SA_RESTART` trap and the lying-wildcard-accessor family
  were both checked and are not implicated: the fixture sets no timeout on this
  socket, and it binds explicitly to loopback rather than the wildcard.
* `RJdkServices` reaches the same `readLine` from **real**
  `ServiceLoader$LazyClassPathLookupIterator.parse`, which is how a
  `META-INF/services` descriptor is read. Zero provider names parsed, no
  `ServiceConfigurationError`, empty list — the fabricated-success shape, but
  produced by a genuine read returning nothing rather than by a stub.

  Worth stating because the obvious lead is wrong: **W6-2 does not explain
  this.** W6-2-module-serviceloader-provider-factory.md fixes the `provider()`
  static-factory form, is deliberately gated on `module_declared`, and its own
  closing section records that `register_service_loader_natives` is tagged
  `NativeKind::SyntheticStub` and is therefore dropped wholesale under
  `--jdk-only`. None of that file runs on this path in either build. Its
  vector `RJdkModule` passes because the mechanism it fixed is a different one.

### `RJdkHello` — the simplest vector, and why it was not simple

`checkError()` is registered **nowhere**, in any build. Neither is
`print([C)V`, nor the 3-arg ctor `(Ljava/io/OutputStream;ZLjava/lang/String;)V`
the fixture uses. All three run real bytecode. So `checkError()` reported the
`trouble` flag faithfully; the VM had set it.

JDK 25's `PrintStream` ctor builds `charOut = new OutputStreamWriter(this,
charset)` and `textOut = new BufferedWriter(charOut)`. The `(OutputStream,
Charset)` descriptor is registered **only** at
`native-builtins/src/lib.rs:9225` — the `native-io` block covers only
`(OutputStream)` and `(OutputStream, String)` — so that one site alone was
enough. `native_output_stream_writer_init` writes the stream to slot 0 and
never creates the `se` StreamEncoder; `native_bw_init` copies slot 0 across and
never sets `out`. Then `ps.print(new char[]{'a','b'})` runs real
`PrintStream.write(char[])` → `textOut.write(buf)` → `ensureOpen()` sees a null
`out` → `IOException("Stream closed")` → caught by `PrintStream`'s own
exception table → `trouble = true`.

The instruction was to find what actually errored rather than clear the flag.
This is it, and the flag is now never set because the real OSW bytecode runs
into the `sun.nio.cs.StreamEncoder` shim in `native-io/src/stream_encoder.rs`
— which is what the default binary does.

The comment on that `native-builtins` site is worth quoting against itself. It
already read: "Real-JDK mode runs the real OSW bytecode → `sun.nio.cs.
StreamEncoder` shim ..., which was validated byte-for-byte against HotSpot's
flush granularity." Everything about the intent was written down correctly. The
guard below it tested the build.

## 5. Defect B — `register_nio_natives`

`native-io/src/lib.rs:6695`, now guarded. The comment at `:6689` said "Gate
them behind the synthetic-jdk feature so real-JDK **mode** uses the JDK's own
bytecode implementations" — mode in the prose, build in the guard.

This registrar covers ~60 methods across `java/nio/ByteBuffer`,
`java/nio/HeapByteBuffer` and `java/nio/Buffer`, and it runs **after**
`register_s2_bytebuffer_essentials`. Registration is last-write-wins, so in a
feature build it won.

* **`RJdkNio`.** The typed accessors (`native_bb_put_int` at `:8612` and its
  family) are hard-coded `to_be_bytes` / `from_be_bytes` and never read
  `bigEndian`; `order` is not re-registered at all. So the order write lands on
  the field, `order()` still *reports* `LITTLE_ENDIAN`, and `putInt` writes
  big-endian anyway. That asymmetry is why big-endian passed and only
  little-endian failed — which is what made the failure look narrow.
* **`RJdkDefineClass`.** `native_bb_remaining` (`:8268`) goes through `bb_state`
  (`:7327`), which resolves the backing array as `hb`-by-name, then slot 5, then
  slot 0 — and has **no direct-buffer arm**. The fixture's `viaDirectBuffer` arm
  calls `ByteBuffer.allocateDirect`, and on the real `DirectByteBuffer` that
  returns, slot 0 is `Buffer.mark`, whose initial value is `-1`. Hence "field 0
  returned Int(-1)". The call comes from the `int len = b.remaining()` that
  opens real `ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain)`.

  So the two vectors **are** one defect, as suspected — but not via a shared
  `ByteBuffer` stand-in. They share a registrar.

`register_nio_natives` saves and restores its own `NativeKind` (`:7664` and
`:8172`), so skipping the call is ambient-neutral for everything registered
after it. This was checked, not assumed.

### The related-but-untouched half

`bb_state`'s missing direct-buffer arm is a **real** defect in shared code, and
it survives this fix — it is simply now unreachable from the real-JDK arm. Its
hardened sibling `bb_storage_view` (`:7377`) already has the arm and a distinct
message. A previous incident, recorded in
spring-bytebuffer-backing-storage-FIXED.md, migrated some call sites and left
roughly 55 behind (`:8130`, `:8155`, `:8273`, `:8282`, and the typed-buffer
family at `:14537`–`:15352`). Completing that migration is a separate, larger
job and is deliberately not attempted here; it cannot affect any of the six.

## 6. `UnmodifiableMapEntry` — the symptom that is not one

`RJdkDefineClass`'s output is preceded by:

```
--jdk-only: refusing to fabricate this bootstrap compatibility class
class="cratonvm/internal/UnmodifiableMapEntry"
```

The brief asked whether this is a genuine compatibility stub or a mis-tagged
`VmInternal` carrier. It is **genuine**, deliberately so, and the refusal is
correct behaviour working as designed. `vm/src/vm/vm_init.rs:1335` argues the
point in place: the eleven `cratonvm/internal/Unmodifiable*` names stand in for
`java.util.Collections$Unmodifiable*`, whose real bytecode is not running, and
"reclassifying them would silence the violation, keep fabricating, and make the
zero-stub census read green while the substitution continued."

Two further facts close it:

1. The mint loop at `vm_init.rs:1239`–`:1357` is **unconditional** — not
   `cfg`-gated, not behind `use_synthetic_jdk`. The default binary emits the
   same eleven warnings under `--jdk-only`, and passes.
2. `RJdkDefineClass` contains no `Map`, no `entrySet`, and no unmodifiable
   collection. The only path that requests this class is
   `native_unmod_entry_itr_next` (`native-collections/src/lib.rs:50075`), which
   the fixture never enters.

It is a `warn!`, boot continues, and it is adjacent noise. Recorded here
because it is exactly the kind of scary-looking line that gets fixed by
accident — reclassifying it to `VmInternal` would have "fixed" nothing, hidden
a real census signal, and left the actual `ByteBuffer` defect in place.

## 7. Defect C — `RJdkJmx`, and two calls in the wrong arm

The lead was that the synthetic MBean machinery binds by name. It does. But
that was the second question; the first is why it was on the path at all.

The feature build and the default build have **separate** real-JDK arms in
`vm_init`, and they had drifted. Both of these sat at brace depth 2 inside the
`else` of `if config.use_synthetic_jdk` — the real-JDK arm — and both are now
removed:

* `register_management_factory_platform_server_stub`. The default build's
  real-JDK arm refuses this exact call under JMX-CLUSTER-20260720, with a
  measured rationale: it "silently zero[ed] out EVERY platform MXBean ...
  confirmed via `MBeanServer.queryNames(null, null)` returning 0 entries". The
  feature build never received that revert.
* `register_mbean_server_factory_synthetic`. Its own comment read "Must NOT be
  called from the real-JDK branch below ... it broke `getPlatformMBeanServer()`
  interface dispatch when it leaked into real mode." "The branch below" was
  read as the `#[cfg(not(feature = "synthetic-jdk"))]` block; the fork that
  decides this is `if config.use_synthetic_jdk`, and the call was in its
  `else`. The comment was right about the rule and wrong about where it stood.

Because the arms are long and similarly indented, the placement was confirmed
by a brace-depth scan rather than by reading indentation — lines 2105, 2122 and
2143 all sat at depth 2, the same depth as the `} else {`.

Removing them costs synthetic mode nothing: `vm_init`'s only two call sites for
these functions were **both** in this arm, so the synthetic arm never received
them. The sole other caller is a unit test at `native-builtins/src/jmx.rs:7172`.

With the stub gone, `ManagementFactory.getPlatformMBeanServer()` runs real
bytecode and builds a genuine `com.sun.jmx.mbeanserver.JmxMBeanServer`, whose
real `StandardMBean` introspection reads the declared management interface.
That is what the default binary does.

### The bind-by-name defect, deliberately left in place

With the synthetic server as receiver, `MBeanServer`'s interface natives answer
instead of real bytecode. `getAttribute` (`native-builtins/src/jmx.rs:6677`)
probes `getAttribute(String)Object`, then builds:

```rust
let cap = capitalize(&attr_name);
for (m, d) in [
    (format!("get{cap}"), "()Ljava/lang/Object;".to_string()),
    (format!("is{cap}"),  "()Z".to_string()),
    (format!("is{cap}"),  "()Ljava/lang/Boolean;".to_string()),
] {
```

The **return descriptor is hard-coded**. `RJdkJmx$Counter.getValue()` returns
`int`, so all three probes missed — and the three misses surfaced as three
`NoSuchMethodError`s naming methods the fixture's own nested class never
declares, from one call site. `getName()` returns `String` and would have
missed identically.

Three more were queued behind it: `setAttribute` (`:6575`) probes
`set<Cap>(Ljava/lang/Object;)V` and **discards the result with `let _ =`**, so
the write silently vanishes; `invoke` (`:6639`) builds an arity-shaped
all-`Object` descriptor and throws away the caller's `String[] signature`; and
`getMBeanInfo` (`:6673`) returns `Ok(Some(Value::Object(None)))` — null — for a
registered bean, doing no introspection at all.

This is **not fixed here**, and the reason is worth stating rather than
leaving as an omission. It is shared code, it is genuinely wrong, and it is the
documented recurring defect — never bind by name. But it is also the *only*
implementation synthetic mode has, this vector cannot measure it, and building
correct `MBeanInfo` / `MBeanAttributeInfo` / `MBeanOperationInfo` is a
substantial new surface. Changing it to fix a vector that no longer touches it
would be speculative churn in code 70 passing vectors also reach.

The right model is already in the tree: `native-builtins/src/phases_late/beans_jndi.rs:2296`
does the `java.beans` Introspector correctly by enumerating
`ctx.declared_methods(cid)` and *parsing* names and descriptors. So does
`mbs_notification_listener` (`jmx.rs:5915`), which asks `ctx.method_exists`
before invoking. The primitive exists; these four sites do not use it.

**Live latent defect, synthetic mode only.** It should be picked up on its own
terms, against a vector that runs in synthetic mode.

## 8. What each change touches

Stated per the constraint that a synthetic-only failure must not be fixed by
changing shared code.

| change | scope |
|---|---|
| `native-io/src/lib.rs:6418`, `:6482`, `:6695` | already inside `#[cfg(feature = "synthetic-jdk")]`; adds the runtime guard. Default build: unchanged (compiled out either way). |
| `native-builtins/src/lib.rs:9222`, `:9278` | `if cfg!(...)` → `cfg!(...) && !registry.drops_real_layout_synthetic()`. Default build: unchanged — the `cfg!` was already false. |
| `vm/src/vm/vm_init.rs` ×2 removals | inside the feature build's real-JDK arm only. Default build: not compiled. Synthetic arm: never received these calls. |

**No shared code paths were modified.** No fixture was weakened. No
`CRATONVM_*` env var was added, so the four-file flag surface is untouched.
`vm/src/vm/tests.rs` was **not** touched.

`native-builtins/src/lib.rs:9278` (the `BufferedInputStream` block) is not
implicated in any of the six. It carries the identical wrong guard six lines
below the `OutputStreamWriter` one and is cited by it as its precedent, so it
was corrected in the same pass — leaving one of two identical wrong guards in
one function is how a family gets re-opened.

## 9. What is not resolved without a build

* **Everything.** No arm of this was run. The expected result is 69/1 — the six
  closing, `RJdkLogging` remaining and owned elsewhere — but that is a
  prediction from source, not a measurement, and it should be re-measured
  before any of it is treated as closed. The 48/6 → 63/7 correction in §1 is
  itself the argument for that: a tracked figure had drifted by fifteen
  vectors.
* **Whether defect A's fix moves the synthetic arm.** It should not — the guard
  is false there — but `shadow_layout.rs:1123`'s `reader_writer_models_match_the_build`
  pins `_vm0` spellings to the cfg, and that test asserts the *model*, not the
  registrations. It should stay green; it was read, not run.
* **Adjacent `cfg`-guarded sites in `native-io/src/lib.rs` that were surveyed
  and left alone**: `:9315`, `:9374`, `:9406` (`StringWriter` / `Writer`),
  `:12651`, `:12676`, `:12711` (`CharArrayWriter`), `:16694`, `:16733`,
  `:21018`, `:21125`. None is on any of the six paths. Whether each is the same
  species or a legitimately build-scoped decision was not determined, and
  guessing would have meant changing registrations no vector measures.
* **The `System.initPhase1/2/3` override** at `native-builtins/src/lib.rs:10889`
  (now `:11149`), whose comment says "only override in synthetic-jdk **mode**"
  while the guard is the feature. Same species by inspection, not implicated in
  any of the six, and boot-path — not a thing to change speculatively.

---

## 10. Where the remaining residuals actually live (2026-08-12)

This section exists because the answer is uncomfortable and easy to state
wrongly in either direction.

### 10.1 "It has never been executed, ever" is **too strong**

That claim is asserted in four places —
`docs/feature-designs/jdk-only-completion-roadmap.md:173`,
`docs/known-issues/jdk-only/README.md:601`, `:606`, and
`W7-18-structured-task-scope-jep505.md:712`. It is **falsified** by a dated
artefact: `apps/h2database-suite-runner/RESULTS-20260721.md:91-95` records a
`--synthetic-jdk` run that "fails immediately on an unrelated gap
(`NoSuchMethodError: java.time.format.DateTimeFormatter.ofPattern`, a missing
synthetic stub hit from `TestBase.<clinit>`)". A `NoSuchMethodError` naming a
*missing synthetic stub* is only producible with the synthetic library loaded,
so a VM did boot in that mode on 2026-07-21.

Four live invocation sites also pass the flag today:
`apps/h2database-suite-runner/run-h2-suite.sh:272` and `:276`,
`apps/hib-suite-runner/run-hib.sh:473`,
`apps/spring-suite-runner/run-suite.sh:347-348`, and
`apps/tomcat-suite-runner/run-tomcat-suite.ps1:338`.

**The defensible restatement**, which is what the roadmap should say:

> The `--features synthetic-jdk` binary W7-50 built has never been run in
> `--synthetic-jdk` mode, and **no `RJdk*` vector has ever been run in that
> mode at all.**

### 10.2 …but the operative consequence is unchanged, and it is structural

Nothing that gates the tree ever launches the mode:

* `regression-suite/run.sh` has **no synthetic arm**. Its only mode knob is
  `CRATONVM_ARGS`, and the only value it recognises is `--jdk-only`
  (`run.sh:203`). Zero `--synthetic-jdk` hits under `regression-suite/` except a
  comment in `src/RDirectBufferElem.java:59`.
* CI's `synthetic-jdk` job (`.github/workflows/ci.yml:447-480`) runs
  `cargo check` / `cargo test` with the feature and **never launches the
  binary** — it exercises the *feature* and never the *mode*.
* `scripts/`, `probes/`, `difftest/`, `test-infra/`, `tools/`, `bench/`: prose
  only, no invocations.

The four runners that *do* pass the flag drive H2 / Hibernate / Spring /
Tomcat, none of which run `RJdk*` vectors — and the last recorded synthetic run
died in `TestBase.<clinit>` before reaching anything this record is about.

### 10.3 Which residuals are unobservable because of it

By construction of W7-50's **own** fix, every surviving residual is now behind
`cfg!(feature = "synthetic-jdk") && !drops_real_layout_synthetic()` — feature
build **and** `JdkMode::Synthetic`:

| residual | file:line | reachable only in |
|---|---|---|
| `to_be_bytes` endianness family | `native-io/src/lib.rs:9456` | runtime `--synthetic-jdk` |
| `native_br_read_line` slot-0 fd | `native-io/src/lib.rs:2650-2653` | runtime `--synthetic-jdk` |
| JMX bind-by-name descriptors | `native-builtins/src/jmx.rs:6747-6751` | runtime `--synthetic-jdk` |
| `System.initPhase1/2/3` override | `native-builtins/src/lib.rs:11149` | runtime `--synthetic-jdk`, boot path |

W7-58 states the same gate independently at
`W7-58-bytebuffer-direct-arm.md:411`: "Every affected registration is inside
`register_nio_natives`, which since W7-50 runs only in a synthetic-jdk build in
synthetic mode."

**So the honest status is: these residuals cannot be observed, confirmed, or
retired by anything currently run in this repo.** They are not "probably fine"
and not "probably broken" — they are *unmeasured*, and the fix that made the
feature build's real-JDK arm converge on the default build's is what pushed
them there. Retiring any of them requires a `--features synthetic-jdk` binary
launched with `--synthetic-jdk`, which is P4-B's job.
