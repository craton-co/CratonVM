# `ParameterizedSslHandlerTest` — the residual stall, and what it actually is

**Status: ONE residual left, OPEN — 2026-08-24.** The page opened with two
stalls that the `Object.wait()` lost-notify fix did not touch, each seen once,
neither explained, neither with a rate. Since then:

* **residual 2 is CLOSED, and was never a second stall.** Its distinguishing
  evidence — a `private volatile Object` holding `Int(0)` — was the watchdog
  dump mis-reading a never-written reference cell. Reproduced deterministically
  off netty and fixed in the dump. Read correctly, residual 2's stall reports
  exactly residual 1's state, so there was one residual, not two;
* **residual 1 is REPRODUCED on the current `dev`, and its proximate cause is
  now measured** — it is not a monitor, a promise, or a selector defect. The
  server's TLS handshake dies on a `NoSuchMethodError` naming
  **`java.lang.Object`** as the receiver class, so no alert is produced and the
  thing that would complete the promise never runs;
* the rate is measured, with a same-day HotSpot control on the same host:
  **1 in 163** whole-class runs, against this page's historical 5 in 80.

## The stall, end to end

One reproduction, `hl6` run 26, whole class, JUnit `@Timeout` disabled, VM
watchdog not armed (see "a watchdog dump is not a stall" below). In order:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/Object.checkClientTrusted([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"
  caller="io/netty/handler/codec/ByteToMessageDecoder.decodeRemovalReentryProtection(…) @pc=12"

[PSH] alert.server userEvent SslHandshakeCompletionEvent(
        java.lang.NoSuchMethodError: 'void java.lang.Object.checkClientTrusted(…)')
[PSH] alert.server exceptionCaught cause=java.lang.NoSuchMethodError
[PSH] alert.server userEvent SslCloseCompletionEvent(StacklessClosedChannelException)
[PSH] alert.client channelInactive promiseDone=false
[PSH] STUCK alert.promise#232 15797ms future.isDone=false
      ch=NioSocketChannel open=false active=false registered=false
      loop=33b375 inEventLoop=false shuttingDown=false pendingTasks=0
      ownerState=WAITING ownerAlive=true ownerInterrupted=false
```

`testAlertProducedAndSend` works by making the SERVER's `X509TrustManager`
throw a `CertificateException`, which the server's engine turns into a TLS
alert, which the client's engine turns into an `SSLException`, which the
client's `exceptionCaught` recognises and completes the test's promise with.
Every link after the first is conditional on the first.

Here the trust-manager call never reaches the test's `checkClientTrusted` at
all: dispatch resolves the receiver's class as **`java.lang.Object`** and
raises `NoSuchMethodError`. That is a `LinkageError`, not the
`CertificateException` the engine is prepared to convert, so **no alert is
produced** — the server just closes. The client sees a plain close, its
`exceptionCaught` never fires, `promise.trySuccess(null)` is never reached,
and `promise.syncUninterruptibly()` waits forever.

Which is exactly what this page described, from the other end: a promise that
was never completed, with no notification due, and nothing wrong at the
monitor. Nothing here is a monitor, promise or selector defect.

### `java.lang.Object` as the receiver class is a named family in this tree

`vm_exec.rs`' own comment at the `NoSuchMethodError` terminal lists the three
faces of the `ClassId(0)` / H2-CID0 stale-receiver family, and the first is
`NoSuchMethodError java/lang/Object.<method>`: a reference slot that still
names an address the collector has since reclaimed or evacuated reads back an
all-zero header, and `ClassId(0)` is `java/lang/Object`.

The call is made from inside `SSL_do_handshake` — netty's OpenSSL provider
enters BoringSSL through tcnative, and BoringSSL calls back up into Java to
run the certificate verifier. So the thread is **inside a native call when the
callback re-enters Java**, which is the exact shape the H2-CID0 note describes:
*"a thread parked in a native publishes its frames exactly once
(`deposit_root_snapshot`) and is then invisible to every collector except
through that snapshot."*

**What is established:** the receiver's class resolved to `ClassId(0)`, and the
free-list verdict (`reclaim_guard::report_reclaimed_receiver`, which runs
flag-free at this terminal) did **not** fire — so the address is not a known
reclaimed hole. That leaves the evacuated-and-not-remapped face, which is the
one `CRATONVM_DBG_VACATED_FRAMES` exists to answer and which nothing has yet
asked here.

**What is NOT established, and must not be assumed:** that the missed slot
belongs to the JNI callback's own frames. That is the hypothesis the next run
tests, not a finding.

### The next step, and it is one run

Re-run the loop with the two tracers armed — `/data/nres/huntloop.sh` does
exactly this and stops on the first catch:

* `CRATONVM_DBG_CCE_BT=1` makes this dispatch miss dump the whole frame stack
  plus the receiver's address, blocked flag, collection epoch and mirror
  identity. That names the producing frame, which is the one thing the log
  above does not (`caller=` is the nearest interpreter frame, not the caller of
  `checkClientTrusted`);
* `CRATONVM_DBG_VACATED_FRAMES=1` asks the complementary question — was this
  address one the LAST collection moved an object away from, i.e. a frame slot
  the remap did not reach.

Both are terminal-path only, so a healthy run pays nothing for them.

## The SECOND reproduction is a DIFFERENT shape, on the same netty frame

`hunt2` run 9, `testCompositeBufSizeEstimation…` invocation #4, load average
141. `PshProbe` reports `composite.donePromise#88` outstanding — and this time
the client channel is **open, active and registered**, `pendingTasks=0`, so
nothing closed and nothing is queued. Data simply stopped.

The reactor stacks say why. One reactor is frozen at the SAME frame at
152 969 ms and again at 447 579 ms — five minutes apart, `RUNNABLE`:

```
at io.netty.handler.ssl.ReferenceCountedOpenSslEngine.writePlaintextData(…:628)
at io.netty.handler.ssl.ReferenceCountedOpenSslEngine.wrap(…)
at io.netty.handler.ssl.SslHandler.wrap(…)
at io.netty.handler.codec.ByteToMessageDecoder.decodeRemovalReentryProtection(…:545)
at io.netty.handler.codec.ByteToMessageDecoder.callDecode(…:484)
```

`writePlaintextData` line 628 is `SSL.writeToSSL(…)` — a genuine tcnative JNI
native in the BoringSSL `.so`; this VM registers no shim for it
(`grep -r writeToSSL --include=*.rs` finds nothing). So the thread is in C.

And beside it, three times in that run's log:

```
WARN cratonvm_vm::runtime::interpreter::gc_and_alloc:
  STW cross-thread JIT takeover is still waiting for cooperative mutators
  rounds=64 pending=1 taken=0
```

**Zero occurrences in the 18 runs that passed, three in the one that hung.**
The `[gcbarrier-tripwire]` beside that warning — which catches a thread that
entered the blocked region without depositing a root snapshot — did NOT fire,
so the pending thread is a genuinely COUNTED running mutator that never
reaches a safepoint, not a mis-accounted blocked one.

Note that both reproductions are on the same netty frame:
`ByteToMessageDecoder.decodeRemovalReentryProtection` re-entering the
BoringSSL boundary. The alert-test hang comes back from that boundary with a
receiver whose class reads `ClassId(0)`; the composite-test hang does not come
back at all.

**What is NOT established:** that the frozen reactor is the `pending=1`
mutator, or that either is the cause rather than a consequence of the other.
`pending=1` is a count with no subject until `CRATONVM_DBG_STW_CENSUS=1` names
it, and that flag was not armed on this run. It is armed in `huntloop.sh` now,
along with a stop condition on the STW warning itself — which, at 0-in-18
against 3-in-1, is a better trigger than the hang.

## The rate, measured — which this page could not do

One binary (`fix/psh-residual-stalls-20260824`, from `origin/dev` `2e9286dde`),
one host, one afternoon, JUnit's `@Timeout` **disabled** so a hang stays a hang.

| arm | runs | hangs |
|---|---:|---:|
| CratonVM, whole class | 163 | **1** |
| CratonVM, `testCompositeBufSizeEstimation…` alone | 60 | 0 |
| HotSpot 25, whole class (control) | 40 | 0 |

Host load average over these runs ranged 18–148 — the same band this page's own
rates were taken in (it records 4/20 at load 12–84 and 1/30 at 6–13). 40 of the
163 CratonVM runs used an argfile WITHOUT the instrumented copy of the test
class, so the instrument is not what suppressed the rate.

**1 in 163 (0.6%) against this page's historical 5 in 80 (6.25%).** The
monitor fix that closed the first stall is on this binary and was not on most
of the historical runs, which is the obvious explanation for most of that gap;
what is left is this defect, and it is a tenth as frequent, which is why
catching it a second time needs a loop rather than a run.

### A watchdog dump is not a stall — the trap that cost three attempts

`--stack-dump-on-timeout` fires on a fixed wall-clock deadline, and this host
reached load average 148 while these loops ran. Two runs tripped a 280 s and
then a 420 s deadline and were logged STALL. **Neither was one:**
`[WAIT-CENSUS]` reported `waited_ms=6` in the first — a healthy mid-test wait —
and no waiter at all in the second, and tests were still finishing every 30 s
in both. One CratonVM run passed cleanly in **667 s**.

Three columns settle it, all already in the log: the census's `waited_ms` (a
real stall reads 348 859; a healthy wait reads single digits), whether ONE
netty operation stayed outstanding for minutes, and whether tests were still
completing. `/data/nres/triage.sh` prints them.

So the loops behind the table do not arm the VM watchdog at all. `PshProbe`
halts the JVM with exit 97 once one netty operation has been outstanding for
two minutes, after printing that operation's state and every reactor's stack.
A hang costs two minutes, a slow run costs whatever it costs, and "the loop
found nothing" means something.

## Residual 2 — `DefaultChannelPromise.result` holding `Int(0)` — CLOSED

This page asked two questions and forbade assuming either answer: is the
`Int(0)` the CAUSE or an artefact of a mis-resolved field index, and are the
run's 34 `primitive-into-reference` guard hits on THIS field?

### It is neither. It is a never-written reference cell, and the dump was the one reader in the VM that did not know

`Value` is `#[repr(u32)]` with `Int = 0` and `Object = 4`
(`types/src/value.rs`), so a zero-filled 16-byte cell decodes as
`Value::Int(0)` and **not** as `Object(None)`. The interpreter's
`init_primitive_fields` has written the `Object(None)` tag into every reference
field since G56-1 (2026-08-17) — its doc comment carries the table — but the
JIT's allocation arms do not: `jit_post_alloc_init`'s per-class recipe stores
only `PrimKind::{Int,Long,Float,Double}`, `jit_init_primitive_fields`'
`_ => None` skips `L`/`[`, and the inline-TLAB arm deliberately relies on the
zero fill (`jit_new_site_flags` counts only `J`/`F`/`D` as needing init).

Every OTHER reader repairs the tag locally — the interpreter's `getfield`
fixup, the inline read's payload-only load, `coerce_field_value_for_slot`, and
`values_equal_for_cas`, which equates `Object(None)` and `Int(0)` in **both**
directions so netty's `RESULT_UPDATER.compareAndSet(this, null, …)` still
succeeds against such a cell. The watchdog dump did not, and reported
`result_is=not-a-reference-slot`, which reads as heap corruption.

**Reproduced without netty, interleaved, on the shipped binary.**
`probes/JitZeroCellProbe.java` allocates a two-field promise-shaped object from
a hot method 400 000 times, never writes its reference field, and parks on the
last one so the watchdog dumps it:

| arm | runs | `result` cell | `keep.result == null`, read in Java |
|---|---:|---|---|
| JIT (default flags) | 5 | `Int(0)` in **2**, `Object(None)` in 3 | **true in all 5** |
| `--nojit` | 5 | `Object(None)` in **5** | **true in all 5** |

`--nojit` never produces it; with the JIT on it is a coin toss, because what
decides the tag is which allocation arm served that particular allocation, and
that is a compile-timing outcome. **That intermittency is the netty
observation** — one stall showing `Int(0)` where others showed a proper
`Object`, on the same field of the same class.

The last column settles this page's question: Java reads the cell as `null` in
every run of both arms. The value is real, the field index was right (the dump
now prints it, with the receiver's whole declared layout beside it), nothing
was corrupt, and the promise in run 8 was **PENDING** — residual 1's state.

(An earlier pass of this A/B ran each arm once and would have gone into this
page as "the compact field layout is the discriminator". It is not — the
default, compact-enabled arm produces `Int(0)` two runs in five. One run per
arm was not a measurement.)

### The 34 guard hits

Same population, and not an attribution. The guard fires on a
descriptor-aware READ of a reference field whose cell still holds the
allocator's zero; `init_primitive_fields`' own G56-1 note measured 1 105 of
1 120 coercion events as exactly that shape on a `--jdk-only` run, before the
interpreter half was fixed. A count of them says how many JIT-allocated
reference fields were read before first write. It cannot name a field, and this
page was right to refuse to read it as if it could.

### What changed, and what deliberately did not

`dump_wait_object_state` now resolves `result` through
`resolve_declared_instance_field` and prints the **slot index, the declared
descriptor and the declaring class**, plus the receiver's complete instance
layout — so "the dump read the wrong slot" became a claim a reader can check
rather than one they must trust. It normalises a non-`Object` value at a slot
declared `L`/`[` to `Object(None)` before classifying, exactly as the VM's
other four readers do, printing both readings and naming which one the verdict
is about. And it answers `no-result-field(NOTHING-WAS-READ)` rather than
`not-a-reference-slot` when the receiver has no `result` field at all — the
same class of mislabel, found by the positive control below.

The allocator asymmetry is left alone, on purpose. Making the JIT write the
`Object(None)` tag would add a store per reference instance field to the
allocation path and would disable the inline-TLAB no-call arm for essentially
every class, for **zero** behavioural change — the table above is the evidence
for "zero".

## What the archived stalls said, once the census question was asked of them

This page's own next step for residual 1 was: *"at stall time, dump every
thread parked in `Object.wait()`… the raw material may be on disk."* It was.

**`OFF 19` — `testAlertProducedAndSend`, the `result_is=null(PENDING)` stall.**
Of 260 registered threads, **four were alive**: `main`, parked in
`DefaultPromise.awaitUninterruptibly` on the test's own promise, and three
reactors, all `blocked=true deposit=live` at `NioIoHandler.select@136`. **No
other thread was in `Object.wait()` at all** — so "the thread that would have
completed the promise is itself parked" is refuted for that stall, and the
completer was not blocked on a monitor either. Its selector census: 16 open
selectors, and **exactly one registered key in the whole process** — the
listener, `interest=0x10` (`OP_ACCEPT`), `ready=0x0`. Both data channels were
already closed and deregistered.

**`run 8` — `testCompositeBufSizeEstimation…`, the `Int(0)` stall.** Same
selector shape, but the reactors are NOT parked: `in_flight_selects=0` on every
selector, all three reactors `blocked=false` with no live dump. And the awaited
object is a `DefaultChannelPromise`, i.e. a **close** future rather than the
test's own promise — so the composite test's failure is reached by a different
route than the alert test's, and only the alert route has been caught with the
proximate cause in hand.

## The instruments — what a returning stall prints

### `[WAIT-CENSUS]` — every thread parked in `Object.wait()`

`ThreadRegistry::waiting_monitor_census` reports every registered thread whose
`jmx_waiting_monitor` slot is set — the same GC-forwarded root the wait-site
dump already resolves through, so every row is sound under a moving collector,
unlike `Monitor::wait`'s own entry-time local.
`SharedVm::dump_object_wait_census` prints one `[WAIT-OBJECT]` block per row
from the watchdog, right after the thread summary.

An EMPTY census is printed as a result rather than omitted: it means no thread
in the process is inside `Object.wait()`, so the stall is parked on something
else and the whole `Object.wait()` line of enquiry is the wrong one. That line
has already earned its keep — it is how one of the false watchdog fires above
was recognised.

Positive control, `probes/WaitCensusProbe.java`: two threads park on two
different objects, one carrying a `result` field and one not. Both rows appear;
the slot-provenance line resolves on the first and reports `UNRESOLVED` on the
second. Unit-tested in `thread_registry`
(`waiting_monitor_census_lists_every_waiter_and_only_waiters`), including that
a thread which LEAVES the wait drops out — a high-water mark here would
manufacture the second reading.

### `kernel=` — the KERNEL's readiness for every registered socket

The selector census printed this multiplexer's OWN bookkeeping
(`interest_ops` / `ready_ops`), and "the peer never sent anything" and "the
peer sent and this multiplexer never reported it" both render as `ready=0x0`.
Each key now carries a zero-timeout `poll(2)` of its fd taken at census time,
rendered `IN|OUT|PRI|ERR|HUP|NVAL`, beside the selector's `epoll_fd`, its
wakeup-pipe fds, and whether that pipe holds an unconsumed wakeup. `POLLIN` on
a socket whose key reads `ready=0x0` with the reactor parked is readiness lost
inside the VM; a quiet poll on every socket says the bytes were never sent, and
the defect is upstream of the selector entirely.

### `monitor owner=` on the wait-object dump

`Object.wait()` must have RELEASED the monitor. An `owner` still equal to the
waiting thread would mean no completer's `synchronized` block can ever run,
which presents as `notify=0` on a promise nobody completed — indistinguishable
from a completer that never ran, and those need opposite investigations. Read
under the state lock the wait loop already holds, beside `entry_count`,
`parked_waiters` and `pending_notifies`.

### `PshProbe` — the Java side

`/data/nres/src-probe/io/netty/handler/ssl/PshProbe.java`, loaded through a
classpath overlay so netty's own sources are untouched. It registers each
awaited netty operation, reports one that has been outstanding for 15 s with
the channel's open/active/registered state and — the deciding number — the
owning event loop's `pendingTasks()`, and halts the JVM at two minutes.
`pendingTasks > 0` on a sleeping reactor would be a lost `Selector.wakeup()`;
the reproduction above reads `pendingTasks=0`, which is what ruled that out.

## Repro

The harnesses are in-tree at `probes/psh-probe/` (with their own README);
the copies below are the working ones on Azure host 2 (`azureuser@20.80.105.49`),
under `/data/nres`, carrying that host absolute paths:

```bash
bash /data/nres/huntloop.sh 400 <tag>                # tracers armed, stops on the first catch
bash /data/nres/hangloop.sh 40 <tag> full            # load-proof; PshProbe halts at 120 s stuck
bash /data/nres/hangloop.sh 40 <tag> full hotspot    # the same command on HotSpot 25
NRES_ARGS=/data/nres/ossl-plain.args \
  bash /data/nres/hangloop.sh 40 <tag> full          # ... without the test-class overlay
bash /data/nres/hangloop.sh 60 <tag> comp            # the composite test alone
bash /data/nres/nresloop.sh 14 <tag> full            # VM watchdog armed (census + kernel poll)
bash /data/nres/zerocell-ab.sh 5                     # the JIT vs --nojit zero-cell A/B
bash /data/nres/triage.sh   /data/nres/<tag>         # stall vs merely-slow
```

`/data/nres/ossl.args` is a private copy of `gen-openssl-args.sh`'s output with
`/data/nres/overlay` prepended, so the instrumented copy of the test class
shadows netty's for these runs only and no other session on this shared host
sees it. `overlay.py` generates that copy from netty's source; every insertion
is a registration or a print, and `PshProbe.await` calls exactly the
`syncUninterruptibly()` the test called. `ossl-plain.args` is the same argfile
WITHOUT the overlay.

`OpenSsl.isAvailable` must be true; `gen-openssl-args.sh` is what makes it so.
