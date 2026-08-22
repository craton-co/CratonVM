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
two records that cite it still resolve.

## The failure

`org.bouncycastle.pqc.math.ntru.test.PolynomialTest` under
`-XX:+UseGenerationalGC --Xmx 1g`:

```text
testSqToBytes(...PolynomialTest)
java.util.IllegalFormatConversionException: d != java.lang.Object
```

The test's assertion messages are built eagerly, once per compared element:

```java
assertEquals(String.format("count = %d, i = %d", count, j), value.byteValue(), packed[j++]);
```

so a 1230-element vector runs 1230 `String.format` calls, each boxing two
`int`s into a fresh `Object[]`. That allocation rate is the whole of the
workload's relationship to this bug.

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

`fmt_symbols_for` runs `java.text.DecimalFormatSymbols.getInstance()` — a full
Java call that allocates. Nothing rooted the array, so a young collection during
it reclaimed the array and its boxes. `arg`, read one line earlier, then named a
zeroed cell.

Two consequences worth stating separately, because each was mistaken for
something else during the chase:

* **The victim's header is all zeros, not a forwarding pointer.** The object was
  SWEPT, not moved. That is why every relocation-side instrument stayed silent.
* **`%d` is what makes it frequent.** Only `d/f/e/E/g/G` resolve `FmtSymbols`,
  so only those conversions run the heavy nested Java call between reading an
  argument and using it. A `%s`-only format string is far less exposed.

## The measurement that names it

`CRATONVM_DBG_FMT_WRONGTYPE=1` dumps the argument's header at the exact site
that raises the refusal (`native-builtins/src/lang_string.rs`, the `!applicable`
arm). On a failing run:

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

`CRATONVM_DBG_HEAP_STALE=1` on the same runs reported **no heap object at all**
pointing at that address — consistent with the array having been reclaimed
alongside the box, and inconsistent with a live referrer whose field went
un-forwarded.

## The controls that ruled out the filed diagnosis

One run per cell per round, three rounds, interleaved so host load lands on all
of them:

| arm | result |
|---|---|
| `--nojit`, generational, 1g | PASS, **SIG**, **SIG** |
| `--Xmx 8g`, generational, JIT on | PASS, PASS, PASS |
| ZGC (default), 1g, JIT on | PASS, PASS, PASS |

**`--nojit` reproduces**, at a HIGHER rate than with the JIT on. That single row
retires the JIT reading: with no compiled code there are no compiled frames, no
oop maps and no conservative JIT scan to lose a root in.

`--Xmx 8g` passing keeps the other half of the diagnosis: the failure needs a
collection to actually happen.

Then, `--nojit` as the workbench, ABBA-interleaved, one binary:

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
// `fmt_symbols_for` above may have run `DecimalFormatSymbols.getInstance()`
// — real bytecode, real allocation, so a collection can have happened since
// `arg` was read. The array is pinned, so the element is still LIVE; re-read
// it through the handle so a moving collection's new address is used rather
// than the copy taken before the call.
let arg = match (arr_ref, arr_pin) {
    (Some(a), Some(h)) if use_idx < arr_len => {
        let a = ctx.read_native_pin(h, a);
        ctx.get_array_element(a, use_idx)
    }
    _ => arg,
};
```

The pin keeps the array (and therefore every box still in it) LIVE; the
re-derivation keeps the address CURRENT under a moving collection. Both halves
are needed and they fix different things.

The split into two functions is not stylistic: `format_impl` has a dozen early
`return Err(fmt_raise(...))` paths, and a pin batch that is not released on one
of them is a permanent retention leak. The same idiom is already used by
`fmt_format_to` further up the file.

`CRATONVM_NO_FORMAT_ARG_PIN=1` restores the unpinned formatter, so both arms are
one binary apart.

## Verification

ABBA-interleaved, one binary, `-XX:+UseGenerationalGC --Xmx 1g`:

| configuration | fix ON | fix OFF (`CRATONVM_NO_FORMAT_ARG_PIN=1`) |
|---|---|---|
| `--nojit` | **PASS 6/6** | **SIG 6/6** |
| JIT on | **PASS 8/8** | SIG 5/8, PASS 3/8 |

Fourteen runs with the fix, zero failures; fourteen without it, eleven failures.
The two configurations also show why an absolute verdict was never going to work
here: the same defect reproduces at ~100 % interpreted and ~60 % compiled.

Breadth, same binary, generational, `--Xmx 1g` — the first ten classes of
`bcjava-pass-list.txt` plus the fixture:

```text
11 classes, 11 PASS
```

Three collectors on the fixture, fix in: G1 PASS 3/3, ZGC PASS 3/3, generational
PASS 3/3 (the JIT-on arm above).

Unit tests: `cratonvm-gc --lib` 1687 passed / 0 failed; `cratonvm-native-builtins
--lib` 4148 / 0; `cratonvm-types` (all targets, which is where the flag
declaration guard, the surface fixture and the generated-doc checks live) 583 + 20
/ 0. `cratonvm-vm --lib` is 2600 passed / **1 failed**, and the failure is
`native_override::enforcement_dial_door_tests::every_force_native_file_asks_the_dial_or_is_exempt`
("FORCE_SITES_EXEMPT still names `dispatch_virtual.rs`, but it NOW CONSULTS THE
DIAL") — a pre-existing red on `dev` in files this change does not touch
(`git diff --name-only` lists eight files; neither `native_override.rs` nor
`dispatch_virtual.rs` is among them).

## What the filing got wrong, and why it looked right

**1. "The signature is a lost JIT reference."** It is the signature of an
all-zero header, and that is what a reclaimed cell reads as on ANY path. The
sibling record `bug-g1-evacuates-live-jit-reference-20260819.md` really was a
lost JIT reference with the same message, on the same test, which is what made
the reading feel settled. Same symptom, different mechanism — the shared symptom
is a lead, not an identity, and this page said so and then did not act on it.

**2. `CRATONVM_MOVING_YOUNG_NO_JIT=1` passed 6/6 against a 3/6 default, and that
was noise.** It is the strongest-looking measurement taken during the chase and
it pointed at the wrong subsystem. The later `--nojit` pair — where the same
"never move" policy fails 4/4 — is what refutes it. At a ~50 % per-run rate,
6 clean runs is a 1.6 % coincidence: unlikely, and unlikely happens. The lesson
is not "take more reps" (the interleaved A/B above is 8 reps and would have said
the same thing); it is that a lever which changes a whole subsystem's behaviour
is not a diagnosis until something ties the subsystem to the failure.

**3. The regression window and both void bisects are moot.** The defect is not
in the window `684f37e14..cae49a85c`; `format_impl`'s unpinned locals predate it.
What varies with the tree — and with the host's load, and with `--nojit` — is
how often a young collection lands inside `DecimalFormatSymbols.getInstance()`,
which is why an absolute per-commit verdict was measuring the box. A third
bisect would have failed the same way.

## A separate finding, recorded elsewhere

While the JIT reading was still live, a post-remap instrument
(`CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1`, added here and kept) found a real,
unrelated gap: after a moving young collection has rewritten every oop-map slot,
a word in a compiled frame's **callee-saved GPR image** could still name a
moved-from address, and the frame-band verifier deliberately does not inspect
that region. It correlated with a failing run once, which is how it survived as
a hypothesis for a while; the correlation did not hold and closing it
(`CRATONVM_REGISTER_IMAGE_REMAP=1`, default off) does not change the failure
rate — measured 4 SIG / 8 with it on against 5 SIG / 8 with it off. It is a
genuine hole with no failure attributed to it, and it is written up separately
in `known-issues/gc/moving-young-leaves-a-callee-saved-register-image-unrewritten-20260822.md`.

## Residuals

* **The same pattern elsewhere in the formatter.** The `%t` date/time paths chain
  `ctx.invoke_virtual` results (`getTimeZone` → `getOffset`, `getZone` →
  `getId`) through unpinned Rust locals. `fmt_zone_id` already pins; the others
  do not. No failure is attributed to them — this workload never reaches a `%t`
  conversion — and they are the same bug class as the wildfly
  stale-`ObjectRef` family, not this record's subject.
* **The formatter's argument is still held across `format_arg_full`.** Pinning
  the array keeps it live, which is what this defect needed; a moving collection
  inside `format_arg_full`'s own `hashCode`/`toString` dispatch would still
  leave the local naming a pre-move address. Not observed, and closing it means
  threading a handle through that function's signature.

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

`--nojit` is the high-rate arm (6/6 without the fix) and the cheap one to
bisect against; add `CRATONVM_NO_FORMAT_ARG_PIN=1` to a fixed binary to get the
failure back.

## Relationship to the other records

* `bug-g1-evacuates-live-jit-reference-20260819.md` — the same message on the
  same test, and a genuinely lost JIT root. Fixed separately; unrelated
  mechanism.
* `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md` — its
  §"The generational failure is NOT this bug" A/B was right, and its request to
  re-check this page's determinism claim is answered here: intermittent,
  ~60 % compiled and ~100 % interpreted, and load-sensitive only because load
  changes when collections land.

---

## Appendix: the original filing, superseded

Kept verbatim below because the way this investigation went wrong is
part of the record. Everything in it that reads as a finding about the
JIT, about relocation, or about a regression window is superseded by the
sections above; the rate measurements and the two void bisects are still
accurate accounts of what was observed.

### The ntru unpinned-JIT-reference failure now reproduces on GENERATIONAL

## What is failing

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

## This is not the G1 branch's doing

Measured with the branch that fixed the G1 case applied, and without it, three
runs each, same host, same fixture:

| build | generational |
|---|---|
| pristine `dev@cae49a85c` | FAIL 3/3 |
| `dev@cae49a85c` + the G1 root-coverage branch | FAIL 3/3 |

Identical. The G1 fixes neither cause nor address it. (Under G1 on the same
tips: pristine FAIL 2/2, branch PASS 2/2 — that half is fixed and landed.)

## The regression window, stated honestly

The window is **`684f37e14..cae49a85c`**, and it is wider than it first looked.

What was actually measured passing under generational was a binary built from
`684f37e14` (my branch point, pristine) — `PASS`, with the collector reporting
`incomplete=true` on all 8 collections, so it never took the precise-only
suppression at all. **Pristine `dev@8d5e26cf9` was never run under
generational**, so the tempting narrower claim ("it broke in dev's last 41
commits") is not supported by anything measured. Do not repeat it without
bisecting.

That leaves roughly 240 commits in the window.

## The bisect was attempted and is VOID

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

## The rate, measured — and it IS a regression

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

## The second bisect completed, and its answer did not survive confirmation

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

## What the evidence actually supports now

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

The rate tracks **when the runs happened** at least as strongly as **which
commit** was built. That is the property that makes an ordinary bisect
unusable here: an absolute per-commit verdict is measuring the box as much as
the code.

The one measurement that controls for it is the interleaved endpoint pair —
7/10 against 0/10, same machine, alternating runs. That remains the only
evidence that any commit difference exists at all, and it is a single
comparison. **It is being re-run; until it reproduces, treat "there is a
regression in this window" as unconfirmed.**

## What a workable method looks like

If the interleaved result does reproduce, a bisect is still possible but each
step must be an **interleaved A/B against a fixed reference build**, not an
absolute verdict: run candidate and reference alternately in the same window and
compare their rates. That costs roughly twice as much per step and needs enough
reps to separate two rates rather than to observe one event — but it is the only
form that survives an environment-sensitive failure.

Absolute-verdict bisects have now been attempted twice, at 2 and 6 repetitions,
and both produced confident answers that confirmation destroyed. A third at
higher repetition would most likely do the same.

## What the failing run shows

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

## Repro

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

## Not claimed

* Not that it is the same root cause as the G1 bug. Same *signature*, different
  collector, different protection mechanism (generational protects by not moving,
  G1 by pinning). Treat the shared symptom as a lead, not an identity.
* Not that any particular commit introduced it. The window is ~240 commits and
  nothing has been bisected.
