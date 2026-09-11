# `stack_trace_across_tiers` failed deterministically on `dev` — FIXED 2026-09-11

**Status: FIXED.** Found 2026-09-09, diagnosed and closed 2026-09-11. The fix is
in the FIXTURE (`probes/StackTraceAfterOsr.java`), and the thing it restores is
the test's ability to fail.

**Severity while open:** the target is listed in `tools/e2e-ratchet.txt`, so the
CI step "E2E prerequisites are real (`CRATONVM_REQUIRE_E2E`)" was RED on `dev`.

---

## 1. What was actually wrong, in two halves

The page was filed on the FIRST half and closed on the SECOND, and they are not
the same defect.

**Half one (2026-09-09, closed by `60965bb3e`).** The
`CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` arm asserted that `mid` and `outer`
DISAPPEAR from `after_main_osr`, and they stopped disappearing. Nothing in the
VM was broken: all three rows still matched the `CRATONVM_DISABLE_JIT=1`
interpreter oracle byte for byte. What changed was an INLINING decision, so the
switch had no inlined callee to take away. That commit made the arm take the
strong form only when the chain actually contributed a frame.

**Half two — the one this page was still open for.** The other branch reports
the unexercised coverage as an ENVIRONMENT outcome, which is **a failure under
`CRATONVM_REQUIRE_E2E`** — and that is the arm CI runs. So after `60965bb3e` a
plain `cargo test` passed and the ratcheted CI step stayed red, which is exactly
what the commit intended ("the note says how to restore the coverage instead").
Restoring it is what this page was waiting for.

## 2. Why the chain stopped inlining, measured rather than argued

`CRATONVM_DBG_JITC=1` and `CRATONVM_DBG_DEOPT=1` on the third throw, Windows,
release binary, Temurin 25.0.3+9:

```text
[ir] inline-plan StackTraceAfterOsr.probe: 1 site(s), 3 spliced bodies
                                           -- outer, mid AND leaf
[cratonvm-deopt] StackTraceAfterOsr.probe   reason=UnreachedCode bci=7
                                            action=MakeNotCompilable
[cratonvm-deopt] StackTraceAfterOsr.outer   reason=UncommonTrap  bci=1
                                            action=Reinterpret
[cratonvm-deopt] StackTraceAfterOsr.mid     reason=UncommonTrap  bci=1
                                            action=Reinterpret
[swchain] entries=2  e0 activations=1 [main]  e1 activations=1 [leaf]
```

`ir-splice-getstatic` going default-ON on 2026-09-09 enlarged every body on this
chain, and the consequence nobody predicted is that `leaf` — the THROW SITE —
began to be spliced all the way up. `probe`'s compiled body then contained the
throw itself, trapped on it the first time it was entered, and was made **not
compilable**; `outer` and `mid` trapped at their own splice sites and were
reinterpreted. At the third throw every frame below the OSR'd `main` is an
interpreter frame, and the only compiled artifact left on the chain is `leaf`,
which splices nothing.

That is why the trace stayed CORRECT — it matches the interpreter because it is
almost entirely produced by the interpreter — while the switch's arm measured
nothing.

## 3. The fix

One expression in the probe:

```java
static final String MARK = "";
static int leaf(int i)  { return table[i & 7][MARK.length()]; }   // the throw site
```

`MARK.length()` is 0, so no reader of the trace can see it. What it changes is
whether `leaf` can be spliced: the inline resolver refuses a callee that calls
something it can neither splice nor direct-bind, and `java/lang/String.length()I`
is such a call (`inline-resolve REFUSED ... is neither spliced nor
direct-bound`). With `leaf` refused, `probe`'s compiled body splices `outer` and
`mid` and CALLS `leaf` — the pre-2026-09-09 shape, and the one the test's
assertions were written against:

```text
[swchain] entries=1
  e0 activations=3 [leaf | probe | main]
                   -- `mid` and `outer` are INLINE LEVELS of `probe`
```

### Three lesser levers, and why none of them is the one

Each was built and run before this one was chosen.

* **Warm `probe` itself** (`warmProbe(5_000)`), so its artifact is entered.
  Refuted: `probe` deopts at bci 7 on EVERY compiled entry and is eventually
  made not compilable — 1 478 deopts in one run. A method whose compiled body
  contains the throw cannot stay compiled through it.
* **Pad `leaf` past the inline size caps** (≈40 arithmetic statements). WORKS —
  all arms correct — but it is 40 lines of noise that pins an inline-budget
  constant nobody wrote down.
* **`static synchronized int leaf`.** Refuted, and it found a defect on the way
  out: the trace DUPLICATES, `len=8 [leaf mid outer probe mid outer probe
  main]`. `synchronized` does nothing here but route the raise through
  `materialize_implicit_signal`, which builds the throwable while the compiled
  frames are still live — so `fillInStackTrace` had already spliced them and the
  snapshot appended a second copy. Fixed in this change (§5), and this lever is
  still not the one: the monitor is noise in a probe about frames.

## 4. The measurement that closes it

Windows, release binary, Temurin 25.0.3+9, one `.class` file, four arms:

```text
HotSpot -XX:-OmitStackTraceInFastThrow
  before_any_warm    len=5 [leaf:54 mid:55 outer:56 probe:71 main:83]
  after_helper_warm  len=5 [leaf:54 mid:55 outer:56 probe:71 main:87]
  after_main_osr     len=5 [leaf:54 mid:55 outer:56 probe:71 main:95]
CratonVM, default                    -- identical, all three rows
CratonVM, CRATONVM_DISABLE_JIT=1     -- identical, all three rows  (the oracle)

CRATONVM_JIT_NO_INLINE_FRAME_MAP=1
  after_main_osr     len=3 [leaf:54                probe:71 main:95]  <- mid, outer GONE
CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1
  after_main_osr     len=5 [leaf:-1 mid:55 outer:56 probe:-1 main:91] <- the historical -1
CRATONVM_JIT_NO_OSR_PC_REFRESH=1
  after_main_osr     len=5 [leaf:54 mid:55 outer:56 probe:71 main:91] <- back-edge pc
```

All three kill switches revert their own half and nothing else, which is what
the test asserts and what was unmeasurable while the chain contributed no
inlined frame. Only the `NO_INLINE_FRAME_MAP` row changed shape from the
pre-2026-09-09 witness: it drops `mid` and `outer` and KEEPS `leaf`, because
`leaf` is now a called artifact rather than a spliced body — which is the point
of §3.

## 5. Found while closing this, and fixed with it

Both were produced by fixture variants built to test the diagnosis in §2, and
both are reachable from ordinary Java. Both are about the compiled-frame
snapshot, which is what this page's family of defects is about, so both are
fixed here rather than filed:

* **A discarded throwable took the snapshot with it.** The two doors that route
  an implicit signal into a compiled callee's own exception table materialise
  the throwable, and when the table does not match they discard it and restash
  only the FLAG. The snapshot went with the discarded throwable, so the
  caller-side drain rebuilt the exception from nothing: `len=1 [main]` where
  HotSpot reads five. Reproduced with a `try/catch` on the probe's throw site,
  which is what makes `leaf` a callee with a non-matching table. Both doors now
  put it back (`restash_jit_pending_trap_frames`).
* **A snapshot spliced onto a trace that already held it printed frames twice.**
  `append_snapshotted_compiled_frames`'s premise is that the compiled frames
  have unwound by construction time; `materialize_implicit_signal` breaks it.
  `trailing_overlap` now appends only the part the capture did not already see,
  and zero overlap — every shape where the frames HAD unwound — is byte-for-byte
  the previous behaviour.

Both are described from the message side in
`the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md` §4, and
both are pinned by `vm/tests/stack_trace_compiled_callee.rs`, which is the file
whose subject the snapshot is.

A third, in the HARNESS rather than the VM: this file's own `run_arm` used the
poll-`try_wait`-then-`wait_with_output` shape, which reads neither pipe until
the child has exited. Every arm here sets `CRATONVM_DBG_JITC=1`; a pipe holds
tens of KiB; a child that writes more blocks in `write` and can never exit, so
the parent polls out its whole cap and reports a hang that is not one. It had
not bitten this file only because this probe's JIT log is small enough — a
property of diagnostics someone else owns. It bit the NEW test immediately
(0.8 s by hand, "timed out after 900 s" under that shape), which is how it was
found. Both now use `common::wait_draining`, whose own doc is about exactly
this and cost someone else a session first.

## 6. Reproducing

```
JAVA_HOME=<jdk25> PATH=<jdk25>/bin:$PATH \
CRATONVM_BIN=target/release/cratonvm CRATONVM_REQUIRE_E2E=1 \
cargo test -p cratonvm-vm --test stack_trace_across_tiers
```

`CRATONVM_REQUIRE_E2E=1` is the arm that mattered: without it this target passed
for two days with half its assertions inert. A missing `javac` still turns e2e
tests into a bare return that reports `ok` in 0.00 s, so put a JDK's `bin` on
`PATH` and check the clock.
