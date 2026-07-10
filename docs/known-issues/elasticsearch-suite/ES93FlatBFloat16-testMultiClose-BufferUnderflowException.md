# ES93FlatBFloat16VectorFormatTests.testMultiClose - genuine BufferUnderflowException

Status: OPEN

Split off from `docs/internal/fixed-suite-bugs/throwable-addsuppressed-clobbers-cause-FIXED.md`
once that doc's `Caused by: java.lang.Object` corruption was fixed
(`native_throwable_add_suppressed`/`native_throwable_get_suppressed` were
clobbering `Throwable.cause` with a hardcoded wrong field index). With the
corruption gone, `testMultiClose` still fails — now with a clean, honest
signal:

```text
1) testMultiClose(org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat16VectorFormatTests)
java.nio.BufferUnderflowException
```

(No message, no cause — a real `BufferUnderflowException` with nothing
further to report, unlike the pre-fix corrupted trace.)

Reproduce (seed `B17AC9D3E1F2A0C4`, deterministic, JIT-on and `--nojit`
both fail — see `apps/elasticsearch-suite-runner/run-elasticsearch-suite.md`
for the full flag set):

```powershell
<cratonvm>.exe --java-home "C:\Program Files\Java\jdk-25" -Xmx 2g `
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home=<es-checkout> ... `
  -cp <server-module-craton-testcp.txt-contents> org.junit.runner.JUnitCore `
  org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat16VectorFormatTests
```

Not yet investigated: this is a genuine Lucene-codec-level correctness
question (does the buffer legitimately run out of bytes during a
close/reopen cycle when it shouldn't, or is HotSpot's real behavior also
to throw here and the test's `expectThrows`-style handling differs?) rather
than an exception-machinery bug. HotSpot passed this test historically (see
the retired doc's original entry), so it's still a CratonVM divergence —
just not the one this family was originally reported for. The corrupted
trace previously made this hard to separate from the print-corruption bug;
it no longer is.
