# ElytronRemoteOutboundConnectionTestCase: native SIGSEGV during elytron subsystem / remoting-client test execution

Status: FIXED 2026-07-07 (for the SIGSEGV specifically — see correction below; the A4 register-only-oop attribution was revised, not confirmed) via dev 60079fc4
Severity: High (hard native crash, not a catchable Java exception; blocks the whole test class)
First confirmed: 2026-07-07, Azure worktree `wt-keyfactory-translatekey` (branch `fix/keyfactory-translatekey-20260707`)
Root-caused: 2026-07-07, Azure worktree `wt-elytron-segv-20260707` (branch `fix/elytron-remoting-segv-20260707`)

## Symptom

Running `org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase`
(module `testsuite/integration/manualmode`) under CratonVM (`jit-real` mode) via Maven/Surefire crashes
the forked JVM outright:

```text
[ERROR] Process Exit Code: 139
[ERROR] Crashed tests:
[ERROR] org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase
[ERROR] org.apache.maven.surefire.booter.SurefireBooterForkException: ExecutionException The forked VM
terminated without properly saying goodbye. VM crash or System.exit called?
```

Exit code 139 = SIGSEGV. Confirmed **still reproducing on current dev** (`4e6dc36d`, 2026-07-07) — same
crash point, twice in a row.

## ✅ CORRECTED 2026-07-07 ~16:35 — fixed via `jit_invoke_virtual_mic` arg-decode validation (NOT the general A4 register-oop-bitmap gap)

The crash this doc documents is now fixed on `dev` (`60079fc4`,
"fix(jit): validate heap membership for L/[ arg slots in jit_invoke_virtual_mic").
Root cause, re-derived from a live gdb repro of this exact class's crash and
cross-checked against two other same-day docs that share the byte-identical
`read_string <- native_builder_set <- safe_native_call <- invoke_or_native <-
jit_invoke_virtual_mic` backtrace and the same small-round-number fault
addresses ([[wildfly-infinispan-remove-listener-segfault]],
[[wildfly-xnio-mockselector-mutex-segfault]]):

`jit_invoke_virtual_mic`'s inline `decode_values` closure
(`../../../../vm/src/jit/helpers.rs`) decoded a raw `i64` call-argument slot into an
`ObjectRef` whenever the callee's descriptor said `L`/`[` and the bits merely
LOOKED like a plausible pointer (8-byte aligned, under the 48-bit canonical
ceiling) -- it never checked the bits were an actual live heap address, unlike
its own sibling `decode_dispatch_values` a few hundred lines above, which
already calls `vm.heap.is_object_address()` for the identical decode.
`org.xnio.OptionMap$Builder.set(Option, Object)` is called throughout XNIO
worker/channel setup with numeric options (read/write timeouts, keepalive
intervals, etc.) as boxed `Long`s; whenever one of those primitive longs
reached this decode path unboxed, a round millisecond value like 60000 or
120000 is ALSO 8-byte-aligned and well under 2^48, so it passed the old
"plausible pointer" check trivially -- `ObjectRef::from_raw` fabricated a bogus
reference into unmapped memory, and `native_builder_set`'s `ctx.read_string(s)`
a few instructions later dereferenced its header. SIGSEGV.

**This revises the "A4 register-only-oop family" attribution below.** That
theory required a REAL, previously-valid `String` `ObjectRef` to go stale
because GC relocated/reclaimed it while it sat only in a register, untracked,
across a non-call safepoint. The actual fault addresses tell a simpler story:
`0xea60` (60000) and `0x1d4c0` (120000) are not "small-looking-because-
relocated" addresses -- they are exactly the millisecond timeout CONSTANTS
XNIO passes to `Builder.set`. The value was never a real pointer at any point;
it was a primitive that never went through boxing and got type-confused for a
reference by one weak validation gap. The bisection evidence below
(`--nojit` eliminates the crash; `--no-precise-maps` makes no difference) is
equally consistent with this simpler mechanism -- the fix lives entirely in
JIT-only code that the interpreter's own correctly-tagged argument path never
goes through, and precise vs. conservative stack maps have nothing to do with
a decode-time type-confusion bug. That bisection does not, on its own,
distinguish between the two theories.

**The general A4 register-oop-bitmap gap (`OopMapEntry` has no register-oop
bitmap, `jit/src/lib.rs:52-73`) is very likely still real and still open** --
this fix does not touch `OopMapEntry` or any safepoint/root-scanning code, only
`jit_invoke_virtual_mic`'s own argument decode. [[fork6-fjp-multithread-jit-root-reclamation]]
remains the correct tracker for that broader, still-unimplemented concern; this
doc's specific crash turned out to be a narrower, independently-fixed decode
bug that happened to produce a symptom shape (SIGSEGV in `read_string`,
JIT-only, small fault address) easy to mistake for the bigger gap.

**Verification**: this exact class no longer SIGSEGVs on the fixed binary --
re-run via `run-suite-linux.sh` with `--class-to 300` hit the 300s ceiling as a
`TIMEOUT`, not a `CRASH`/exit-139 (the timeout is consistent with the separate,
already-documented GC-barrier blocked-region-transition boot hang in
[[wildfly-xnio-mockselector-mutex-segfault]]'s "Reproduction attempt" section,
not a new problem from this fix). No kernel segfault was recorded for this run.
A full pass/fail (not just crash-free) confirmation of this specific class is
still worth doing once the GC-barrier hang is separately fixed, but the SIGSEGV
this doc exists to document is gone.

## ADDENDUM 2026-07-07 ~17:00 -- complementary source-level fix: `OptionMap.get(Option)Object` also now boxes correctly

`60079fc4` (above) is a defense-in-depth fix at the JIT argument-decode layer: it stops a leaked raw
primitive from being fabricated into a wild `ObjectRef` (converts it to `null` instead), which is what
stops the SIGSEGV. But it does not fix the actual source of the leaked primitive. Independently (same
day, worktree `wt-a4-register-oop-20260707`, branch `fix/a4-register-oop-bitmap-20260707`), traced the
concrete data flow one level further back using `CRATONVM_DBG_JIT_MIC=1` against the live crash: the
`60000`/`120000` values passed to `Builder.set(Option, Object)` originate from
`org/xnio/OptionMap.get(Lorg/xnio/Option;)Ljava/lang/Object;` (a `get()` call feeding its own result
directly into a `set()` call -- the common "copy an option from one map into another" idiom). Its native
override, `native_option_map_get` (`../../../../native-builtins/src/xnio_async.rs`), stores numeric option values
unboxed internally (`OptionValue::Int`/`Long`/`Bool`) and, for the generic `Object`-returning overloads,
was returning that raw value directly as `Value::Int`/`Value::Long` instead of boxing it -- violating the
method's declared `Ljava/lang/Object;` return type. This is architecturally the SAME missing-boxing bug
`60079fc4` protects against at the decode layer, just caught at its origin instead of its landing site.

**Why both fixes matter**: `60079fc4` alone means `Builder.set(option, workerMap.get(otherOption))`
would no longer crash, but would silently store `null` where a real `60000` belonged -- a correctness
bug, not a crash, and one that could resurface identically for any OTHER native method with the same
"stores primitives unboxed, forgets to box on the generic `Object` accessor" shape. Fixing
`native_option_map_get` at the source makes `OptionMap.get(Option)Object` return a real, correct
`Integer`/`Long`/`Boolean` -- verified to match real HotSpot exactly (`getClass()`/`instanceof`/`equals()`/
the typed-primitive-overload sibling/the default-`null`-when-missing fallback), not just "doesn't crash."
Both changes are complementary and both landed on dev: `60079fc4` as a general JIT-layer safety net for
this whole bug *class*, and the `native_option_map_get` boxing fix for this specific *instance*'s
correctness. See `../../../../native-builtins/src/xnio_async.rs`'s `native_option_map_get` doc comment for the
source-level writeup.

## Root cause (CONFIRMED 2026-07-07): A4 register-only-oop family

**Disambiguation from the concurrent [[wildfly-xnio-mockselector-mutex-segfault]]
investigation**: that doc's crash (unlocking a `std::sync::Mutex<T>` on invalid
memory, in `../../../../native-builtins/src/xnio_io_thread.rs`'s I/O-selector subsystem) is a
**native Rust handle/lifetime bug** (an `Arc`/`Box`-owned native object freed while
another thread still holds a raw pointer/handle to it). This doc's crash (below) is
a **Java-heap GC-root bug**: `NativeContextImpl::read_string` dereferencing a stale
`java/lang/String` `ObjectRef` in `../../../../vm/src/vm/vm_exec.rs`, whose staleness traces to
the JIT not tracking a register-resident oop across a safepoint
(`../../../../vm/src/jit/helpers.rs`, `../../../../jit/src/lib.rs`) — nowhere near `xnio_io_thread.rs` or
`std::sync::Mutex`. Confirmed via live gdb backtrace (see below) that these are two
distinct mechanisms in two different subsystems, not the same root cause wearing
two symptoms, despite both being WildFly-under-CratonVM SIGSEGVs found the same day.


Got a real backtrace by driving Surefire's own `-Djvm=<path ending in bin/java.exe>` property at a
**gdb wrapper script** (Surefire validates the jvm path's parent dir must literally be named `bin` and
the executable `java`/`java.exe`, and the forked process's stdout is consumed by Surefire's own binary
IPC protocol — so gdb's own textual output must be redirected via `set logging file ... redirect on`,
NOT left on stdout, or Surefire never even starts the fork). This pattern (drop-in `-Djvm=` gdb wrapper
+ direct `mvnw -Dtest=... -Djvm=<wrapper>/bin/java.exe test`, bypassing `run-suite-linux.sh` for this one
diagnostic run) is reusable for any future CratonVM-under-Surefire native crash — no core dump needed,
no WildFly-specific setup beyond `CRATONVM_JAVA_HOME` env.

```text
Thread 2 "main-vm" received signal SIGSEGV, Segmentation fault.
0x0000555555adea57 in <cratonvm_vm::vm::vm_exec::NativeContextImpl as cratonvm_native_api::registry::NativeContext>::read_string ()
#1  cratonvm_native_builtins::xnio_async::native_builder_set ()
#2  cratonvm_vm::vm::vm_exec::safe_native_call ()
#3  cratonvm_vm::vm::vm_exec::invoke_or_native ()
#4  cratonvm_vm::jit::helpers::jit_invoke_virtual_mic ()
#5..#16  (unwinder garbage through the JIT-generated call site — no debug/unwind info emitted for JIT'd code)
#17 cratonvm_native_api::native_ring::record_exit ()
#18 cratonvm_vm::vm::vm_exec::safe_native_call ()
#19 cratonvm_vm::runtime::interpreter::invoke_cached_native_callback ()
#20 cratonvm_vm::runtime::interpreter::execute_invokestatic_cached ()
#21 cratonvm_vm::runtime::interpreter::execute_frame ()
#22 cratonvm_vm::runtime::interpreter::execute ()
#23 cratonvm_vm::vm::vm_exec::invoke_on_class_shared_inner ()
#24 cratonvm_vm::vm::vm_exec::invoke_or_native ()
#25 <NativeContextImpl as NativeContext>::invoke_virtual ()
#26 cratonvm_native_builtins::lang_class::native_method_invoke ()
#27 cratonvm_native_builtins::lang_reflect::native_method_invoke_boxed ()
#28 cratonvm_vm::vm::vm_exec::safe_native_call ()
... (frames #29-#115+ repeat the #18-#27 invokevirtual→interpreter→Method.invoke cycle
     roughly a dozen times — a deep recursive reflective-dispatch chain, consistent
     with WildFly's management-operation marshalling)
```

`native_builder_set` (`native-builtins/src/xnio_async.rs:859`, the native override for
`org.xnio.OptionMap$Builder.set(Option, Object)`) does `ctx.read_string(s)` on `args[2]` (the `value`
argument) with **no allocation in between** receiving `args` and the read — so the `ObjectRef` was
*already* a dangling/stale pointer by the time it reached this native. The corruption happened earlier,
somewhere up the deep reflective call chain (frames #29+), most likely while a live `String` reference
was held **only in a register** (not a scanned stack slot) across a GC-triggering safepoint inside that
recursion, then later copied — now stale — into the `jit_invoke_virtual_mic` call-argument buffer at
frame #4.

**Two bisection experiments, both via the same gdb-wrapper repro:**

1. **`CRATONVM_DISABLE_JIT=1` (interpreter-only): crash GONE.** All 22 test methods ran to completion
   (0 crashes; they then failed on an unrelated `java.io.IOException` — a different, non-fatal issue,
   not investigated here). Confirms the bug is JIT-specific.
2. **`CRATONVM_NO_PRECISE_JIT_MAPS=1` with JIT still ON: crash UNCHANGED** (identical `read_string`
   crash site, identical call chain). This **refutes** the natural suspicion that today's
   `f22a8d8c` ("flip precise JIT oop maps back to DEFAULT-ON") caused/exposed this — precise maps
   being on or off makes no difference here. The bug is a **general JIT** gap, not specific to that
   flip; it was simply never reachable under CratonVM before because this WildFly test class never got
   past its `@Before` setup until [[wildfly-keyfactory-translatekey-null-spi]] was fixed today.

**This matches an already-documented, deliberately-deferred gap exactly**: `docs/internal/
gcstress-residual-corruption-faces-FIXED.md` (§"Precise-JIT-oop-maps do NOT fix this residual") and
`docs/known-issues/fork6-fjp-multithread-jit-root-reclamation.md` (the "A4" tracker) both describe —
and this session's own bisection independently re-confirms — that even with precise JIT stack maps on,
**`OopMapEntry` has no register-oop bitmap** (`jit/src/lib.rs:52-73`): a register-only oop is covered
by neither the frame-slot maps nor conservative stack scanning, only by
`emit_pre_safepoint_spill` (which only fires at *call* safepoints). A live oop held only in a register
across some other safepoint (e.g. inside a deep interpreter/reflection recursion, not a JIT call
safepoint) can go stale if GC relocates or reclaims it, and any later use of that register's value is a
dangling-pointer read — exactly what happened here.

**Why this finding matters beyond just this WildFly test**: the existing A4 tracker's repros are all
synthetic, multi-threaded `Fork6Hard ... GC_STRESS=...` runs orchestrating `ForkJoinPool` worker races,
and that doc explicitly says the ForkJoinPool residual "never crashes on current dev" (it usually
surfaces as contained guard warnings or occasional NPEs). **This is a single-process, single-thread-at-
the-crash-point, real production code path (WildFly Elytron + JBoss Remoting + XNIO) that reliably
SIGSEGVs** — a much simpler, more direct, real-world repro of the same underlying gap than orchestrating
GC-stress races. Whoever picks up the A4 register-oop-bitmap work should use this repro to verify the
eventual fix, in addition to the existing Fork6Hard lane.

## Repro

**Fastest (no gdb, just confirms the crash)**:
```bash
cd /data/data/wt-wildfly-bugbash-20260707-runner   # or any checkout of apps/wildfly-suite-runner's Linux driver
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<any cratonvm release binary with the keyfactory-translatekey fix merged (fdfeb8c8+), or later dev>
export JDK25_WIN=/home/victor/jdk25
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --only 'ElytronRemoteOutboundConnectionTestCase' --tag repro
# -> classes: CRASH=1, Process Exit Code: 139 (SIGSEGV)
```

**With a gdb backtrace** (bypasses `run-suite-linux.sh`'s `.exe`-suffix cp-binary wrapper, which can't
host a script; drives Maven directly instead):
```bash
mkdir -p /tmp/gdbwrap/bin
cat > /tmp/gdbwrap/bin/java.exe << 'EOF'
#!/bin/bash
rm -f /tmp/gdb-backtrace.log
exec gdb -q -batch \
  -ex 'set confirm off' -ex 'set pagination off' -ex 'set backtrace limit 300' \
  -ex 'set logging file /tmp/gdb-backtrace.log' -ex 'set logging redirect on' -ex 'set logging enabled on' \
  -ex 'handle SIGSEGV stop print nopass' -ex run \
  -ex 'thread apply all bt full' -ex 'set logging enabled off' -ex quit \
  --args <path-to-cratonvm-binary> "$@"
EOF
chmod +x /tmp/gdbwrap/bin/java.exe
cd /data/data/cratonvm/apps/wildfly/testsuite/integration/manualmode
export CRATONVM_JAVA_HOME=/home/victor/jdk25
rm -rf target/surefire-reports
/data/data/cratonvm/apps/wildfly/mvnw -B -ntp -Dsurefire.default-test.phase=test \
  -Dtest=ElytronRemoteOutboundConnectionTestCase \
  -DfailIfNoTests=false -Dsurefire.failIfNoSpecifiedTests=false \
  -Djvm=/tmp/gdbwrap/bin/java.exe test
# backtrace lands in /tmp/gdb-backtrace.log regardless of Maven's own exit status
```

Both require a binary with the `KeyFactory.translateKey`/`getKeySpec` fix (`fdfeb8c8`+) — against an
unfixed binary the class fails earlier (in `@Before`) and never reaches this crash.

## Evidence

```text
/data/data/wt-wildfly-bugbash-20260707-runner/out/verify-fix-jit-real-all-20260707-053719/logs/00001-*.log
/data/data/wt-wildfly-bugbash-20260707-runner/out/precheck-jit-real-all-20260707-150946/logs/00001-*.log  (re-confirmed on dev@4e6dc36d)
/data/data/cratonvm/apps/wildfly/testsuite/integration/manualmode/target/surefire-reports/*.dumpstream    (trace log ending in "Segmentation fault (core dumped)")
/data/data/scratch-elytron-segv/gdb-backtrace.log   (full gdb backtrace, both JIT+precise-maps-on and JIT+precise-maps-off runs)
```

## Suggested next steps

Not a quick fix — this is the same class of gap `fork6-fjp-multithread-jit-root-reclamation.md`
describes as needing a **register-oop bitmap on `OopMapEntry`** (`jit/src/lib.rs:52-73`), a real JIT
codegen feature addition (tracking which live oops are register-resident, not just frame-slot-resident,
at every safepoint), not attempted in this session — too large/risky to implement blind without the
established JIT-team context on that project. When someone picks up the A4 register-oop-bitmap work,
this doc's repro (single-threaded, deterministic, real production code) is a much cheaper verification
lane than orchestrating `Fork6Hard ... GC_STRESS=...` races.

## Related

Found via [[wildfly-keyfactory-translatekey-null-spi]]'s own fix-verification run — not caused by that
fix, just newly reachable because of it. Root cause is the same family as
[[fork6-fjp-multithread-jit-root-reclamation]] (the canonical A4 register-only-oop tracker) and the
"Precise-JIT-oop-maps do NOT fix this residual" section of `docs/internal/gcstress-residual-corruption-faces-FIXED.md` —
see those docs for the register-oop-bitmap fix design context. NOT caused by `f22a8d8c`'s precise-JIT-
maps default-ON flip (bisected: identical crash with `CRATONVM_NO_PRECISE_JIT_MAPS=1`).
