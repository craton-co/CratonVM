# A `String` receiver guard, speculated with no evidence, at every `CharSequence` call site — FIXED 2026-08-25

**Status: FIXED**, `CRATONVM_JIT_RECEIVER_DESPEC` (default-ON, `=0` restores the
old route). Worth **6.6x / 2.6x** on the two loops of netty's
`HttpHeaderValidationUtilTest`, and it takes that class's deopt census from
**6 236–123 832 entries to 19**.

All numbers: **Azure Linux host `vm1`, idle (load 1.1–2.9)**, 2026-08-25, release
build, real-JDK mode, ONE binary with the flag off and on. Six interleaved
(ABBA) readings per arm.

## What the compiler was doing

`try_resolve_string_intrinsic` (`jit/src/lib.rs`) matches `length()`,
`charAt(int)` and `isEmpty()` on **two** declared receiver types:

* `java/lang/String` — `final`, so the site is monomorphic and the inline
  String-layout decode needs no guard (`guard_class_id == 0`);
* `java/lang/CharSequence` — the receiver may be **any** `CharSequence`, so the
  same inline decode is emitted behind an exact class-id compare against the
  real `String` class id.

The second case is a speculation, and an unusual one: **the miss edge is a
deopt, not a fall-through to dispatch.** `bytecode_walk.rs`'s String-access
region emits `CMP DWORD [RAX+0], guard_class_id ; JNE <deopt stub>`, and the
stub re-runs the whole method in the interpreter.

And the compiler took that bet **with no evidence about the receiver at all**.
Receiver types are recorded by the interpreter into `profile_store`, but
`is_profiling_enabled()` is driven by `CRATONVM_TIER_PGO`, which is
**default-OFF** — so on an ordinary run there is no receiver profile to read,
and every `CharSequence.length()` / `charAt()` in the program was compiled as
"assume `String`".

### Why that is not a 50/50 bet — measured

`probes/CharSeqStringIntrinsicProbe.java` prices both sides. Four arms, each
with its own call site so no site is polymorphic; four interleaved readings per
arm, one binary:

| receiver, through a `CharSequence`-declared site | guard on | guard off |
|---|---:|---:|
| `length()` on a `String` (guard hits) | **7.85–8.37 ns** | 28.50–31.36 ns |
| `length()` on a `StringBuilder` (guard misses) | 416.82–460.71 ns | **282.56–320.55 ns** |
| `charAt()` on a `String` (guard hits) | **9.04–10.26 ns** | 53.83–66.35 ns |
| `charAt()` on a `StringBuilder` (guard misses) | 652.36–716.12 ns | **310.99–399.96 ns** |

A hit is worth **22 ns** (`length`) / **47 ns** (`charAt`). A miss costs
**140 ns** / **320 ns** — plus, once the deopts accumulate, a whole-method
`MakeNotCompilable` that retires the method to the interpreter for good. The
break-even is therefore around **87% `String` receivers**, not 50%, which is
where `MIN_GUARDED_RECEIVER_PCT = 90` comes from. It is a measurement, not a
round number picked to look conservative.

netty's `HttpHeaderValidationUtil.validateValidHeaderValue(CharSequence)` is
the worst case the bar exists for: its `CharSequence.length()` at bci 1 is
reached only with an `AsciiString` and with the anonymous `CharSequence` the
test builds. The guard is not mis-tuned there; it is **wrong about the
program**, on every single call, forever.

## The fix

A guarded (`guard_class_id != 0`) String call-site intrinsic is now admitted
only when something says the guard will hold. Asked at the **resolver**, in both
compile doors — `try_compile_inner` (`jit/src/lib.rs`, the method-entry door)
and `compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`, the OSR
door), which previously had no receiver profile at all:

1. **`crate::deopt::despec_contains(method_key, pc)`** — this exact bci has
   already deopted `PER_BCI_DESPEC_LIMIT` times on this guard. Veto, always.
2. **`receiver_profile_supports_guard(profile, pc, class_id)`** — at least
   `MIN_GUARDED_RECEIVER_EVIDENCE` (32) recorded receivers at this bci, at least
   `MIN_GUARDED_RECEIVER_PCT` (90) percent of them the guarded class. Admit.
   Fail-**closed**: no profile, no entry, too few samples ⇒ not supported.
3. **`charseq_blind_guard_enabled()`** (`CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC`,
   now default-**OFF**) — the old behaviour, kept as an explicit opt-in so the
   two halves can be bisected apart.

`java/lang/String`-declared sites carry `guard_class_id == 0` and never reach
any of this; their intrinsic is unchanged, and so is every unguarded direct
call.

`DeoptimizationLog::recommend_action_at_bci` is the policy half: a
`ReceiverTypeChanged` / `ClassCheck` deopt at a bci that is ALREADY in the
de-spec registry no longer escalates to `MakeNotCompilable`, because the next
compile really will come back without that guard. The per-method backstop is
kept, not removed — at `CRATONVM_JIT_DESPEC_SPARE_FACTOR` (default 2) times
`max_deopts_per_method` the blacklist fires anyway, because "de-spec'd" is a
claim about the NEXT compile and a site still trapping past that point is
evidence the claim is wrong.

### The layer this had to be moved OFF, and what it cost

The first implementation put the de-spec consult in the **backend**, as a
`direct.filter(..)` in `x64::bytecode_walk`'s invoke ladder — deliberately
copying the `ArraycopyPrimitive` filter that was already there. It read as
**inert**: `sites-declined=51`, deopts `3502 → 3055`.

It was worse than inert. A site the resolver registers as an intrinsic takes
`direct_calls.push(..); continue;` in `try_compile_inner` **above** the
`invoke_info.push` — so it never gets dispatch metadata. Dropping the direct
call in the backend therefore leaves that pc with **neither** `direct` **nor**
`info_ptr`, and the emitter falls through to the unconditional `UnreachedCode`
trap at the bottom of the invoke arm. The "declined" site does not dispatch: it
**deopts on every execution**.

The census says it plainly once you ask: 33 declines at
`oldHeaderValueValidationAlgorithm pc=6`, only one compile after the last of
them, and **1 466 deopts at that same bci afterwards** — plus `unreached=85 193`
in a 65 536-iteration run, an `UnreachedCode` trap taken more than once per
iteration.

**The two pre-existing backend filters have the identical shape and therefore
the identical defect**: `ArraycopyPrimitive`'s de-spec filter and
`StringIndexOfChar`'s constant-needle filter both decline a site the resolver
already registered. Neither is touched here — each needs its own A/B — and both
are named in the comment that replaced the removed filter.

## Results

`probes/io/netty/handler/codec/http/HeaderValidationLoopRate.java`. Its ABSOLUTE
numbers are uncalibrated and must not be used (see its own header); this is a
same-host same-binary RATIO between two arms. Six interleaved readings each:

| | guard on (`=0`) | default | ratio |
|---|---:|---:|---:|
| value loop, ns/iter | 4617.59 – 5594.00 | **657.65 – 722.72** | **6.6x** |
| name loop, ns/iter | 2648.46 – 5070.30 | **1153.60 – 1250.41** | **2.6x** |

The counters, on the same probe at `n=20000` (65 536 iterations), four readings
per arm — and these are the result, because the clock on this host is shared:

| | guard on (`=0`) | default |
|---|---:|---:|
| deopt-log entries | 6 236 – 123 832 | **19** (all four readings) |
| deopts at `oldHeaderValueValidationAlgorithm` bci 6 | 4 419 – 4 440 | **0** |
| `reason=UnreachedCode` traps | 0 – 36 006 | **0** |
| `receiver despec: guards-emitted` | 146 – 171 | **0** |
| `receiver despec: profile-declined` | 0 | **11** |

`profile-declined=11` is the engagement counter. A run where it is zero is a run
where this changed nothing, whatever the clock says — which is why it is printed
under `CRATONVM_DBG=jit-method-stats` alongside `guards-emitted`, so "wired and
never needed" and "no guarded site compiled at all" cannot be confused.

### Correctness, and no regression elsewhere

* **netty `codec-http`, 93 classes**, one binary, flag off and on, 4 shards, the
  suite's own 180 s wall: **identical result sets** — 88 `PASS`, 3 `HANG`, 2
  `NOTESTS` in both arms, and a per-class join on
  `(status, found, ok, failed, aborted)` is empty.
* Per-class walls in that run: biggest improvement **`DefaultHttpRequestTest`
  49.703 s → 31.871 s**, plus six classes 1.5–1.8 s faster. Biggest regression
  +504 ms on a 1 036 ms class (`multipart.HttpDataTest`), with the next four
  between +175 ms and +354 ms — all sub-second classes, all inside this host's
  run-to-run spread.
* **`regression-suite/run.sh`**, both arms: **71 of 71 scheduled vectors pass**,
  0 failed, 0 coverage errors.
* `cargo test --release -p cratonvm-jit --lib` 2 105 passed / 0 failed;
  `-p cratonvm-vm --lib` 2 615 passed / 0 failed;
  `-p cratonvm-native-builtins --test stub_ratchet` 12 passed / 0 failed.

## Two instrument findings worth keeping

**`reason=ReceiverTypeChanged` does not mean a receiver guard failed.**
`snapshot_pre_intrinsic_call(pc, DeoptReason::ReceiverTypeChanged)` is called at
the top of the intrinsic ladder **and** on the plain MIC/PIC dispatch arm, so
that reason is stamped on the pre-invoke snapshot at essentially every invoke
bci. A `reason=` census therefore names the **site** correctly and the **cause**
only by inference. The known-issue page this fix came from read "1.67
`ReceiverTypeChanged` deopts per iteration … on a receiver that is genuinely
bimorphic" — the site was right, the receiver description was right, and the
conclusion was wrong: the speculated class is a **third** one that never appears
at that site at all.

**A de-spec that fires is not a de-spec that works.** The registry's own log
line says "speculation suppressed on next compile (method stays compilable)".
For two days of this work that sentence was false in two different ways — first
because nothing on the receiver-guard path read the registry, then because the
thing that read it did so in the backend and compiled a trap instead. Both
times `sites-declined` was non-zero. The counter that separated them was the
deopt count at the SAME bci, AFTER the decline.

## Repro

```bash
cratonvm --java-home <jdk> -cp . CharSeqStringIntrinsicProbe 20000000
```

```bash
CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC=1 cratonvm --java-home <jdk> -cp . CharSeqStringIntrinsicProbe 20000000
```

```bash
CRATONVM_DBG=jit-method-stats CRATONVM_DBG_DEOPT=1 cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 20000 2>&1 | grep -E 'receiver despec|reason=' | tail -3
```

```bash
CRATONVM_JIT_RECEIVER_DESPEC=0 cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 2000000
```

## Related

* [`../../../known-issues/netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](../../../known-issues/netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md)
  — the class this was written for. Still OPEN: this closed its deopt defect,
  not its throughput wall.
* `osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md` — the same
  class's previous blocker, and the same lesson about engagement counters.
