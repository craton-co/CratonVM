# WildFly domain boot: Host Controller SIGSEGV in a cached-invoke dispatch stub, right after `host=foo:add()` — respawns forever, gates the four front-line residuals

Status: OPEN — crash site conclusively identified (JIT inline PIC cascade inside a compiled ReentrantLock.lock() method, high confidence but not 100% proven); a specific, named root-cause candidate found (stale synthetic field-layout padding for ReentrantLock in classloading/src/class_manager.rs) but NOT YET CONFIRMED LIVE OR FIXED — see 2026-07-10 (new session) update at the bottom
Severity: High — this is now the gating blocker for `docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md`'s four front-line residuals (`AttributeChangeNotification`, `ContentCleanerService`, `FileInputStream(File)`, `WFLYHC0034`), which cannot be re-observed until this is fixed
First confirmed: 2026-07-10, on Azure host `victor@20.83.144.174`, branch `fix/wildfly-residuals-20260710`

## Context

Picked up `wildfly-domain-heap-corrupt-value-timeout.md`'s four front-line residuals now that
`docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md` (the prior gating
regression) is fixed on `dev`. A pristine WildFly 32.0.1.Final `bin/domain.sh` boot
(`CRATONVM_MSC_REAL_START=1`) got past the process-controller/Host-Controller handshake cleanly
(confirming the `ThreadPoolExecutor` fix), then hit a NEW blocker before this doc, fixed the same
session (see `java-lang-ref-cleaner-static-native-half-initialized-object-FIXED.md`), then hit
THIS crash — a genuine SIGSEGV, not a Java exception.

## Symptom

`bin/domain.sh` boots, `WFLYSRV0049 WildFly Full 32.0.1.Final ... starting` prints, extensions
parse, WildFly Elytron initializes, then `DEBUG [org.jboss.as.host.controller] Invoking the
initial host=foo:add() op` — and the Host Controller process dies **silently** (no Java exception,
no `FATAL`/`WFLYSRV0239` line, no `[cratonvm] System.exit(...) called` line — it just stops
producing output). `INFO WFLYPC0011: Process 'Host Controller' finished with an exit status of %d`
(the `%d` itself never gets substituted — a separate, pre-existing, apparently-cosmetic
message-formatting gap, seen on many other WildFly log lines too, e.g. `WFLYPC0021: Waiting %d
seconds...`; not investigated further here) followed by `WFLYPC0021: Waiting %d seconds until
trying to restart process 1.`, then process-controller respawns Host Controller, which repeats the
exact same sequence and dies again — forever, until the overall boot times out.

**Confirmed a real SIGSEGV, not a hang or clean exit**, via `strace -f -e trace=exit_group`
wrapping the whole `domain.sh` process tree: two `--- SIGSEGV {si_signo=SIGSEGV,
si_code=SEGV_MAPERR, si_addr=NULL} ---` events appear, each immediately preceding a Host Controller
respawn cycle.

**Confirmed independent of the JIT**: identical respawn loop with `CRATONVM_DISABLE_JIT=1`.

## Live-gdb root cause (partial)

Because `ptrace_scope=1` on this host normally blocks attaching to a non-child process, and there
is no core-dump path configured (`ulimit -c` is 0, `core_pattern` routes to apport), catching this
live required `sudo sh -c 'echo 0 > /proc/sys/kernel/yama/ptrace_scope'` (restored to `1` afterward) plus
a polling script that `pgrep -f org.jboss.as.host-controller` in a tight loop and races to
`gdb -batch -ex "handle SIGSEGV stop print nopass" -ex continue -ex bt -ex "x/8i $pc" -p $pid`
against every newly-spawned Host Controller process (multiple respawns per boot = multiple chances
to win the race). Caught cleanly on the first or second attempt, both with JIT on and with
`CRATONVM_DISABLE_JIT=1`.

**Crash site** (identical relative instruction sequence across every capture, both JIT-on and
JIT-off, only the ASLR base differs):

```
=> mov    (%rsi),%eax          ; <-- faults here: %rsi is NULL
   cmp    (%r10),%eax
   jne    <miss-path>
   cmpb   $0x0,0x28(%r10)
   je     <slow-path>
   mov    0x10(%r10),%r11
   call   *%r11
```

This is a classic **monomorphic inline-cache dispatch stub**: load something at offset 0 of the
receiver (`%rsi`) — almost certainly a `class_id`/discriminant read — compare it against a cached
expected value (`%r10`), take a miss/slow path on mismatch, otherwise check a flag byte at `+0x28`
and (if clear) load a function pointer from `+0x10` and call through it. `%rsi` is `NULL` at the
fault (matches the reported `si_addr=NULL`), i.e. **a null/zeroed receiver reached a cached
call-site whose fast path never null-checks it**.

`thread apply all bt` cannot walk past this frame — gdb reports every enclosing frame as `?? ()`
with clearly-bogus "return addresses". This is not stack corruption in the usual sense: the
values gdb misreads as return addresses are recognizable as **CratonVM's own tagged `Value`
words** (many share the `0x0000'02......` high-bit pattern the VM's `Value` encoding uses for one
of its variants — visible directly by comparing two independent captures: frame #1 was the
*exact same* `0x000002000d12d010` in both). In other words, gdb is walking the *Java operand
stack*, not the native C call stack, once it loses the frame-pointer chain at the crash site — a
strong hint the crash is inside a hand-written / codegen'd dispatch helper that does not follow
normal Rust calling-convention frame-pointer conventions (consistent with either JIT-generated
code or a `#[naked]`/raw-asm-adjacent fast path in the interpreter's cached-invoke machinery).

One capture's corrupted backtrace happened to include exactly one resolved Rust symbol several
frames up: `gc_alloc_object () at vm/src/runtime/interpreter.rs:1674` — suggesting the crash
follows shortly after a GC allocation, though the intervening frames could not be reliably
correlated (the unwinder was already lost by that point).

## Working hypothesis (not confirmed)

The instruction pattern matches the monomorphic invoke-cache fast path described in
`vm/src/runtime/interpreter.rs` (`execute_invokevirtual_cached` / `CachedInvokeTarget::VirtualNative`
et al. — see the receiver-class-id-check-then-call-through-fn-pointer pattern in that function's own
doc comments) — the SAME family of dispatch machinery investigated at length while fixing
`threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md` earlier this session (that investigation
found and mapped at least 5-6 independent "native always wins" dispatch call sites in this same
area). If this cached fast path can be reached with a genuinely null/stale receiver (e.g. a cache
entry populated for one call site then reused after the receiver was GC'd, moved, or was never
properly initialized), that would explain both the `SEGV_MAPERR`-on-NULL and the `gc_alloc_object`
breadcrumb nearby. **Not confirmed** — the exact Rust source line was not identified this session
(ran out of investigation budget after the disassembly-level pin-down above).

## Reproduction

```bash
# Pristine WildFly 32.0.1.Final distribution (re-extract from a stored .zip each time --
# a previously-booted copy accumulates .bak/mutated config and is not a clean baseline).
cd <fresh-wildfly-extract>
export JAVA_HOME=<dir-with-bin/java-symlinked-to-cratonvm>
export CRATONVM_JAVA_HOME=<real-JDK25-home>
export CRATONVM_MSC_REAL_START=1
bin/domain.sh
# Boots, parses extensions/Elytron, then "Invoking the initial host=foo:add() op",
# then Host Controller dies silently and respawns forever.
```

Live-catch recipe (needs `sudo sh -c "echo 0 > /proc/sys/kernel/yama/ptrace_scope"` first, restore
to `1` after):

```bash
while true; do
  for pid in $(pgrep -f "org.jboss.as.host-controller"); do
    timeout 10 gdb -batch -ex "handle SIGSEGV stop print nopass" -ex continue \
      -ex bt -ex "x/8i \$pc" -p "$pid" >> gdb-catch.log 2>&1 &
  done
  sleep 0.05
done
```

## Recommended next steps

1. Identify the exact Rust source for the crash-site instruction sequence — likely
   `execute_invokevirtual_cached`'s `CachedInvokeTarget::VirtualNative`/`Intrinsic` match arms in
   `vm/src/runtime/interpreter.rs`, or a JIT-emitted equivalent. Add `info proc mappings` to the
   live-gdb recipe above to get the loaded base address, subtract from the captured `$pc` to get a
   load-address-independent offset, then `addr2line -e <the frozen cratonvm binary> -f -C <offset>`
   (works even across ASLR since the *offset* into the ELF text is what's constant — confirmed
   constant across two independent captures in this session, both ending in `...12d`).
2. Once the exact call site is found, determine whether the receiver truly is null (a genuine bug
   upstream feeding a null/uninitialized object into a virtual dispatch during host-controller
   boot -- likely worth chasing with `CRATONVM_DBG_UNCAUGHT=1`-style tracing one level up, or a
   conditional breakpoint on the fast-path entry checking for a null receiver before it reaches
   this specific stub) or a cache-staleness bug (the cached target was populated for a different,
   now-collected/moved object -- check whether this fast path's cache invalidation accounts for
   GC compaction/moves, matching the "BUG-03 cross-thread JIT root scan" family of prior fixes).
3. Once fixed, re-run this doc's reproduction recipe, then return to
   `wildfly-domain-heap-corrupt-value-timeout.md` to re-check for the four front-line residuals,
   which this crash currently gates.

## 2026-07-10 update (new session) — crash site conclusively identified as JIT-compiled `ReentrantLock.lock()`; root cause narrowed to a stale synthetic-stub field-layout entry, not yet fixed

Picked this up after the host suffered an unrelated ~45-minute outage (reboot) mid-session;
resumed on a fresh `origin/dev` in a new worktree, `/data/data/wt-wildfly-hc-sigsegv-20260710-203435`
(branch `fix/wildfly-hc-sigsegv-20260710-203435`), binary frozen at
`/data/data/bin-cratonvm-hc-sigsegv-20260710-215120` (plain repro) and a series of
`/data/data/bin-cratonvm-hc-sigsegv-dbg*-20260710-222500` (temporary debug builds, since reverted —
worktree is currently clean, no uncommitted diagnostic code remains).

### Confirmed: the crash site is exactly `JitPICSlot` slot-0 dispatch (byte-for-byte)

A full, wide (`x/500i $pc-0x120`) live-gdb disassembly of the compiled method containing the
crash — captured from `info proc mappings` first to confirm the fault PC falls inside an
**anonymous `r-xp` mapping** (a JIT code-cache arena, not the main ELF binary) — shows the
7-instruction cascade the earlier session's disassembly excerpt hinted at is the true entry
point of the compiled method, not a random mid-method offset. The offsets match
`JitPICSlot::CLASS_ID_OFFSETS=[0,4,8]` / `ENTRY_PTR_OFFSETS=[16,24,32]` /
`NEEDS_CONTEXT_OFFSETS=[40,41,42]` (`jit/src/lib.rs:3351+`) exactly, confirming (as the prior
session suspected) this is `jit/src/x64.rs`'s inline 3-way PIC cascade
(~lines 23540-23780), reached via the **first-call-compile / on-demand-compile** path
(`jit::try_compile` in `jit/src/lib.rs`, confirmed via targeted temporary instrumentation —
see below), not OSR and not the background tiered-compile queue.

### The NPE/null guard is present, correctly encoded, and correctly NOT the bug

Hand-verified byte-for-byte against `jit/src/x64.rs:23760-23789`'s `TEST recv_reg,recv_reg; JZ
.miss` guard (added 2026-05-20, commit `a615bad8`, for an unrelated already-fixed Tomcat
`Locale.hashCode` null-receiver bug) — the encoding is correct for this register/ABI, and it
**does not fire** because the receiver register is **not actually zero**. Register captures
across multiple independent crashes show the receiver (RSI on this SysV build) consistently
holds a **small, 8-byte-aligned, well-under-47-bit value** (e.g. `0x22841af8`, `0x226814b0`) —
distinct between runs but consistently "small-integer-shaped", nothing like the VM's real heap
arena addresses (which are consistently `0x2000_xxxx_xxxx`+ in every other register in the same
captures). This value passes `plausible_heap_pointer`'s bit-pattern check (non-null, aligned,
<2^47) — **so a plausibility-only guard, like the one already used elsewhere in this codebase for
stale-GC-reference protection, would NOT have caught this value.** This is not a stale/GC'd
reference degrading through an otherwise-sound pointer-shaped value; it looks like a genuine
non-pointer scalar landing in a pointer slot.

### Traced to source: the receiver is fed by `jit_getfield`'s raw return value, unconditionally

Widening the disassembly window to the full compiled method (from its very first instruction)
and resolving the two `CALL <imm64>` targets via live `info symbol *(void**)(...)` shows the
compiled method's body is:

```
jit_frame_record(rbp)                     ; GC precise-frame prologue hook, unrelated
<reload this, from the method's own first param>
<null + alignment + heap-region-bounds-table check on `this`>   ; validates getfield's receiver
CALL jit_getfield(vm_ptr, this, field_index=0)   ; edx=0 (xor edx,edx) — always field #0
<compare result against the i64::MIN deopt sentinel; bail to deopt-exit if equal>
<store the raw i64 result to [rbp-0x28] and treat it as the receiver>
<PIC-cascade dispatch on that value — CRASHES here>
```

`jit_getfield` (`vm/src/jit/helpers.rs:2673`) returns the field's **raw i64 bits with no type
tag** — `Value::Int(i) => i as i64`, `Value::Object(Some(r)) => r.as_ptr() as i64`, etc. are all
indistinguishable in the return value. The calling codegen (confirmed at
`jit/src/x64.rs:19760-19790`, the "compact receiver → helper" arm of the getfield-inlining
logic) does the right thing on its OWN terms — it `push_from_rax()`s the result onto the normal
operand stack and calls `mark_top_as_oop()` **only if `c_is_ref` (the statically-resolved field
type tag) says the field is reference-typed** — so the getfield codegen itself is not blindly
treating every field as a pointer. The bug is not a fusion/peephole shortcut; getfield and the
later invokevirtual are ordinary, independently-correct operand-stack producer/consumer. **The
field being read genuinely is supposed to be reference-typed** (its declaring bytecode's own
`invokevirtual` on the popped value proves the verifier accepted it as a reference at
compile time) **but the runtime object's field #0 slot does not actually hold that reference.**

### Crashing method identified: `java.util.concurrent.locks.ReentrantLock.lock()`

Live-gdb alone cannot show the Java class/method identity of JIT-compiled code (no debug info
for the JIT's own machine code). Correlating `jit::try_compile`'s existing call graph (not the
crash site itself) via a **temporary** (since reverted) unconditional debug print at the top of
`jit::try_compile` (`jit/src/lib.rs:4978`) — this fires for every method entering the compiler,
tagged with the real class/method/descriptor from `CachedBytecodeMethod` — captured across
*multiple independent domain.sh boot-and-crash cycles* the **exact same terminal sequence**
immediately before every `WFLYPC0011` (Host Controller exit) event:

```
try_compile class=org/jboss/dmr/ModelValue method=copy desc=()Lorg/jboss/dmr/ModelValue;
try_compile class=org/jboss/dmr/ModelValue method=copy desc=()Lorg/jboss/dmr/ModelValue;
try_compile class=java/util/EnumSet method=of desc=(Ljava/lang/Enum;)Ljava/util/EnumSet;
try_compile class=java/util/EnumSet method=typeCheck desc=(Ljava/lang/Enum;)V
try_compile class=java/util/concurrent/locks/AbstractOwnableSynchronizer method=getExclusiveOwnerThread desc=()Ljava/lang/Thread;
try_compile class=java/util/concurrent/locks/AbstractQueuedSynchronizer method=getState desc=()I
try_compile class=java/util/concurrent/locks/AbstractQueuedSynchronizer method=setState desc=(I)V
try_compile class=java/util/concurrent/locks/ReentrantLock$Sync method=lock desc=()V
try_compile class=java/util/concurrent/locks/ReentrantLock$NonfairSync method=initialTryLock desc=()Z
try_compile class=java/util/concurrent/locks/AbstractQueuedSynchronizer method=compareAndSetState desc=(II)Z
try_compile class=java/util/concurrent/locks/ReentrantLock$Sync method=tryRelease desc=(I)Z
try_compile class=java/util/concurrent/locks/AbstractQueuedSynchronizer method=getState desc=()I
try_compile class=java/util/concurrent/locks/AbstractOwnableSynchronizer method=getExclusiveOwnerThread desc=()Ljava/lang/Thread;
try_compile class=java/util/concurrent/locks/AbstractQueuedSynchronizer method=setState desc=(I)V
try_compile class=java/util/concurrent/locks/AbstractOwnableSynchronizer method=setExclusiveOwnerThread desc=(Ljava/lang/Thread;)V
try_compile class=java/util/concurrent/locks/ReentrantLock method=lock desc=()V
--- (no further try_compile lines; WFLYPC0011 next) ---
```

`ReentrantLock.lock()`'s real bytecode is `this.sync.lock()` — a single `getfield sync
(index 0, the class's only instance field); invokevirtual` — an exact structural match for the
crash-site pattern above (field_index=0, deopt-sentinel-checked getfield result fed straight to
an invokevirtual receiver). `Sync.lock()` is abstract (implemented by `NonfairSync`/`FairSync`),
so this genuinely is a virtual dispatch needing the PIC cascade — this is architecturally
consistent end to end, not a coincidental match. **Confidence this is the crashing method is
high** (identical terminal sequence across every independent capture, structurally exact match,
right at the point every capture stops) but **not proven with 100% certainty** — the debug print
correlates compile *order*, not the crashing call frame directly; a live capture that resolves
`jit_getfield`'s `vm_ptr`/`this` arguments back to a `ClassId`/name would be the fully conclusive
next step and was not completed this session (ran out of budget after the rebuild/verify cycle
below).

### Leading root-cause hypothesis (NOT fixed, NOT confirmed): stale synthetic field-layout padding for `ReentrantLock`

`classloading/src/class_manager.rs:7538-7539`:

```rust
// ReentrantLock: 3 fields (owner=0, holdCount=1, fair=2)
"java/util/concurrent/locks/ReentrantLock" => instance_fields(3),
```

This is `synthetic_stub_fields`'s entry for `ReentrantLock` — a **legacy 3-field layout**
(`owner`/`holdCount`/`fair`) matching the **old**, pre-"real-AQS" synthetic `ReentrantLock`
implementation (`native_rl_*` in `native-builtins/src/lib.rs`, now gated OFF by default per
`register_concurrent_natives`'s own comment: *"Real AQS is now the DEFAULT... Opt OUT with
CRATONVM_SYNTHETIC_AQS=1"*). The real `java.util.concurrent.locks.ReentrantLock` class has
exactly **one** instance field (`sync`), not three.

Critically, this stale entry is not dead: `class_manager.rs:5779-5793` (the real-classfile
loading path, comment "Wave 3-B (RE.4)") **unconditionally consults `synthetic_stub_fields`
even when loading a class from its real `.class` bytecode**, and pads the computed field count
up to the stub's:

```rust
let stub_fields_for_pad = synthetic_stub_fields(name);
let stub_instance_count_for_pad = stub_fields_for_pad.iter()
    .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC)).count();
...
let num_total_fields = num_total_fields.max(stub_total_for_pad);
```

For `ReentrantLock` this pads the real class's field count from **1 up to 3**. The comment
explains the *intent* ("classes that are upgraded from a synthetic stub but whose real bytecode
field count is smaller than the synthetic-mode layout used by native helpers") — a legitimate
concern in general — but for `ReentrantLock` specifically, the synthetic layout it pads against
is **stale**: it describes a native-helper implementation that is default-OFF, not the
implementation actually in use. Extra *trailing* padding slots (indices 1-2) are inert on their
own and would not by themselves corrupt field 0 (`sync`). Whether — and exactly how — this
padding, or a related by-name field lookup elsewhere still keyed to the old
`owner`/`holdCount`/`fair` names, actually corrupts field index 0 specifically (as opposed to
merely wasting two harmless trailing slots) was **not conclusively traced this session**; it is
the strongest concrete, named lead uncovered so far, not a confirmed fix. A live
`CRATONVM_DBG_MCL=1` (classloader) trace correlated with a breakpoint on this class's `<init>`
and `lock()` field-index resolution, to see the actual `num_total_fields` and field-index-0
resolution CratonVM computes for a live `ReentrantLock` instance during this exact boot, is the
concrete next step — not attempted this session (would need another rebuild/verify cycle this
session's time budget did not allow for after the identification work above).

### Separately confirmed, independently valuable finding: env vars do NOT propagate into the Host Controller child process

Verified directly (after finding and fixing a self-matching `pgrep -f 'org.jboss.as.host-controller'`
methodology bug in this session's own probing — the pattern matched the *invoking shell's own
command line* when run inline, giving false-positive "found" results in earlier checks this
session; the fix is to invoke `pgrep` from a script *file* whose own argv doesn't contain the
search string, or filter matches by verifying `/proc/$pid/cmdline` independently): a clean,
repeated, race-free `/proc/<host-controller-pid>/environ` dump shows **only the base login-shell
environment** (`USER`, `HOME`, `PATH`, `SSH_*`, etc.) — **none** of `CRATONVM_MSC_REAL_START`,
`CRATONVM_DISABLE_JIT`, or any other `CRATONVM_*` variable set on the `bin/domain.sh` invocation
reach the actual "Host Controller" grandchild process. (`JAVA_HOME` "reaching" the child is not a
counterexample — `bin/domain.sh`'s own shell script reads `JAVA_HOME` and bakes an explicit
`-default-jvm <path>` *argument* into the next process's command line; that is argument
construction, not environment inheritance.) `ProcessBuilder.start()`'s real native implementation
(`native-builtins/src/phases_late.rs`, `register_phase57_process`, the "start" registration) calls
`std::process::Command::new(program)` and `.spawn()` **without ever reading or applying the Java
`ProcessBuilder.environment()` map at all** — so if WildFly's own `ProcessController`/
`ManagedProcess` Java code builds an explicit (curated or cleared) environment map for the child
(the common, idiomatic pattern for exactly this kind of process-supervisor code), that map is
silently discarded, and the child's actual environment is left to `std::process::Command`'s
default (full inheritance from the CratonVM process that's *executing* `ProcessBuilder.start()`
— i.e., process-controller's own environment). Since process-controller *does* directly inherit
my shell's full environment (it is my direct child, launched by the `bin/domain.sh` script, not
through this same gap), and Host Controller demonstrably does *not* have my vars, the most likely
explanation is that process-controller's own Java code deliberately narrows Host Controller's
environment before/via its `ProcessBuilder.environment()` call — a call this native
implementation ignores, so the ACTUAL behavior (full inheritance) happens to differ from what
real Java `ProcessBuilder.environment()` semantics would produce, in a way that (by the accident
of "ignore the curation, inherit everything, which the curation would have kept anyway or
stripped anyway") apparently still drops custom `CRATONVM_*` vars specifically. The exact
mechanism was not fully traced (would need to read WildFly's own `ManagedProcess` bytecode or
add tracing to `register_phase57_process`'s `start` registration) but the **fact** of
non-propagation is solidly confirmed and explains real confusion in prior sessions' notes
(including this session's own initial mis-reading, from an unfixed self-matching `pgrep`, that
`CRATONVM_DISABLE_JIT=1` was reaching the process and the crash was therefore "confirmed
independent of the JIT" — that specific claim in this doc's own 2026-07-10-earlier-session entry
should now be treated as unconfirmed / likely an artifact of the same measurement bug, not a
settled fact). **Practical impact:** any future debug env var (`CRATONVM_DBG_*`,
`CRATONVM_DIAG_*`) will silently no-op inside Host Controller/managed-server processes unless
either (a) `ProcessBuilder.start()`'s native implementation is fixed to honor
`ProcessBuilder.environment()`, or (b) the flag is threaded through some channel that already
demonstrably reaches the child (e.g. baked into a `-D` JVM system property the way `JAVA_HOME`
is baked into `-default-jvm`).

### What was tried and abandoned this session (for the next session's benefit)

- Static analysis of the JIT PIC/MIC inline dispatch codegen and the loop-unroll duplicator in
  `jit/src/x64.rs` for an encoding bug — found nothing wrong; the guard is byte-correct.
  Confirmed via multiple independent full disassembly captures.
- Hypothesis "receiver is a stale/GC'd-but-bit-plausible reference" (the BUG-03/stale-reference
  family pattern used elsewhere in this codebase) — refuted; the captured values don't look like
  addresses that were ever valid heap pointers (compare to the real heap arena's consistent
  `0x2000_xxxx_xxxx`+ range visible in every other register in the same captures).
  `plausible_heap_pointer`-style bit-only checks would not catch this value.
- Hypothesis "the STW cross-thread JIT-takeover signal handler corrupts a register on resume" —
  refuted; read the handler (`vm/src/jit/xt_root_scan.rs:805-874`) end to end, it only reads
  `uc_mcontext.gregs[...]` into a side table for root-scanning, never writes back.
- Hypothesis "`CRATONVM_DISABLE_JIT` has an internal bypass gap letting some compile through" —
  investigated at length (checked all 4 call sites of `compile_with_param_slots`, all of which
  are properly gated); this hypothesis is now understood to have been chasing the wrong signal —
  see the env-var-propagation finding above, which fully explains why the flag "did nothing":
  it never reached the process being tested in the first place, in every test this session ran
  under it. Whether disable_jit's own internal gating has a genuine bug is now unknown again
  (moot until it can actually be delivered to the child process to test).

### Recommended next steps, in order

1. Get `CRATONVM_DBG_MCL=1`-equivalent visibility (or ad hoc temporary instrumentation, following
   this session's `try_compile`-print recipe) into the Host Controller child process specifically
   — either fix `ProcessBuilder.start()` to honor `pb.environment()` (native-builtins/src/phases_late.rs,
   `register_phase57_process`'s `"start"` registration currently ignores it entirely; auditing
   whether the Java-side `environment()`/`environment(Map)` accessor natives even exist and store
   anywhere retrievable is the first sub-step) or bake a temporary probe into a `-D` system
   property the way `-default-jvm` already demonstrates works.
2. With that visibility, confirm (or refute) the `ReentrantLock` field-layout-padding hypothesis
   directly: what `num_total_fields` and field-index-0 resolution does CratonVM compute for a
   live `ReentrantLock` instance during this exact boot, and does it actually diverge from what
   the JIT-compiled `lock()` body's `field_index=0` expects.
3. If confirmed, the fix is almost certainly in `classloading/src/class_manager.rs`'s Wave-3-B/RE.4
   padding logic (~line 5779) and/or its stale `synthetic_stub_fields` entry for `ReentrantLock`
   (~line 7538) — bring the stub's layout in line with the real (1-field) class, or stop
   padding real-bytecode classes against a synthetic layout describing a native implementation
   that's default-off. This machinery is shared by many other classes (`AtomicInteger`,
   `ReentrantReadWriteLock`, `Condition`, etc. all have their own `synthetic_stub_fields` entries
   in the same match block) — audit whether any of THEM have drifted the same way before
   assuming `ReentrantLock` is the only affected class.
4. Once fixed, re-run this doc's reproduction recipe, then return to
   `wildfly-domain-heap-corrupt-value-timeout.md`'s four front-line residuals, which this crash
   still gates.

This doc stays OPEN. No fix landed this session — the crash mechanism and a strong, specific,
named root-cause candidate are now understood in far more depth than before, but shipping a fix
to `classloading/src/class_manager.rs`'s field-layout-padding logic without first confirming the
mechanism live (step 1-2 above) was judged too risky to attempt with this session's remaining
budget, given that code is shared across many classes and a wrong change risks a regression
elsewhere in exchange for an unconfirmed fix here.
