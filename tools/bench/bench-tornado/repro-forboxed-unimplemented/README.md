# Repro set: `TornadoSnippetReflectionProvider.forBoxed` unimplemented

Diagnostic-only fixtures, not part of the regular GPU comparison suite
(`run-gpu-comparison.sh` / `run-gpu-warm.sh` never invoke these). They exist
to isolate the exact trigger for the `TornadoInternalError: unimplemented`
that `TornadoDotBench.dotReduce` (the TornadoVM twin of
`bench-gpu/GpuDotBench.dotReduce`) hits on this box — see
`bench-gpu/results/gpu-comparison-20260802.md` section 2 and
`BENCHMARK.md`'s GPU notes for the full writeup.

**Root cause:** `TornadoSnippetReflectionProvider.forBoxed` is an
unconditional stub (`unimplemented(); return null;`) in both the TornadoVM
4.0.1 jar this repo builds against and the current `master` branch on
GitHub (confirmed 2026-08-03) — so this is not fixable by upgrading
TornadoVM. Graal's `SnippetTemplate.bind`/`instantiate` calls into it while
lowering a `@Reduce` kernel whose per-element expression needs a primitive
widening conversion before accumulating into a differently-typed reduce
array.

**Results on this box** (RTX 2060, driver 591.86, TornadoVM 4.0.1-jdk25-ptx):

| Fixture | Shape | Result |
|---|---|---|
| `TornadoLongSumRepro` | single `LongArray` in, `LongArray` out, no cast | **works** |
| `TornadoLongTwoArraySum` | two `LongArray` in, `LongArray` out, no cast, no multiply | **works** |
| `TornadoIntToLongCastSum` | single `IntArray` in, one `(long)` widening cast, `LongArray` out | **fails** — `TornadoInternalError: unimplemented` |

So the gap is specifically a `@Reduce` kernel that needs an `int`→`long`
widening conversion before accumulating — not "long reductions" or
"two-array reductions" in general, both of which work fine when the types
already match end-to-end. `GpuDotBench.dotReduce`'s `int·int → long` shape
exists specifically to avoid `int` overflow in the product, so there is no
workaround here that preserves the benchmark's intended semantics.

`TornadoIntToLongCastSum` is the minimal standalone repro if this ever gets
filed against [beehive-lab/TornadoVM](https://github.com/beehive-lab/TornadoVM).

## Running

Same pattern as the other `bench-tornado/Tornado*.java` sources — compile
with `--patch-module tornado.examples=<dir>`, run with `@tornado-argfile`.
Each fixture is self-contained (own `main`), so no `n`/`reps` arguments are
needed:

```bash
"$TVM_JAVAC" -g --module-path "$TVJARS" --add-modules tornado.annotation,tornado.api \
  --patch-module tornado.examples="$GO" -d "$GO" \
  bench-tornado/repro-forboxed-unimplemented/*.java

"$TVM" "@$ARGFILE" --patch-module tornado.examples="$GO" \
  -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoIntToLongCastSum
```
