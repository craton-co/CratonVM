# A `[site-alias]` JIT diagnostic prints to stdout, and it makes the strict corpus gate nondeterministic

| | |
|---|---|
| **Status** | OPEN — reproduced and rate-measured, not diagnosed |
| **Severity** | medium — the print itself is cosmetic; what it *reports* may not be, and it turns a ratchet gate into a coin flip |
| **Modes** | seen under `--jdk-only`; the print is not mode-gated |
| **Opened** | 2026-08-06, while measuring `nio/buffer-address-indexed-slot-aliasing.md` |

## What happens

`vm/src/jit/helpers.rs:1959` prints, unconditionally:

```text
[site-alias] #1 of 14 keys: key=0x2000deb0cc0 WAS java/lang/String.isLatin1()Z NOW java/util/regex/Matcher.lookingAt()Z
```

It lands on the probe's stdout. `scripts/jdk-only-strict-probes.sh` keys its
baseline on `(probe, arm, section)` and takes the first token of each line as
the section name, so the line arrives as a **new section** called
`[site-alias]` and the ratchet fires:

```text
divergent sections: 6 observed, 5 baselined
NEW DIVERGENCES — the ratchet fired:
  + JdkOnlyPlatformProbe/strict/[site-alias]
RESULT: FAIL -- the strict corpus regressed against its baseline.
```

## It is intermittent, and the rate depends on the binary

Same binary, back-to-back gate runs:

```text
run 1   divergent sections: 5 observed, 5 baselined   RESULT: PASS
run 2   divergent sections: 6 observed, 5 baselined   RESULT: FAIL
```

Six `JdkOnlyPlatformProbe --jdk-only` runs per binary, counting runs where the
line appeared at all:

| binary | fired |
|---|---|
| `origin/dev` @ `66725787e` | 1 / 6 |
| the same tree plus an unrelated `java.nio` change | 5 / 6 |

So it is **not** introduced by any one change — it reproduces on pristine dev —
but the rate moves with the binary. The key in the message is an address
(`key=0x2000deb0cc0`), so the most likely reading is that the event depends on
JIT code-cache addresses, which shift with any change to code layout. That
would make the rate difference a property of the build, not of the diff. It has
not been confirmed.

## Two separate things to fix, and they have different urgencies

**The print.** A diagnostic that is on by default and writes to stdout is what
`vm-cli/tests/no_diag_eprintln.rs` exists to prevent; `[site-alias]` is simply
not in that test's forbidden-tag list. Gate it behind an env flag (or route it
to `tracing::debug!`) and the gate stops flaking. That is the cheap half.

**What it reports.** The message says a `JitSiteKey` was **recycled**: the key
that used to identify `String.isLatin1()Z` now identifies
`Matcher.lookingAt()Z`. If anything caches per-site state under that key — an
inline cache, a profile, a devirtualisation assumption — then a recycled key is
a wrong-target hazard, not noise. `[site-alias] (further hits are counted, not
printed)` and the `[cratonvm] site-alias: distinct JitSiteKeys=… recycled-key
hits=…` summary in `vm-cli/src/main.rs:4205` suggest whoever added this was
chasing exactly that. **Do not silence the print without first answering
whether a recycled key can reach a cache.** Adjacent shapes:
`array-receivers-alias-component-class-id-in-inline-caches`,
`jit-code-unmapped-while-executing`.

## Repro

```bash
CP=<strict corpus classes>
for i in $(seq 1 6); do
  cratonvm --jdk-only --java-home "$JAVA_HOME" -cp "$CP" JdkOnlyPlatformProbe 2>&1 \
    | grep -c 'site-alias'
done
```

Non-zero on some runs, zero on others, with no other input changing.

## Why this doc exists rather than a baseline bump

`--update-baseline` would make the gate green by recording `[site-alias]` as an
accepted divergent section. That freezes a *diagnostic print* into the contract
and hides the recycled-key question underneath it. The gate is correct here: a
line appeared that was not there before.
