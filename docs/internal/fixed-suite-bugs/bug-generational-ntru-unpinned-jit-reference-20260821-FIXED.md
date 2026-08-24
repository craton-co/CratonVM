# The ntru `d != java.lang.Object` was `String.format` reclaiming its own arguments

## Status

**FIXED 2026-08-22.** `String.format`'s native held the format string, the
varargs array and the fetched argument as raw unpinned `ObjectRef`s across
`DecimalFormatSymbols.getInstance()` — real bytecode, real allocation. A young
collection there reclaimed the varargs array together with every box still
inside it, and the next conversion read an all-zero header at the argument's
address: `ClassId(0)`, which the class manager names `java.lang.Object`.

Filed as a lost JIT root (this file's name). **It is not a JIT defect and not a
relocation defect** — see "What the filing got wrong". The name is kept so the
records that cite it still resolve.

## The failure

`org.bouncycastle.pqc.math.ntru.test.PolynomialTest` under
`-XX:+UseGenerationalGC --Xmx 1g`:

```text
testSqToBytes(...PolynomialTest)
java.util.IllegalFormatConversionException: d != java.lang.Object
```

The test builds its assertion message eagerly, once per compared element:

```java
assertEquals(String.format("count = %d, i = %d", count, j), value.byteValue(), packed[j++]);
```

so a 1230-element vector runs 1230 `String.format` calls, each boxing two
`int`s into a fresh `Object[]`. That rate is the whole of the workload's
relationship to this bug.

## What it actually is

`native-builtins/src/lang_string.rs`, `format_impl` — the body of every
`String.format` / `Formatter.format` overload. It receives `args[0]` = the
format string and `args[1]` = the varargs `Object[]`, both as raw `ObjectRef`s,
and holds them for the whole call. Inside the conversion loop:

```rust
let arg = /* ctx.get_array_element(a, use_idx) */;
let sym = if matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G') {
    match symbols {
        Some(s) => s,
        None => { let s = fmt_symbols_for(ctx, locale); symbols = Some(s); s }
    }
} else { FmtSymbols::default() };
let text = format_arg_full(ctx, &arg, spec, ...)?;
```

`fmt_symbols_for` runs `java.text.DecimalFormatSymbols.getInstance()` followed by
four `invoke_virtual` reads off the returned object — a full Java call sequence,
with no process-wide cache, **once per `String.format` that has a numeric
conversion**. Nothing rooted the array across it, so a young collection there
reclaimed the array and its boxes, and `arg` — read one line earlier — named a
zeroed cell.

Two consequences worth stating separately, because each was mistaken for
something else during the chase:

* **The victim's header is all zeros, not a forwarding pointer.** The object was
  SWEPT, not moved. That is why every relocation-side instrument stayed silent,
  including `CRATONVM_DBG_STALE_OBJREF`, whose canary fires only on a FORWARDED
  header.
* **`%d` is what makes it frequent.** Only `d/f/e/E/g/G` resolve `FmtSymbols`, so
  only those conversions run the heavy nested Java call between reading an
  argument and using it.

## The measurement that names it

`CRATONVM_DBG_FMT_WRONGTYPE=1` dumps the argument's header at the exact site
that raises the refusal (`lang_string.rs`, the `!applicable` arm). On a failing
run:

```text
[fmt-wrongtype] spec=d obj=0x200424298b0 cid=0 cname="java/lang/Object"
                kind=Object w0=0x0 w1=0x0 w2=0x0 w3=0x0
```

Four zero words. `ClassId(0)` is `java.lang.Object` because it is the first
class this VM loads, and an all-zero header is what the collector leaves over a
reclaimed span — so the message names a class that was never involved. The dump
is what separates the three states the message cannot: reclaimed cell, stale
reference to a moved object (forwarded header), and an argument that really is
of the wrong class.

`CRATONVM_DBG_HEAP_STALE=1` on the same runs found **no heap object at all**
pointing at that address — consistent with the array having been reclaimed
alongside the box, and inconsistent with a live referrer whose field went
un-forwarded.

## The controls that retired the filed diagnosis

One run per cell per round, three rounds, interleaved:

| arm | result |
|---|---|
| `--nojit`, generational, 1g | PASS, **SIG**, **SIG** |
| `--Xmx 8g`, generational, JIT on | PASS, PASS, PASS |
| ZGC (default), 1g, JIT on | PASS, PASS, PASS |

**`--nojit` reproduces**, at a HIGHER rate than with the JIT on. That single row
retires the JIT reading: with no compiled code there are no compiled frames, no
oop maps and no conservative JIT scan to lose a root in. `--Xmx 8g` passing
keeps the other half: the failure needs a collection to actually happen.

Then, `--nojit` as the workbench, one binary:

| arm | SIG | PASS |
|---|---:|---:|
| default | 5 | 0 |
| `CRATONVM_NO_MOVING_YOUNG=1` | 4 | 0 |

**Turning the moving young generation off does not fix it.** So it is not a
relocation defect either: a non-moving sweep reclaims the object just the same.

## The fix

`format_impl` is split in two. The outer half pins the format string and the
varargs array and releases the batch on every return path; the inner half is the
original body, with the array re-derived from its pin handle before every
element read:

```rust
let pin_base = ctx.pin_native_root(fmt_obj);
let arr_pin = arr_obj.map(|a| ctx.pin_native_root(a));
let out = format_impl_pinned(ctx, args, locale, arr_pin);
ctx.unpin_native_roots(pin_base);
```

and, after the symbols lookup, in both the general-conversion and the `%t` arms:

```rust
let arg = match (arr_ref, arr_pin) {
    (Some(a), Some(h)) if use_idx < arr_len => {
        let a = ctx.read_native_pin(h, a);
        ctx.get_array_element(a, use_idx)
    }
    _ => arg,
};
```

The pin keeps the array — and therefore every box still in it — LIVE; the
re-derivation keeps the address CURRENT under a moving collection. They fix
different things and both are needed.

`fmt_symbols_for` gets the same treatment one level down: it held the
`DecimalFormatSymbols` instance across four `invoke_virtual` reads, so reads two
through four were issued against the address the first one saw. The receiver is
now pinned and re-derived per read. Nothing was measured to fail on it — it is
the same defect, in the same call, found while reading the call that did.

The split into two functions is not stylistic: `format_impl` has a dozen early
`return Err(fmt_raise(...))` paths, and a pin batch not released on one of them
is a permanent retention leak. The same idiom is already used by `fmt_format_to`
further up the file.

`CRATONVM_NO_FORMAT_ARG_PIN=1` restores the unpinned formatter, so both arms are
one binary apart.

## The contract was already written down, one function away

`fmt_zone_display_name`, in the same file, opens with:

> This call runs Java bytecode (`fmt_date_name` alone invokes three methods for
> `%tc`), and a held reference across that is a moved-object hazard. `val` is
> the caller's own varargs element, **so it is rooted for the whole
> conversion.**

The hazard is named exactly right and the conclusion was false: nothing rooted
the varargs element, because nothing rooted the array it came out of. The
comment is not wrong now — the pin above is what makes its last clause true —
but it was a statement about code that did not exist, sitting one screen from
the loop that needed it, and reading it is part of why the formatter looked
already audited on the first pass over this file.

## Verification

ABBA-interleaved, one binary, `-XX:+UseGenerationalGC --Xmx 1g`:

| configuration | fix ON | fix OFF (`CRATONVM_NO_FORMAT_ARG_PIN=1`) |
|---|---|---|
| `--nojit` | **PASS 6/6** | **SIG 6/6** |
| JIT on | **PASS 8/8** | SIG 5/8, PASS 3/8 |
| JIT on, after merging 118 dev commits | **PASS 5/5** | **SIG 5/5** |

Nineteen runs with the fix and no failures; nineteen without it and sixteen
failures. The two configurations also show why an absolute verdict was never
going to work here: the same defect reproduces at ~100 % interpreted and ~65 %
compiled.

### The EXPOSURE question is answered, and item 2 above needs correcting (2026-08-24)

This page says the regression is real and that it does not explain it. It is
explained: **moving young collections are what expose the defect, and the window
reduced how often the collector declines to move.**

Fallback counts (a fallback IS the collector declining to move) are perfectly
deterministic and separate perfectly with outcome:

```text
cae49a85c   5 fallbacks/run   SIG  6/6
684f37e14   7 fallbacks/run   PASS 6/6
```

Two collections that stayed non-moving on the good endpoint stopped doing so on
the bad one. Forcing the decline back on, in the bad binary, 10 reps interleaved
against its own control:

```text
default                      SIG 8/10   (6 fallbacks)
CRATONVM_MOVING_YOUNG_NO_JIT=1   SIG 0/10   (8 fallbacks)
```

At an 80 % baseline, 0/10 is ~1e-7. The lever raises the decline count and the
failure disappears with it.

**Item 2's dismissal of this same lever is wrong, and the reasoning is the
instructive part.** It reads `--nojit` as "the same never-move policy" and takes
its 4/4 failure as a refutation. But `--nojit` does not force never-move — it
removes JIT frames, and it is the PRESENCE of a JIT frame that makes this
collector decline to move. With no JIT frames the young collector is *free* to
move, so `--nojit` is the MOVING arm. Its 4/4 failure is evidence FOR
"moving exposes it", not against, and every measurement now agrees:

| arm | moves? | result |
|---|---|---|
| `--nojit` (no JIT frames) | yes, freely | SIG 4/4 |
| default, JIT on | sometimes (5-6 declines) | SIG 8/10 |
| `MOVING_YOUNG_NO_JIT=1` | no (8 declines) | **SIG 0/10** |

The original 6/6 was also not noise: at these rates it is well under 1 %. It was
discarded for a stated reason that does not hold, which is a harder failure to
catch than a coincidence — see
`a-guard-scoped-by-a-stated-premise-is-only-as-good-as-the-premise`.

None of this changes the FIX, which is correct and is the right layer: pinning
`format_impl`'s locals makes the defect immune to whether the collector moves.
It changes the open question at the bottom of this page from "why did exposure
change" to "which commit in the window reduced the decline count" — a much
narrower search, over a deterministic 7-to-5 signal rather than a 70 % coin.

### Independent confirmation on the shipped tip (2026-08-23)

From the other line of work, arrived at without knowing this fix existed —
cross-commit rather than in-binary, so it corroborates from a different angle:

```text
dev c057a3a78   SIG 0/35
old cae49a85c   SIG 19/25   <- positive control, 76 %, interleaved, same loop
```

Thirty-five clean runs on the tip against a control the harness demonstrably
still sees. The control is the load-bearing half: without it "0/35" and "the
harness stopped working" are the same output, and that failure mode had already
cost three experiments that week. Bounds the residual below ~8 % (95 %) on the
shipped tip, which is what the `PASS 5/5` row above establishes at a smaller
sample.

Breadth, same binary, generational, `--Xmx 1g` — the first ten classes of
`bcjava-pass-list.txt` plus the fixture: **11 classes, 11 PASS**. Three
collectors on the fixture: G1 3/3, ZGC 3/3, generational as above.

Unit tests on the merged tree, all green: `cratonvm-gc --lib` 1687 / 0,
`cratonvm-native-builtins --lib` 4160 / 0, `cratonvm-vm --lib` 2601 / 0, and
`cratonvm-types` across all targets — which is where the flag declaration guard,
the surface fixture and the generated-doc checks live, and therefore what pins
the five new flags this change declares.

Before the merge, `cratonvm-vm --lib` was 2600 / **1 failed** on
`native_override::enforcement_dial_door_tests::every_force_native_file_asks_the_dial_or_is_exempt`.
That was a red on `dev` in files this change does not touch, and merging `dev`
cleared it — which is the confirmation, rather than the `git diff --name-only`
argument that stood in for it beforehand.

## What the filing got wrong, and why it looked right

**1. "The signature is a lost JIT reference."** It is the signature of an
all-zero header, and that is what a reclaimed cell reads as on ANY path. The
sibling record `bug-g1-evacuates-live-jit-reference-20260819.md` really was a
lost JIT reference, with the same message on the same test, which is what made
the reading feel settled. Same symptom, different mechanism — this page said the
shared symptom was a lead rather than an identity, and then treated it as one.

**2. `CRATONVM_MOVING_YOUNG_NO_JIT=1` passed 6/6 against a 3/6 default, and that
was noise.** It is the strongest-looking measurement taken during the chase and
it pointed at the wrong subsystem. The later `--nojit` pair — where the same
"never move" policy fails 4/4 — refutes it. At a ~50 % per-run rate, six clean
runs is a 1.6 % coincidence: unlikely, and unlikely happens. The lesson is not
"take more reps"; the verifying A/B above is 8 reps per arm and would have said
the same thing. It is that a lever which switches a whole subsystem off is not a
diagnosis until something ties that subsystem to the failure.

**3. The regression IS real and this record does not explain it.** The window
`684f37e14..cae49a85c` was confirmed by a separate line of work on `dev` —
interleaved endpoints, reproduced twice, 0 SIG in 20 at the good end against 14
in 20 at the bad end (see the appendix). The defect this page fixes predates
that window: `format_impl`'s unpinned locals and `fmt_symbols_for`'s nested Java
call both landed 2026-08-11 in `77675ce8c`, an ancestor of BOTH endpoints. So
something inside the window changed the EXPOSURE, not the defect.

The leading candidate was that the array had been accidentally covered by the
conservative sweep of `[scanner_sp, frame_base)` — the Rust and interpreter
frames a compiled method calls INTO — which `scan_compiled_frame_bands` narrows
away, and whose own module comment already warns that the narrowing "does not
hold for an object that has been allocated and not yet stored anywhere tracked".
`CRATONVM_JIT_NO_FRAME_BANDS=1` restores the whole-band sweep, so it is a
one-binary A/B. **It is not the explanation:**

```text
bounded bands (default)      SIG 18 / 20
CRATONVM_JIT_NO_FRAME_BANDS  SIG 15 / 20
```

The first six reps of that pair read 5/6 against 2/6 and looked like a finding.
Fourteen more reps per arm collapsed it to nothing (Fisher p ≈ 0.66). Recorded
because the six-rep version was written down as a result before the twenty-rep
version existed, which is the same mistake as item 2 in the same session.

So the window is left unexplained. It is no longer load-bearing — the mechanism
is known and the repair is verified against it directly — but "some commit in
that window made an already-broken formatter start failing" is a true statement
this page cannot complete, and a reader should not take the fix as having
answered it.

## A separate finding, recorded elsewhere

While the JIT reading was still live, a post-remap instrument
(`CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1`, added here and kept) found a real,
unrelated gap: after a moving young collection has rewritten every oop-map slot,
a word in a compiled frame's **callee-saved GPR image** can still name a
moved-from address, because `band_slot_is_verifiable` deliberately does not
inspect that region and `remap_active_jit_frames` does not rewrite it. It
correlated with a failing run once, which is how it survived as a hypothesis;
the correlation did not hold, and closing it
(`CRATONVM_REGISTER_IMAGE_REMAP=1`, default off) does not change the failure
rate — 4 SIG / 8 with it on against 5 SIG / 8 with it off. Written up in
`moving-young-left-a-callee-saved-register-image-unrewritten-FIXED-20260823.md`
(fixed 2026-08-23; that page superseded the open one cited here).

## Residuals

* **The same pattern elsewhere in the formatter.** The `%t` date/time paths chain
  `ctx.invoke_virtual` results through unpinned Rust locals —
  `fmt_resolve_zone` holds `tz` across `getOffset` and then calls
  `getRawOffset` on it, which is the identical shape to the `dfs` reads fixed
  here. `fmt_zone_id` already pins; the others do not. No failure is attributed
  to them: this workload never reaches a `%t` conversion, so hardening them
  would be an unmeasured change riding on a measured one.
* **The argument is still held across `format_arg_full`.** Pinning the array
  keeps it live, which is what this defect needed; a moving collection inside
  `format_arg_full`'s own `hashCode`/`toString` dispatch would still leave the
  local naming a pre-move address. Not observed, and closing it means threading
  a handle through that function's signature.
* **The window, per item 3 above.**

## Reproducing

```bash
cd /data/cratonvm/apps/bc-java
CRATONVM_DBG_FMT_WRONGTYPE=1 cratonvm --java-home /data/toolchain/jdk-25 \
    -XX:+UseGenerationalGC --Xmx 1g --nojit \
    -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
    -Dtest.java.version.prefix=25 \
    -c "$(cat /data/bcjca-classpath.txt)" \
    junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest
```

`--nojit` is the high-rate arm (6/6 without the fix) and the cheap one to work
against; add `CRATONVM_NO_FORMAT_ARG_PIN=1` to a fixed binary to get the failure
back.

## Relationship to the other records

* `bug-g1-evacuates-live-jit-reference-20260819.md` — the same message on the
  same test, and a genuinely lost JIT root. Fixed separately; unrelated
  mechanism.
* `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md` — its
  §"The generational failure is NOT this bug" A/B was right, and its request to
  re-check this page's determinism claim is answered here: intermittent, ~65 %
  compiled and ~100 % interpreted.

---

## Appendix: the original filing, superseded

Kept verbatim below, including the two rounds of corrections it received
on `dev` while this investigation ran, because the way the chase went
wrong is part of the record. Everything in it that reads as a finding
about the JIT or about relocation is superseded by the sections above;
its rate measurements, its two void bisects and its confirmation of the
regression window are still accurate accounts of what was observed, and
the window is the one question this page closes without answering.

### The ntru unpinned-JIT-reference failure now reproduces on GENERATIONAL

#### What is failing

`org.bouncycastle.pqc.math.ntru.test.PolynomialTest` under
`-XX:+UseGenerationalGC`, on **pristine `dev`**:

```text
gen-devpure-1  FAIL  IllegalFormatConversionException: d != java.lang.Object
gen-devpure-2  FAIL  IllegalFormatConversionException: d != java.lang.Object
gen-devpure-3  FAIL  IllegalFormatConversionException: d != java.lang.Object
```

> **CORRECTION (2026-08-21, later the same day).** This section originally said
> "deterministically". **It is not deterministic — it is intermittent**, and
> that word was wrong. An attempted bisect ran the same commit `cae49a85c` as
> its BAD endpoint and got **PASS 2/2**, and one interior commit reported
> `run1=PASS run2=SIG` in a single step. Three consecutive failures were real
> but did not establish determinism.
>
> The likeliest reading is host load: the 3/3 runs happened while a 53-class
> gate and other jobs were saturating the box; the bisect endpoints ran on a
> quiet one. That matches this host's recorded behaviour, where load flips
> PASS/FAIL and not only timings.
>
> Everything below that depends on the failure being reliable — including the
> regression *window* — is weaker than it reads. See the bisect section.

That is the signature of `bug-g1-evacuates-live-jit-reference-20260819.md` — a
live reference the collector's root set did not contain, read back stale after
the object moved — but on a **different collector** and a different test method
(`testSqToBytes` in one run, `testS3FromBytes` in the G1 case).

#### This is not the G1 branch's doing

Measured with the branch that fixed the G1 case applied, and without it, three
runs each, same host, same fixture:

| build | generational |
|---|---|
| pristine `dev@cae49a85c` | FAIL 3/3 |
| `dev@cae49a85c` + the G1 root-coverage branch | FAIL 3/3 |

Identical. The G1 fixes neither cause nor address it. (Under G1 on the same
tips: pristine FAIL 2/2, branch PASS 2/2 — that half is fixed and landed.)

#### The regression window, stated honestly

The window is **`684f37e14..cae49a85c`**, and it is wider than it first looked.

What was actually measured passing under generational was a binary built from
`684f37e14` (my branch point, pristine) — `PASS`, with the collector reporting
`incomplete=true` on all 8 collections, so it never took the precise-only
suppression at all. **Pristine `dev@8d5e26cf9` was never run under
generational**, so the tempting narrower claim ("it broke in dev's last 41
commits") is not supported by anything measured. Do not repeat it without
bisecting.

That leaves roughly 240 commits in the window.

#### The bisect was attempted and is VOID

`git bisect run` over `684f37e14..cae49a85c`, probe = build + run, matching the
exception signature rather than the exit code, two passes required for "good":

```text
BAD  end (cae49a85c)  exit=0   <- the endpoint should have been BAD
GOOD end (684f37e14)  exit=0
78606e45d  BAD  (run1=PASS run2=SIG - flaky)
…                                   walked to 819ad679a
```

**The BAD endpoint passed, so the run proves nothing** and `819ad679a` is not a
result. With an intermittent failure a two-run pass cannot establish "good" —
any commit can pass twice by chance — so every GOOD verdict in that trace is
unsound, and the bisect walked a tree of unreliable answers to a confident
conclusion. Do not cite it.

#### The rate, measured — and it IS a regression

Ten runs per endpoint, interleaved so host load lands on both equally:

```text
bad  cae49a85c   SIG=7  PASS=3     ~70% per run
good 684f37e14   SIG=0  PASS=10    0/10, never reproduced
```

Two things follow.

**The regression is real.** The good end is clean in ten runs, so this is not a
long-standing intermittent defect that was always present — something inside
`684f37e14..cae49a85c` introduced it. That was the question that decided whether
bisecting is meaningful at all.

**The void bisect is explained arithmetically, not vaguely.** At 70% per run,
two consecutive passes occur 0.3² = **9%** of the time. The first probe required
exactly two passes to call a commit good, so it carried a 9% chance of
mislabelling *any* bad commit — and it spent that on the endpoint check. Nothing
about the host or the tooling was wrong; the probe was simply under-powered for
the rate, and the rate had not been measured.

The re-run uses **six** consecutive clean passes for GOOD (0.3⁶ ≈ 0.07% per
step, ~0.6% over eight steps) and exits on the first signature, so bad commits
stay cheap and only genuinely good ones pay the full six.

A GOOD verdict still means "did not reproduce in six", not proof: if the failure
rate collapses near the introduction point, six passes buys less than that
arithmetic suggests. Whatever commit the search names should be confirmed by
re-running it and its parent directly, rather than trusting the walk.

#### The second bisect completed, and its answer did not survive confirmation

The repetition-aware run named `e40c176d8` — a **merge** — and marked both its
parents GOOD, which would have made this a two-clean-branches-interact defect.
Confirmation, 15 reps per arm, all three interleaved:

```text
merge  e40c176d8   SIG=0  PASS=15
p1     0fbb1df8a   SIG=0  PASS=15
p2     927350e53   SIG=0  PASS=15
```

**45 runs, zero reproductions.** The commit the bisect called BAD does not
reproduce at all. So this search is void too, and `e40c176d8` is not the answer
any more than `819ad679a` was.

Note what the BAD verdict rested on: the probe saw the signature **once**, on
run 5 of 6. That was a real observation, not a bug in the probe — and it is not
reproducible fifteen runs later.

#### What the evidence actually supports now

Collecting every measurement of this failure, in the order taken:

| commit | result | when |
|---|---|---|
| `cae49a85c` | SIG 3/3 | during a saturating 53-class gate |
| `cae49a85c` | SIG 0/2 | bisect #1 endpoint, quiet box |
| `cae49a85c` | **SIG 7/10** | **interleaved against `684f37e14`** |
| `684f37e14` | **SIG 0/10** | **same interleave** |
| `e40c176d8` | SIG 1/5 | bisect #2 |
| `e40c176d8` | SIG 0/15 | confirmation, interleaved |
| `0fbb1df8a`, `927350e53` | SIG 0/15 each | same interleave |

**An earlier version of this record read that table as "the rate tracks WHEN the
run happened". That was wrong**, and a knob experiment plus pooling disproves it
— see the two sections below. The rate tracks the COMMIT. The apparent
time-correlation was small-sample noise being over-read.

The one measurement that controls for it is the interleaved endpoint pair, and
**it reproduced exactly**:

```text
run 1   bad cae49a85c  SIG=7 PASS=3      good 684f37e14  SIG=0 PASS=10
run 2   bad cae49a85c  SIG=7 PASS=3      good 684f37e14  SIG=0 PASS=10
```

Twenty runs at the good end with zero failures against twenty at the bad end
with fourteen. **The regression is confirmed.** An interleaved A/B is a reliable
instrument here even though an absolute verdict is not — the alternation cancels
whatever the environment is contributing.

### The rate is not uniform across the window, and that matters

`e40c176d8` showed the signature once in six runs, then zero in fifteen. If good
commits never fail (`684f37e14` is 0/20), a single signature there cannot be
noise — it means `e40c176d8` is already bad, but at a **much lower rate** than
`cae49a85c`'s 70%. One in twenty-one is consistent with roughly 5%.

So the rate appears to *rise* across the window rather than switch on. That has a
sharp consequence: **"the first bad commit" may not be a well-formed question
here.** Either several commits each widen the race, or one introduces it and
later ones amplify it. A binary search assumes a step function and there may not
be one.

It also prices the search honestly. Detecting a 5% rate with confidence needs on
the order of 60 reps per step, not 6 or 15 — and near the introduction point
that is exactly the rate a bisect would face.

#### No knob makes it deterministic

Same binary (`cae49a85c`) across five arms, 5 reps each, to see whether anything
removes the variance. Heap pressure was the leading candidate — a smaller heap
means more young collections and more chances at the race — and CPU contention
was the other, because the rate *looked* load-correlated.

```text
baseline-1g    SIG=4 PASS=1 OTHER=0
heap-512m      SIG=3 PASS=2 OTHER=0
heap-256m      SIG=4 PASS=1 OTHER=0
heap-128m      SIG=2 PASS=3 OTHER=0
busy6-1g       SIG=4 PASS=0 OTHER=1
```

Nothing moves. Every arm sits in 40-80%, and at n=5 those are indistinguishable
(the 95% interval for 4/5 is roughly 28-99%). **Only a large effect is excluded**
— a knob that moved 70% to 95% would not be visible at this sample size — but
none of heap size from 1 g down to 128 m, nor six busy cores, does anything
detectable.

The baseline arm reproduced (4/5) before the others ran, so the instrument was
working; the script aborts if it does not, precisely so an unreadable run cannot
present as five tidy rows of zeros.

#### Correction: the failure is NOT environment-sensitive

Pooling every measurement ever taken at `cae49a85c`:

```text
3/3   0/2   7/10   7/10   4/5   4/5        = 21 SIG in 30 runs = 70%
```

That is a **stable ~70% Bernoulli**, not a rate that moves with the box. The one
result that drove the whole "environment-sensitive" reading — the bisect
endpoint's 0/2 — is simply the 9% case for a 70% coin, exactly as predicted. No
load explanation was ever needed for it. And `busy6` at 80% is the direct test:
saturating the CPU does not change the rate.

So the diagnosis of why two bisects died changes. It was **not** the environment.
It was a **rate gradient across the window** — ~70% at `cae49a85c`, ~5% at
`e40c176d8` — met with probes powered for neither. A 2-rep probe misreads a 70%
commit 9% of the time; a 6-rep probe misreads a 5% commit 74% of the time.

#### What a workable method looks like

If the interleaved result does reproduce, a bisect is still possible but each
step must be an **interleaved A/B against a fixed reference build**, not an
absolute verdict: run candidate and reference alternately in the same window and
compare their rates. That costs roughly twice as much per step and needs enough
reps to separate two rates rather than to observe one event — but it is the only
form that survives an environment-sensitive failure.

Absolute-verdict bisects have now been attempted twice, at 2 and 6 repetitions,
and both produced confident answers that confirmation destroyed. A third of that
KIND would do the same — but an interleaved A/B bisect is a different instrument,
and the endpoint pair reproducing twice is evidence it works.

Interleaving is still good practice but is **not** the thing that makes it work,
since there is no environment effect to cancel. What makes it work is
REPETITION MATCHED TO THE LOCAL RATE, and near the introduction point that rate
is ~5%: separating 5% from 0% with confidence needs on the order of 60 runs per
step, about 2 hours per step and ~16 hours for the search.

Removing the variance was tried and failed (above), so that discount is not
available.

Given the cost, the better target is probably the MECHANISM rather than the
commit. The failing run logs six `[moving-young]` fallbacks to the non-moving
sweep, which relocates nothing — so either a cycle that did not fall back is the
one that corrupts, or the damage is not a relocation at all.
`CRATONVM_DBG_JIT_ROOTSCAN=1` prints one line per collection, and a single
failing run under it answers which. That is one ~2-minute run with a 70% chance
of landing the failure, against ~16 hours of bisecting.

#### What the failing run shows

The collector is repeatedly *declining* to move, which is the safe direction,
and failing anyway:

```text
[moving-young] fallback #3: reason=unregistered-jit-frame-on-stack
[moving-young] fallback #4: reason=compiled-frame-oop-not-published
[moving-young] fallback #5..#8: reason=compiled-frame-oop-not-published
```

`compiled-frame-oop-not-published` is `incomplete_reason::UNPUBLISHED_FRAME_OOP`.
It is **not** new and not from the G1 branch — it is present in both `684f37e14`
and `dev`, introduced by `ba2c417af` ("the frame-band verifier must not pass
vacuously").

The tension worth chasing: a non-moving sweep does not relocate anything, so a
stale JIT slot should be impossible on those cycles. Either a cycle that did
*not* fall back is the one that corrupts, or the damage is not a relocation at
all. That distinction is the first thing to establish — the fallback log is
evidence about the cycles that were safe, not about the one that was not.

Also present, immediately before the failure:

```text
JIT compile bailed: code buffer estimate too small; retrying at the measured size
  method="…/PolynomialTest.testSqToBytes:()V" code_len=341 capacity=67552 wanted=67957
```

Whether a recompile of the very method that then fails is coincidence or
mechanism is unknown; it is recorded because it is one line above the failure,
not because there is a story for it.

#### Repro

```bash
cd /data/cratonvm/apps/bc-java
cratonvm --java-home /data/toolchain/jdk-25 -XX:+UseGenerationalGC --Xmx 1g \
    -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
    -Dtest.java.version.prefix=25 \
    -c "$(cat /data/bcjca-classpath.txt)" \
    junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest
```

**Intermittent** — budget many repetitions, not one. ~2 minutes per run. `CRATONVM_DBG_JIT_ROOTSCAN=1` prints one line per
collection (`precise_only` / `incomplete` / `scan_added`), which is what
distinguishes "the scan was skipped" from "the scan ran and found nothing" — the
distinction that took the G1 case three wrong hypotheses to get right.

#### Not claimed

* Not that it is the same root cause as the G1 bug. Same *signature*, different
  collector, different protection mechanism (generational protects by not moving,
  G1 by pinning). Treat the shared symptom as a lead, not an identity.
* Not that any particular commit introduced it. The window is ~240 commits and
  nothing has been bisected.
