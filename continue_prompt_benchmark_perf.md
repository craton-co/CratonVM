# Bug/Task: CratonVM-CPU benchmark perf gap vs HotSpot

Perf, not correctness. All micro-benchmark checksums are **identical** across CratonVM-CPU,
HotSpot, and TornadoVM (correctness is perfect on the 5 working benchmarks). Independent of
the other `continue_prompt_*` bugs. (bintrees18 is excluded — it crashes; that's the
separate JIT safepoint-spill effort.)

## Numbers (this session, CPU mode, ms; lower is better)
| benchmark  | cratonvm-cpu | hotspot | ratio |
|------------|-------------:|--------:|------:|
| arith1500M | 9321 | 5061 | ~1.8× |
| fib44      | 7910 | 5131 | ~1.5× |
| sieve250k  | 2531 | 1914 | ~1.3× |
| matrix600  | 1186 |  587 | ~2.0× (worst) |
| vadd2_28   | 3235 | 2446 | ~1.3× |

## Next steps
- Profile the hottest gaps first: `matrix600` (int[][] triple loop) and `arith1500M`
  (long arithmetic + integer div/mod in a tight counted loop). Look at the JIT codegen for
  those loops (bounds-check elimination, induction-variable / strength reduction, register
  allocation, div/mod lowering).
- Compare JIT-on vs `--nojit` to confirm the JIT is engaging, and use the OSR/inlining
  diagnostics. Watch for missed inlining or spills in the inner loop.

## Repro
```
CV=target/release/cratonvm.exe; JDK="C:/Program Files/Java/jdk-25"
$CV --java-home "$JDK" --Xmx 8g -cp bench BenchSuite matrix600   # RESULT name=.. ms=.. checksum=..
"$JDK/bin/java.exe" -Xmx8g -cp bench BenchSuite matrix600        # HotSpot baseline
# Or the 4-way: VARIANTS="cratonvm-cpu hotspot tornadovm" bash test-infra/run-vm-comparison.sh (SECTIONS=bench)
```
Memory: `reference_cross_vm_comparison_harness`, `reference_osr_main_corruptor` (GC/OSR perf notes).
