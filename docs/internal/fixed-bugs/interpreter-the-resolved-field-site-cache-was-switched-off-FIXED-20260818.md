# The resolved-field site cache had been switched off since it landed — FIXED 2026-08-18

**Status:** FIXED. `CRATONVM_JIT_FIELD_SITE_CACHE` is default-ON;
`CRATONVM_JIT_FIELD_SITE_CACHE=0` opts out.

**Reproducer:** `probes/InterpDecodedOpcodeCostProbe.java`.

## What was wrong

The interpreter's per-thread resolved-field site cache — the "resolved constant
pool" built specifically so a `getfield` stops re-deriving its field-owning
class on every access — shipped default-OFF on 2026-08-05 and stayed there.

Nothing was broken. It was simply never turned on, which made it a feature only
reachable by someone who already knew the variable's name.

`site_stats` reports the state without ambiguity. Over 38 million interpreted
`getfield`s:

```
[site-cache] FINAL slots=1024 field: hit=0 miss=0 fill=0 reject_loader=0 | ...
```

`hit=0 miss=0 fill=0` is not a cache that missed. It is a cache never
**consulted** — the counters that would have recorded a miss are themselves
zero. The `site_cache` module docs anticipated exactly this reading: *"an inert
gate shows up here immediately as `hit=0`."*

## How it was found, and what nearly got built instead

This came out of a request to add raw-bytecode fast-path arms for the opcodes
that have none — `getfield`, `putfield`, `getstatic`, `putstatic`, `ldc`, `new`,
`checkcast`, `instanceof`, the switches, and the monitors. Those opcodes reach
the decoded handler in `opcodes.rs`, paying the quickened `resolve(pc)`, a
non-inlined call, a ~200-variant match and the post-call diagnostic checks
before the arm body runs.

The first step was to size that overhead rather than assume it. Marginal ns per
opcode, differenced against an `iadd` control that already HAS an arm,
`--nojit`, baseline `24a5d4528`:

| opcode | ns/op | | opcode | ns/op |
|---|---:|---|---|---:|
| `iadd` (has an arm) | 10.8 | | `getfield` | 471 |
| `ldc` | 90 | | `putfield` | 461 |
| `tableswitch` | 48 | | `getstatic` | 400 |
| `instanceof` | 308 | | `putstatic` | 375 |
| `monitorenter`+`exit` | 444 | | `checkcast` | 821 |

The dispatch overhead an arm removes is ~12 ns, measured separately in the
2026-08-18 back-edge audit. **Against a 471 ns `getfield` an arm is worth about
2.5%** — and the plan on the table was to move ~2,200 lines through the hottest
file in the VM to get it.

The number that mattered was the 471 itself: ~44x an `iadd`. That is not a
dispatch problem, it is a body problem, and asking the site cache's own counter
why produced the `hit=0` above.

## Measurements

Two release binaries, `24a5d4528` (**base**) and the same tree with the default
flipped (**fixed**), run alternately on one host, `--nojit`, three runs each.
Marginal ns per opcode:

| kernel | base | fixed |
|---|---|---|
| `iadd` **(control)** | 7.3 / 9.3 / 10.3 | 7.9 / 8.0 / 10.1 |
| `getfield` | 340 / 340 / 503 | 161 / 137 / 177 |
| `putfield` | 327 / 327 / 475 | 157 / 141 / 146 |
| `getstatic` | 305 / 316 / 436 | 124 / 125 / 126 |
| `putstatic` | 279 / 285 / 409 | 128 / 105 / 131 |

**~2.1-2.5x on the four field opcodes, and the control did not move.** The
control holding still is what makes the rest attributable: this host had other
sessions building throughout (see base run 3, slower across every row), which is
also why the arms are interleaved rather than run in blocks.

This independently reproduces the figures the lever was accepted on in
known-issues/tomcat/!webapp-deploy-annotation-scan-interpreted-226x.md —
1.9-2.7x per pass on the field-saturated probe, and 12.7% off the real BCEL
annotation scan on Azure. The default moved rather than the measurement being
retaken, because the measurement already existed.

## Why it stayed off, as far as the record shows

No stated blocker. The sentence in the Tomcat page that keeps a lever
default-OFF — *"Kept, default-OFF, on the same footing as the other
measured-nothing levers: the waste is real, the payoff is not"* — is about
**`method-site-cache`**, which genuinely measures nothing (it removes a read
lock, a hash probe and three `Arc<str>` clone/drop pairs per invoke, and the
counter proves it fires 900k times without showing). That lever is correctly
still off, and stays off here.

`field-site-cache` is the one the same document calls *"the first lever in this
investigation to move the number outside noise"*. It was left opt-in beside its
neighbour and never separated from it.

## What changed

One function. `field_site_cache_enabled()` now defaults true and reads its
opt-out the way `CRATONVM_JIT_OSR` does (`0`/`false`/`off`/`no`), so the
interpreter's default-ON levers share one kill-switch spelling.

**The correctness argument is untouched.** It lives in `site_cache`'s module
docs — three conditions, each a single atomic load, checked per entry on every
hit and every fill: the class-definition epoch, the resolution epoch, and the
redefinition latch. Any change wipes the entry and the caller falls through to
the authoritative slow path. Nothing here relaxes any of it; only the default
moved.

The two wider arms stay opt-in: `field-site-cache-loader` (which additionally
admits loader-sensitive sites, a wider correctness surface) and
`method-site-cache` (measured nothing).

Declared in the flag surface as `opt-out | on` with `off_word: Some("0")`.

## Verification

**The switch itself, by counter, in all three directions** — because "the
default flipped" and "the cache is running" are different claims:

| binary | env | field counters |
|---|---|---|
| base | none | `hit=0 miss=0 fill=0` |
| fixed | none | `hit=1602621 miss=610 fill=610` |
| fixed | `CRATONVM_JIT_FIELD_SITE_CACHE=0` | `hit=0 miss=0 fill=0` |

The kill switch restores the shipped behaviour exactly, which is what makes this
safe to default.

**`RFieldSiteCache`** — the vector built for this feature, 291 checks, every one
an exact expected value chosen so that a mixed-up site produces a different one
(every failure mode here returns a plausible number rather than throwing):

| arm | checks | verdict | engagement |
|---|---|---|---|
| default-ON | 291 | PASS | `hit=115093 miss=64567 reject_loader=64016` |
| kill switch `=0` | 291 | PASS | `hit=0` (inert — the control) |
| + loader arm | 291 | PASS | `hit=180133 miss=567 reject_loader=0` |

Identical answers in all three. The engagement counter is printed beside the
verdict deliberately: a PASS from a cache that never ran would prove nothing.

`reject_loader=64016` on the default arm is the conservative path working — the
vector's custom-classloader section produces loader-sensitive sites and the base
arm refuses to cache them, falling through to full resolution. The opt-in loader
arm converts those to hits and still answers all 291 identically.

**Regression suite: 63 of 64 vectors pass** against HotSpot. The one failure,
`RImmutableFactoryTypes`, fails identically on the base binary — pre-existing on
dev.

**difftest**: clean across `jit-on`, `nojit` and `interp-decoded` versus
HotSpot.

### A harness note worth recording

`regression-suite/run.sh` invokes the VM through `timeout`, an MSYS binary, so
Git Bash performs no `/c/…` → `C:\…` conversion on the arguments and
`--java-home` reaches the VM as a path it cannot resolve. Every vector then
exits `rc=1` with no output, and the suite reports `0 passed, 64 failed` —
which reads exactly like a catastrophic regression. Pass `JDK="C:/Program
Files/…"` on Git Bash. Nothing is wrong with the harness or the vectors; two
plausible-sounding diagnoses (a relative `CV=` path, and the `ONLY=` filter
skipping setup) were both wrong before the trace showed the real cause.

## What this leaves

A `getfield` still costs ~150 ns against a ~10 ns `iadd`. The cache removed the
re-resolution; what remains is the decoded-dispatch overhead (~12 ns, which the
fast-path arms would take) and the opcode body itself. The arms are now worth
~8% of a `getfield` rather than the ~2.5% they were worth against the
unaccelerated path — still the smaller half, and still worth doing, but the
ordering was the point.

`method-site-cache` remains off and should stay off until something measures it.
