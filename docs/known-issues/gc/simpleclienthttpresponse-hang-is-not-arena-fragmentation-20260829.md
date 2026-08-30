# `SimpleClientHttpResponseTests` hangs — and it is NOT arena fragmentation

## Status

**OPEN, re-attributed 2026-08-29.** This class was Occurrence 1 on
`zgc-arena-fragmentation-occurrences-to-reverify-20260829.md`. The
re-verification that page asked for has been run, and it takes the class OFF
that page: the hang survives with the fragmentation gauge firing **zero** times,
and the signature the page matched on belongs to a different guard.

The full matrix is in
`internal/fixed-suite-bugs/gc/zgc-frag-occurrences-reverified-20260829.md`.

## The failure

`org.springframework.http.client.SimpleClientHttpResponseTests`, Azure Linux
(`20.80.105.49`), JDK 25 Temurin, CratonVM `dev@96e07ca86`, through the suite's
own `one.sh` so it inherits Spring's Gradle test-JVM flags:

| VM | result |
|---|---|
| real HotSpot | **PASS** `found=5 succ=5 fail=0`, **9 s** |
| CratonVM, all four arms | **rc=124** — killed at the 500 s cap |

| arm | rc | wall | `zgc frag gauge` lines |
|---|---:|---:|---:|
| default | 124 | 500 s | 1 |
| `CRATONVM_ZGC_HIGH_COMPACTION=0` | 124 | 501 s | **0** |
| `CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` | 124 | 500 s | 1 |
| both `=0` | 124 | 500 s | 1 |

## Three reasons this is not the fragmentation defect

**1. It hangs identically in a run where the gauge never fires.** The `hi0` arm
timed out with `zgc frag gauge` count **0**, and a separate 240 s run under
`CRATONVM_GC_STATS=1` also logged the gauge **0** times while the class hung the
same way. A symptom that is absent while the failure is unchanged is not the
cause.

**2. Neither 2026-08-29 repair moves it, in either direction.** All four arms
time out, including `both=0`, which is the pre-2026-08-29 behaviour byte for
byte. Whatever this is, it predates and outlives that work.

**3. The signature the parent page matched on belongs to a different guard.**
That page's "symptom to match on" was the fragmentation gauge *"followed by …
a livelock in which the guard's own `occurrence` counter doubles on every firing
(32768 -> 1048576 -> …)"*. Measured, with the ANSI escapes stripped so the
fields are visible:

```text
occurrence= carriers in the failing log:
     23  "…that one boxes and fires only for the class mirror; this one nulls…"
     15  "a non-reference value was stored into a slot the class declares as a REFERENCE…"
      0  zgc frag gauge
```

The doubling counter — `… 32768 65536 131072 262144 524288 1048576` — is on the
**descriptor-coercion guards**, `G30-1-the-silent-reference-slot-coercion` and
its W7-84 autobox sibling. **The `zgc frag gauge` line carries no `occurrence`
field at all.** Both guards live in `cratonvm::gc::guard`, which is how the two
came to be read as one symptom.

## What it looks like it IS

At least **1,048,576** descriptor-coercion events in a 500 s run, and ≥524,288
in 240 s — a sustained rate, not a burst. The class is an HTTP-client test built
almost entirely out of Mockito mocks.

A backtrace census is available but is **dangerous to take casually**:
`CRATONVM_DBG_COERCION=1` on this class wrote **690 MB in 120 seconds** and
filled `/` on the shared Azure host (since cleaned up). Take it to a file on
`/data`, with a cap, or use the throttled `occurrence=` counter as above.

See `known-issues/jdk-only/G30-1-the-silent-reference-slot-coercion-20260817.md`
for the mechanism, and note its own reconciliation banner: the guard sees
descriptor MISMATCHES only, so this count is a count of wrong-*typed* writes and
not of wrong writes. A quiet log is not a clean one, and a loud one is not
necessarily the whole story either.

## Not yet done

- The backtrace census, taken safely, to name the coercing site(s). That is the
  one measurement that turns this from "re-attributed" into "diagnosed".
- Whether the hang is the coercion storm's cost or something the storm is a
  symptom of. Over a million events is enough to be the cost by itself, but
  that has not been separated from "the same wrong write is also breaking the
  logic and the test is spinning on it".
- A `--nojit` arm, which the fragmentation matrix did not include and which
  would say whether the storm is JIT-path-specific.

## Repro

```bash
# on the Azure host
cd /data/cratonvm/apps/spring-suite-runner
CRATONVM_BIN=<binary> JDK25=/data/toolchain/jdk-25 \
  timeout 500 bash one.sh org.springframework.http.client.SimpleClientHttpResponseTests

# HotSpot control, 9 s
JDK25=/data/toolchain/jdk-25 bash hs.sh \
  org.springframework.http.client.SimpleClientHttpResponseTests
```

Strip the ANSI escapes before grepping the log — `sed -r 's/\x1b\[[0-9;]*m//g'`.
Without that, `grep occurrence=` matches nothing, because the field is rendered
as `[3moccurrence[0m[2m=[0m`. That is how the counter came to be attributed to
the wrong guard in the first place.
