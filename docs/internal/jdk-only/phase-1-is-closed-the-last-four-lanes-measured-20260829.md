# Phase 1 is CLOSED — the last four lanes, measured

**Status: COMPLETE 2026-08-29.** Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`.

`HANDOFF-20260828-SCOPE.md` had Phase 1 at **5 of 9 lanes closed** since
2026-08-27: A/B/D/G/I, by `probes/Phase1Sweep.java`, 80 rows. The other four had
never been measured the same way — not known broken, just unproven.

They are now, and **all nine are closed**.

## 1. What Phase 1 actually claims

> An essential native survives strict mode, asks for a FABRICATED receiver, is
> correctly refused, and kills its caller with `NoClassDefFoundError`.

That is a mechanism, not a defect list, so the test is whether the mechanism
FIRES. `probes/P1RemainingSweep.java` (29 rows, both modes, HotSpot 25.0.3+9)
labels a `NoClassDefFoundError` as `PHASE1-KILL` and prints it distinctly from
every ordinary refusal, so the two can never be read for each other.

| lane | mint site | payload exercised |
| --- | --- | --- |
| **P1-C** | `SSLSocket{Input,Output}Stream` | stream access on an `SSLSocket`, the context, the factories, the engine |
| **P1-E** | `java/lang/foreign/DowncallHandle` | a real `strlen` downcall through `Linker.nativeLinker()`, plus `Arena` |
| **P1-F** | `cratonvm/internal/SnapshotEnumeration` | `ConcurrentHashMap.keys()` / `.elements()`, empty and 64-entry |
| **P1-H** | `java/util/concurrent/CompletedFuture` | `AsynchronousFileChannel` write-then-read, EOF, negative position, close |

All four mint sites are still LIVE in the tree — this is not a lane closed by
deleting its subject.

```text
PHASE1-KILL rows:  0 strict / 0 compat
final:            29/29 rows, 0 differing lines, BOTH modes
```

## 2. Three of the first five "defects" were the probe

The first run showed five differing rows. Three were mine, and all three the
same mistake: I resolved FFM methods on the returned object's own class.

```text
Linker.nativeLinker()      -> jdk.internal.foreign.abi.x64.…   non-public
SymbolLookup.find          -> a lambda                          non-public
MemorySegment              -> an implementation                 non-public
```

HotSpot refuses a reflective `invoke` on a non-public class with
`IllegalAccessException`; CratonVM allows it. So the rows measured **reflective
access to JDK internals**, which is a real difference and not the one the lane
is about. Resolving every method on the PUBLIC INTERFACE
(`java.lang.foreign.SymbolLookup`, `…MemorySegment`) is what makes the row about
Panama.

A fourth asserted `mh.getClass().getSimpleName()` — HotSpot answers
`BoundMethodHandle$Species_LLLL`, an unspecified implementation detail. The
specified fact is the handle's TYPE, and that is what the row asks now.

**Four of five rows in the first draft were about the instrument.** A probe's
setup is code that can be wrong, and a reflective probe of a modular JDK is
mostly setup.

## 3. The one real defect

```text
SSLContext.getDefault().getProtocol()
  HotSpot   Default
  CratonVM  TLS
```

Not interchangeable. `"Default"` names the JDK's PRE-INITIALISED context — the
one `getDefault()` is contracted to hand back ready to use. `"TLS"` names a
context you asked for by protocol and must `init()` yourself. Code that switches
on `getProtocol()` to decide whether a context needs initialising reads ours as
unconfigured.

### The fix went to the registration that WINS

`javax/net/ssl/SSLContext.getDefault` is registered TWICE — in
`phases_late/ssl_security.rs` (which stamps `TLSv1.3`) and in
`net_phase_e.rs::register_re6_ssl_context` (which stamped `TLS`). The measured
answer was `TLS`, so the second is the live one, and `net_phase_e.rs` says so in
its own comment: *"The sibling `SSLContext.getDefault()` duplicate in this same
function IS intentional and documented — that one deliberately relies on the
ordering to win."*

Editing the first match would have changed a string nothing reads. The tree
named its own winner; the measurement confirmed it.

## 4. Verification

```text
probes/P1RemainingSweep.java     29 rows   0 differing, both modes
probes/L8InvokeLookupSweep.java  83 rows   1 differing (unchanged)
probes/L5ModuleInvokeSweep.java 125 rows   1 differing (unchanged)
```

Arms and gates: see the landing commit.

## 5. What Phase 1 being closed does and does not mean

It means the roadmap's stated mechanism no longer fires on any of the nine
lanes, on the payloads the roadmap named. It does **not** mean no fabricated
class can ever kill a caller: `RJdkEnumerations` was exactly that shape and was
found on 2026-08-29 by the corpus, not by a Phase 1 probe, and fixed by L6
(`844c581fa`). The definition-of-done screen's own rule is the durable one —
the blocking set is *refused AND not recovered from*, and it is found by running
real programs, not by re-running the nine lanes.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out P1RemainingSweep
```
