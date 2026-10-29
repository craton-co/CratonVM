# i107-L2: aarch64: a field site whose owner is not loaded at compile time bail-lists the method for good

Status: OPEN, performance only, aarch64 only. Interpreter round i5 wave 107, lane L2. Split out of
`interpreter-i92-L2-deferred-field-sites-only-at-the-method-entry-door-FIXED-20261016.md`, whose x86-64 part is
complete (see that page's Wave 107 note). Wave 109: fix option 1 is in code behind
`CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT` (default off). It is not built or run on aarch64 and
needs an aarch64 host to verify. See "Wave 109".

## Symptom

On aarch64 with `CRATONVM_JIT_ARM64=1`, a hot method with a field site (`getstatic`, `putstatic`,
`getfield`, `putfield`) whose owner class is not loaded when the method compiles is never
compiled, not even after the owner loads. Output is correct: the interpreter runs the method,
loads the owner when it first runs the site (JVMS 5.4.3), and prints what HotSpot prints. What
is lost is the compiled tier for that method, for the life of the VM.

## HotSpot behaviour

C1 compiles the site as a patching stub. C2 plants an uncommon trap (`Reason_unloaded`) and
recompiles after the owner loads. Neither leaves the method interpreted for good.

## Where in code

* `jit/src/lib.rs`, the `#[cfg(target_arch = "aarch64")]` block (line ~51374). It sets
  `*backend_attempted = true` (line ~51432) before the field maps are built. A permanent bail
  follows when the backend refuses.
* `jit/src/aarch64_backend.rs`: `static_field_sites` (line ~772) and `instance_field_sites`
  (line ~822) list the sites. The backend refuses any site that is missing from its field maps.
* Since wave 96, `field_access.rs` `field_peek_never_loads_enabled` applies on every target. So
  the compile door's field peek does not load a cold owner, and the row stays absent. This is the
  correct class-load timing.

Only aarch64 builds compile this block. A Windows build or a WSL x86-64 check does not even
type-check an edit here, so it was not attempted blind.

## Probe

`tools/probes/interp/L2/L2W94FieldPeekLoadsNothing.java` checks correctness, which already passes
by design. Run it on an aarch64 host in both modes, with and without `CRATONVM_JIT_ARM64=1`. It
must print HotSpot's lines.

For the gap itself: run `CRATONVM_JIT_ARM64=1 CRATONVM_DBG_JITC=1` on
`tools/probes/interp/L2/L2W93DeferredFieldSiteBench.java` (rows `static-cold`, `field-cold`). Its
hot method never shows a compiled body.

## What a fix needs (either one, on an aarch64 host)

1. **Transient bail (smallest).** Count the field sites first, before the
   `*backend_attempted = true` line. A site counts when its resolver answered `None`, or when
   `resolve_static_base` answered `None` (owner unloaded or uninitialised). If the count is not
   zero, `return None` without setting `backend_attempted`. The hot door then retries, as it does
   for `jitc_bail!("cp_invoke_resolver")`, and compiles once the owner has loaded. Optionally put
   this behind a default-OFF switch `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`.
2. **Deferred site.** Port x86-64's `op_field.rs` `emit_deferred_field_site` to
   `aarch64_backend.rs`. It needs a frame-homed operand block, an oop map for the helper call, and
   the post-call exception check.

To check a fix, run `L2W94FieldPeekLoadsNothing` as above. Then confirm with `CRATONVM_DBG_JITC=1`
that the method compiles once the owner has loaded.

## Wave 109

Fix option 1 (transient bail) is implemented in code behind a new switch, **default OFF**:
`CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`. Interpreter round i5 wave 109, lane L2. It is not built
or run on aarch64: no host here compiles the `#[cfg(target_arch = "aarch64")]` block.

* `jit/src/aarch64_backend.rs` (compiled on every host): `arm64_field_site_transient_enabled()` and
  the pure decision `arm64_first_cold_field_site(code, static_ready, instance_known)`. It returns the
  first `getstatic`/`putstatic` whose static resolver or `resolve_static_base` answers nothing, or the
  first `getfield`/`putfield` whose instance resolver answers nothing. A site that resolves to a row
  the backend refuses (a reference field, the SATB marker tag) is not cold. Its refusal stays
  permanent, because a retry would repeat it. Unit tests: `i109_l2_cold_field_site_tests` (5 tests).
* `jit/src/lib.rs`, the aarch64 block of `try_compile_inner`: with the switch on, the check runs
  before `*backend_attempted = true`. A cold site records bail site `arm64_cold_field_site` (with
  its pc and opcode) and returns `None` without setting `backend_attempted`. This is a transient
  bail, so the hot door retries. With the switch off, nothing changes.
* Cost while the owner stays unloaded: the method is re-attempted at every hot-door trip. This is
  the same cost as the existing `cp_invoke_resolver` transient bail. A static owner that is loaded
  but never initialized keeps the method interpreted, as it does today, but it is retried rather
  than bail-listed.

What the orchestrator must run:

1. Any host (the decision logic):
   `cargo test -j 5 -p cratonvm-jit --lib i109_l2_cold_field_site_tests`
2. Any host, to type-check the aarch64 block. This needs an aarch64 target with a C toolchain for the
   `-sys` crates, which is why WSL/Windows could not do it:
   `cargo check -j 5 -p cratonvm-jit --target aarch64-unknown-linux-gnu`
3. On an aarch64 Linux host:
   * `cargo build --release -j 5 -p cratonvm` (package `vm-cli`, the `cratonvm` launcher).
   * `javac -d . tools/probes/interp/L2/L2W93DeferredFieldSiteBench.java tools/probes/interp/L2/L2W94FieldPeekLoadsNothing.java`
   * Correctness. The output must equal HotSpot's lines in every arm:
     `CRATONVM_JIT_ARM64=1 CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT=1 cratonvm --jdk-only -cp . L2W94FieldPeekLoadsNothing`.
     Repeat with `--compatible`, then with the switch unset.
   * The gap:
     `CRATONVM_JIT_ARM64=1 CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT=1 CRATONVM_DBG_JITC=1 cratonvm --jdk-only -cp . L2W93DeferredFieldSiteBench 2>&1 | grep -E 'arm64_cold_field_site|compile-bail|compiled'`.
     Expected: `resolver-bail site=arm64_cold_field_site(...)` lines for the hot method of rows
     `static-cold` / `field-cold` while the owner is cold, then no `compile-bail ...
     backend_attempted=true` for that method, and a compiled body after the owner loads. With the
     switch unset, the method's `compile-bail ... backend_attempted=true` is printed once and it is
     never compiled.

If (2) or (3) fails to compile, the edit is confined to the `if
aarch64_backend::arm64_field_site_transient_enabled()` block of `jit/src/lib.rs`. Fix option 2 (port
`emit_deferred_field_site`) is still open. It needs the aarch64 helper-call oop map and a
post-call exception check, and cannot be written without a host to run it on.

## Wave 115 (interpreter round i6, lane L2)

OPEN. The wave-109 code was re-read for compile and logic errors (still no aarch64 host); one
small change, and one logic gap that makes the wave-109 fix ineffective for the bench's own shape.

**Compile review** (`jit/src/lib.rs`, the `if aarch64_backend::arm64_field_site_transient_enabled()`
block of `try_compile_inner`'s aarch64 block). By reading, it type-checks:
`cp_static_field_resolver` / `cp_field_resolver` / `direct_helpers` are bound by reference from the
`CompileRequest` destructuring and are `Copy` (`Option<&dyn Fn ..>`, `&&DirectHelperTable`), so the
`.map(..)` calls and the `move` closures copy them and the field-map loops below still use them;
the closure argument tuples match `CompileRequest::cp_static_field_resolver`'s
`Option<(u32, usize, u8, bool)>` and `resolve_static_base(u32, usize)`; `f as &dyn Fn(u16) -> bool`
is an unsizing cast; `crate::note_jit_bail_site_at(&'static str, usize, u8)` and
`crate::x64::dbg_jitc_enabled()` exist on every target (`x64` is not `cfg`-gated). Nothing to fix.

**Logic gap 1 (the fix is bounded by the tier-failure budget).** The transient bail does not set
`backend_attempted`, so the method is not bail-listed -- but the background worker still reports
the compile as unpublished, and `CompilerCore::finish` (`jit/src/tiered.rs`, the `!c.epoch_moved`
arm) charges it to `MethodState::tier_fail_count`; `should_compile` gives up at
`MAX_TIER_FAIL_RETRIES = 3` (`tiered.rs:42`), and nothing resets the count when the owner loads
(only a publish, or `on_class_redefined`). The hot door offers at the threshold and then every
`JIT_RETRY_STRIDE` (64) calls, so a method whose cold owner loads later than about three offers
after warm-up -- `L2W93DeferredFieldSiteBench` `static-cold` / `field-cold`, and
`L2W100DeferredFieldRetierBench`, whose cold call comes after three 200k-call warm rounds -- is
settled for good exactly as before, only three compiles later. The fix works only when the owner
loads within those three offers.

What a fix needs (aarch64 host to verify; not written blind): an unpublished outcome the manager
does not charge but parks, released when a class is defined. Sketch: the worker reads the bail
site (`take_jit_bail_site` == `arm64_cold_field_site`) and completes the task with a new
`Completion::waiting_on_class`; `finish` records `state.waiting_on_class_epoch = <class-definition
epoch at the compile>` instead of charging; `should_compile` declines while the VM's class
definition epoch (the one `dispatch_static::static_owner_epochs` reads) still equals it. Cost
while the owner stays cold: one epoch compare per offer, no compile. The x86-64 tier does not need
this (it defers the site), so it would be dead code there.

**Logic gap 2 (minor).** `arm64_first_cold_field_site` counts every resolver `None` as cold, but
`CpResolvers::static_field` (`jit_bridge.rs`) / the instance resolver also answer `None` for permanent
properties of the site (an `IncompatibleClassChangeError` kind mismatch, an illegal `putstatic` of
a final, a descriptor-tag disagreement). Those used to bail-list at once; with the switch they cost
three failed compiles first. Bounded by the same budget, so no hazard; worth noting if gap 1's park
is built (a parked method with a permanent `None` would wait for every class definition).

**Changed** (`vm/src/runtime/interpreter/jit_bridge.rs`, classification only): `RESOLVER_MISS_BAIL_SITES`
lists `arm64_cold_field_site`, so `bail_reason_is_resolver_miss` (its one consumer is the
optimizing OSR route's `osr_optimizing_refusal_is_resolver_miss`, up to
`OSR_OPTIMIZING_RESOLVER_MISS_RETRIES` = 4 unmemoised declines) calls the aarch64 transient bail
what it is, "waiting on a class". The eager first-call door classifies its misses itself and is
not affected. Whether any aarch64 compile reaches that OSR route is not known here; off aarch64 the
site is recorded nowhere, so nothing changes. Unit test: `i101_l2_resolver_miss_tests` in the same
file gained the `arm64_cold_field_site(pc=9,op=0xb4)` assertion.

Orchestrator: `cargo test -j 5 -p cratonvm-vm --lib i101_l2_resolver_miss_tests` (any host), then the
wave-109 commands above on an aarch64 host.

## Wave 116 (interpreter round i6, lane L2)

SWITCH-OFF. Logic gap 1 from wave 115 is closed in code, behind the existing
`CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT` (default off). The gap was that the transient bail was
charged to `tier_fail_count`, so the method was settled three offers after warm-up. Still not
built for aarch64. The policy half does not depend on the target and is unit-tested on x86-64.

* `jit/src/tiered.rs`, the "waiting on a class" park:
  * `pub fn note_compile_waiting_on_class()` sets a thread-local. A compile calls it when it
    declines because a class is not loaded or not initialized yet. Around each `compile_fn`, the
    worker loop:
    * clears the thread-local before the compile and reads it after;
    * reads the class-definition epoch BEFORE the compile
      (`CompilerCore::class_definition_epoch`). The epoch comes from a source the VM installs
      with the new `TieredCompilationManager::set_class_epoch_source`.
  * `Completion::waiting_on_class: Option<u64>` carries that epoch for an unpublished,
    undeclined completion. `CompilerCore::finish` then PARKS the method
    (`MethodState::waiting_on_class_epoch`) instead of charging `tier_fail_count`. A method can
    park up to `MAX_WAITING_ON_CLASS_PARKS` (64) times; after that it is charged as before.
  * `CompilerCore::admit` (`waiting_on_class_refuses`) refuses a parked method's method-entry
    offers until the epoch moves. Nothing is charged and nothing is queued.
    * Every `WAITING_ON_CLASS_RELEASE_OFFERS`-th (16th) refused offer is let through. This
      covers a static owner that is loaded but only initialized later, which never moves the
      epoch.
    * A publish or a redefinition clears the park. OSR completions never park.
    * Census: `TieredCompilationManager::waiting_on_class_parks()`.
* `jit/src/lib.rs`, the aarch64 transient-bail block: calls
  `crate::tiered::note_compile_waiting_on_class()` next to `note_jit_bail_site_at`.
* `vm/src/vm/vm_init.rs`: installs the epoch source
  (`cratonvm_classloading::store_epochs(shared.jit.class_layout_domain).class_definition_epoch()`).
  This reads this VM's store, not a process global. The source is installed only when this switch
  or the x86-64 one below is on. With both off, nothing parks anywhere.
* x86-64 producer of the same park: `CRATONVM_JIT_BG_RESOLVER_MISS_WAITS_ON_CLASS`. See the
  "Wave 116" section of
  `docs/internal/fixed-bugs/interpreter-i39-L2-proposal-a-compile-request-returns-why-it-produced-no-body-OBSOLETE-20261224.md`.

Unit tests, any host: `cargo test -j 5 -p cratonvm-jit --lib i116_l2_`. The three tests in
`tiered.rs` check that:
* a parked method is not charged and waits for a class definition;
* a parked method is let through on the release stride;
* a method past the bound is charged, no epoch source means no park, and a publish ends the wait.

Probe: `tools/probes/interp/L2/L2W116WaitingOnClass.java`, rows `static-field`, `static-call`,
`new-object`, `checkcast`, with the HotSpot block filled.

On an aarch64 host, run
`CRATONVM_JIT_ARM64=1 CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT=1 CRATONVM_DBG_JITC=1 cratonvm --jdk-only -cp out L2W116WaitingOnClass`,
then the wave-109 `L2W93DeferredFieldSiteBench` command. Expected for the `static-cold` /
`field-cold` hot methods:
* `arm64_cold_field_site` bails;
* no `tier_fail` settle;
* a compiled body after the cold call.

## Wave 117 (interpreter round i6, lane L9 review)

FIXED (one gap in the wave-116 waiting-on-class park). The one-shot C2 retries all went through the
park's `admit` refusal and were lost: the late-bind deferred-field-site re-tier, the deferred-`new`
retry, and the unenterable-OSR promotion. A static owner's `<clinit>` links a deferred site without
moving the class-definition epoch, so those retries were refused exactly when the wait was over.
`request_c2_retry` now ends the park before it admits. See
`interpreter-i117-L9-a-one-shot-c2-retry-is-eaten-by-the-waiting-on-class-park-FIXED-20261019.md`.

## Wave 120 (interpreter round i6, lane L2)

SWITCH-OFF (unchanged; `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`, default off). I re-read the
whole aarch64 path, waves 109 and 116 together, for compile and logic errors. Still not runnable
here.

**Compile.**
* The wave-116 line in the aarch64 block, `crate::tiered::note_compile_waiting_on_class()`, names
  a `pub fn` of the always-compiled `tiered` module.
* The rest of the block is as checked in wave 115:
  * the bindings are `Copy` references;
  * the resolver tuples match `CompileRequest`;
  * `note_jit_bail_site_at` and `x64::dbg_jitc_enabled` exist on every target.

**Logic, end to end.**
1. The block bails with `arm64_cold_field_site` before `backend_attempted` is set, so the method
   is not bail-listed.
2. On a background worker the bail is noted as "waiting on a class". The worker reads the
   class-definition epoch before the compile; `vm_init.rs` installs that source only under this
   switch or `CRATONVM_JIT_BG_RESOLVER_MISS_WAITS_ON_CLASS`.
3. `CompilerCore::finish` parks the method uncharged (at most 64 parks per method).
4. `admit` then refuses the method's offers until a class is defined, letting every 16th offer
   through.
5. A publish, or a redefinition, clears the park. So the method compiles once the owner loads,
   or, for a loaded owner whose `<clinit>` runs later, within 16 offers of the initialization.

**What a resolver `None` means.** It also answers for permanent properties: an ICCE kind mismatch,
an illegal final `putstatic`, a tag disagreement. The x86-64 tier makes the same choice: it
defers every site its field resolution declined
(`CompileRequest::cp_field_deferred_resolver`, a pool-only tag), and the deferred site's helper
raises the right error at run time. So the aarch64 policy is the same as x86-64's, bounded by the
64 parks.

**Not fixable here.** Proposal 2 (porting `emit_deferred_field_site` to `aarch64_backend.rs`)
still needs the aarch64 helper-call oop map and a host. No new defect was found by this reading.

On an aarch64 host:
* run the wave-109 and wave-116 commands;
* run the tiered unit tests on any host: `cargo test -j 5 -p cratonvm-jit --lib i116_l2_`.

## Wave 139 (interpreter round i7, lane L6)

SWITCH-OFF (unchanged; the aarch64 block re-read at `ba7fa53356`, `jit/src/lib.rs` line ~54407,
no change: nothing trivial is left to fix by reading). What is left needs hardware or a cross
toolchain this project does not have today. What each remaining step needs:

* **Type-check only (no aarch64 hardware).** An x86-64 Linux box (WSL Ubuntu is enough) with the
  Rust target and a cross C toolchain for the `-sys` crates, which is what failed before:
  ```text
  rustup target add aarch64-unknown-linux-gnu
  sudo apt install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu
  CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
  CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    cargo check -j 5 -p cratonvm-jit --target aarch64-unknown-linux-gnu
  ```
  This compiles the `#[cfg(target_arch = "aarch64")]` block; it runs nothing.
* **The run (the measurement).** An aarch64 Linux host: an Azure Dpsv5/Epsv5 (Ampere Altra) VM,
  an AWS Graviton instance, or a Linux VM on Apple Silicon (UTM / Parallels, arm64 Ubuntu).
  4 vCPU and 16 GB are enough for `cargo build --release -j 5 -p cratonvm-cli` (the package of the `cratonvm` binary; the wave-109 list says `-p cratonvm`, which is the binary name) with
  `CARGO_PROFILE_RELEASE_LTO=off`. It also needs an aarch64 JDK 25 (Temurin `jdk-25+..-aarch64`)
  as `JAVA_HOME` (CratonVM's class library and `javac`) and as the HotSpot oracle. Then the
  wave-109 and wave-116 commands above, unchanged.
* The decision-logic and park unit tests already run on any host:
  `cargo test -j 5 -p cratonvm-jit --lib i109_l2_cold_field_site_tests` and
  `cargo test -j 5 -p cratonvm-jit --lib i116_l2_`.

Until such a host exists this page stays open with the switch off; the default (switch unset) is
the pre-wave-109 behaviour, which is correct output with a lost compiled tier.

## Wave 144 (interpreter round i7, lane L2)

**OPEN, NEEDS-AARCH64-HOST.** I re-read it at `b3558ac1cf` and nothing changed. The transient bail
(wave 109), the waiting-on-class park (wave 116) and the one-shot retry fix (wave 117) are in code
behind `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT` (default off). What is left is verification,
which no x86-64 host can do: the `#[cfg(target_arch = "aarch64")]` block of `jit/src/lib.rs` is
not even type-checked off aarch64. I did not edit it blind. Option 2 (porting
`emit_deferred_field_site` to `aarch64_backend.rs`) needs the aarch64 helper-call oop map and a
host to run on, so it is not written either.

**Commands on an aarch64 Linux host** (Ampere / Graviton / an arm64 VM, aarch64 JDK 25 as
`JAVA_HOME`, `CARGO_PROFILE_RELEASE_LTO=off`):

```text
cargo test -j 5 -p cratonvm-jit --lib i109_l2_cold_field_site_tests      # any host
cargo test -j 5 -p cratonvm-jit --lib i116_l2_                           # any host
cargo build --release -j 5 -p cratonvm-cli
javac -d out tools/probes/interp/L2/L2W93DeferredFieldSiteBench.java tools/probes/interp/L2/L2W94FieldPeekLoadsNothing.java tools/probes/interp/L2/L2W116WaitingOnClass.java
for m in --jdk-only --compatible; do for s in "" 1; do
  CRATONVM_JIT_ARM64=1 CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT=$s cratonvm $m -cp out L2W94FieldPeekLoadsNothing   # = HotSpot block
  CRATONVM_JIT_ARM64=1 CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT=$s cratonvm $m -cp out L2W116WaitingOnClass         # = HotSpot block
done; done
CRATONVM_JIT_ARM64=1 CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT=1 CRATONVM_DBG_JITC=1 cratonvm --jdk-only -cp out L2W93DeferredFieldSiteBench 2>&1 | grep -E 'arm64_cold_field_site|compile-bail|compiled'
```

Type-check only, on x86-64 Linux with a cross toolchain (no hardware):
```text
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu
CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo check -j 5 -p cratonvm-jit --target aarch64-unknown-linux-gnu
```

Expected HotSpot blocks: the ones recorded in the three probes' headers (MEASURED on x86-64 in
earlier waves; the answers do not depend on the architecture). Expected with the switch on:
`arm64_cold_field_site` bails while the owner is cold, no `compile-bail ...
backend_attempted=true` for the hot method, and a compiled body after the owner loads. With the
switch unset, the method's `compile-bail` line appears once and it is never compiled (correct
output, compiled tier lost).

## Interpreter round i7 (orchestrator, 2026-10-22)

**OPEN, NEEDS-AARCH64-HOST.** The host-independent tests still pass on the round's final tree
(`cargo test -j 5 -p cratonvm-jit --lib i116_l2_`: 3 passed, `c48699ce5e`). This machine has no
aarch64 emulator (no `qemu-aarch64`), no aarch64 Rust target and no aarch64 JDK, so the switch
(`CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`) stays off until a run on an aarch64 host.
