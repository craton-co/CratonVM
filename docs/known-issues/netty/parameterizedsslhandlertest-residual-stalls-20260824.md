# `ParameterizedSslHandlerTest` — the two stalls that are NOT the lost `notifyAll()`

**Status: OPEN — 2026-08-24.** The stall this test class was known for is
closed: `Object.wait()` depended on the condvar alone and lost a delivered
`notifyAll()`. That page is
`fixed-suite-bugs/netty/parameterizedsslhandlertest-object-wait-lost-a-delivered-notify-FIXED-20260824.md`
and it carries the whole investigation, including the two claims it had to
withdraw.

This page exists because the instruments added there showed the class has
**more than one** way to stall, and only one of them was fixed. Both of the
others were observed once each, with a dump, and neither is explained.

Do not read either as a monitor defect. The monitor fix's own A/B — 0 stalls in
25 against 5 in 25 with the condition switched off — includes one of these in
the OFF arm's five, and it would have stalled either way.

## Residual 1 — a promise that was never completed, with no notification due

```
OFF 19  STALL  wall=421  notifies_since_wait=0  polls=68842  signalled=0
                          consumed=0  result_is=null(PENDING)
```

`result_is=null(PENDING)` is the dump comparing the promise's `result` field
against that class's own `SUCCESS` / `UNCANCELLABLE` statics: the field is
genuinely `null`, so the promise is pending and `isDone()` is correctly false.
`notifies_since_wait=0` and a monitor total of `notify=0` say no `notifyAll()`
was ever served on that monitor — which is CORRECT for a pending promise.
Nothing was lost here. The waiter is doing exactly what it should.

So the defect is upstream: **whatever should have completed this promise did
not.** That is a different investigation with a different suspect list — the
event loop, the task queue, the operation the promise stands for — and none of
the instruments on the fixed page can see it, because they all describe the
waiter.

The one thing worth doing first is cheap and has not been done: at stall time,
dump **every** thread parked in `Object.wait()` and what each is parked on,
rather than the first one the watchdog reaches. If the test thread is pending
because the thread that would complete it is itself parked, the current dump
names the symptom and never the cause. The logs already report
`N thread(s) dumped`, so the raw material may be on disk.

## Residual 2 — `private volatile Object result` holding `Int(0)`

```
run 8  DefaultChannelPromise  result=Some(Int(0))  result_is=not-a-reference-slot
       notifies_since_wait=0  (monitor totals: notify=0)  orphan 0
       34 `primitive-into-reference` guard hits in that run's log
```

`io.netty.util.concurrent.DefaultPromise.result` is declared
`private volatile Object`. The slot holds a PRIMITIVE. That is the
`G30-1-the-silent-reference-slot-coercion` family, and the run carried 34
`species="primitive-into-reference"` guard hits.

`notify=0` is consistent with it: if `result` can never satisfy `isDone0`, the
promise never completes, nothing ever notifies, and the waiter parks forever.

**What is NOT established**, and must not be assumed:

* whether the `Int(0)` is the CAUSE or an artefact of the dump resolving the
  wrong field index for `DefaultChannelPromise`. `dump_wait_object_state`
  resolves `result` through `resolve_field_index_in_hierarchy` on the
  receiver's class, and the same code answered with a proper `Object` on every
  other observation — but "it usually works" is not the same claim as "it
  resolved the right slot here";
* whether the 34 coercion hits are on THIS field or elsewhere in the run. The
  guard's own doc is explicit that it sees descriptor MISMATCHES only, so a
  quiet log is not a clean one and a loud one is not an attribution.

Settling both is one run: print the resolved field INDEX and the receiver's
declared field list beside the value, and turn on `CRATONVM_DBG_COERCION=1` so
every coercion arrives with a backtrace.

## Rate

Neither residual has a rate. Each was seen once, in 50 runs of the fixed-page
A/B plus 20 of the instrumented loop that preceded it. The class's overall
stall rate is strongly load-dependent — 4/20 at load average 12–84, 1/30 at
6–13 on the same host in the same session — so a rate for either of these needs
a same-day control and is not worth quoting from what exists.

## Repro

The harnesses from the fixed page, on Azure `vm1`:

```bash
/tmp/mjnloop.sh 40 <tag>     # rate + [WAIT-OBJECT] with the notify counter
/tmp/mjnab.sh   20 <tag>     # interleaved A/B of the two perf switches
/tmp/mjnfix.sh  25 <tag>     # interleaved A/B of CRATONVM_MONITOR_PENDING_NOTIFY
/tmp/mjncensus.sh 14         # ON-arm runs with CRATONVM_DBG=monitor-notify
```

`gen-openssl-args.sh -o /tmp/ossl.args` first, and confirm
`OpenSsl.isAvailable == true`. **The host carries other sessions' builds** —
check `uptime` before quoting any rate, and interleave the arms.
