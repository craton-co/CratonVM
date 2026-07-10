# WildFly domain boot: Host Controller SIGSEGV in a cached-invoke dispatch stub, right after `host=foo:add()` — respawns forever, gates the four front-line residuals

Status: OPEN — reproduced and pinned to a specific instruction sequence via live gdb, root Rust source line NOT yet identified
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
