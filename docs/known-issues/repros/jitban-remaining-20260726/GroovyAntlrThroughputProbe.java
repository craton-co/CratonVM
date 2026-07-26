import groovy.lang.GroovyShell;
import groovy.lang.Binding;

// ANTLR.1 retest: the package-level groovyjarjarantlr4/ ban's ONLY
// remaining justification (the correctness reason is already covered
// independently and unconditionally by is_antlr_prediction_context_miscompile)
// is a documented ~8x cold-parse THROUGHPUT regression under JIT, predating
// today's "big JIT rework". This probe times repeated cold Groovy script
// parses (fresh GroovyShell + fresh script text per iteration, so ANTLR's
// ATN simulation runs cold each time -- matching the original "trivial
// warmup class alone takes ~95s under JIT" shape) and reports total/average
// wall-clock, to compare against a --nojit / ban-active baseline.
public class GroovyAntlrThroughputProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20;

        String[] scriptTemplates = {
            "class Widget%d { String name; int value; Widget%d(String n, int v) { name = n; value = v } }\n"
                    + "def w = new Widget%d('probe', %d)\n"
                    + "def total = 0\n"
                    + "for (int i = 0; i < 50; i++) { total += i * w.value }\n"
                    + "return total",
        };

        long totalNanos = 0;
        int failures = 0;
        long[] perIterationMs = new long[iterations];

        for (int i = 0; i < iterations; i++) {
            String script = String.format(scriptTemplates[0], i, i, i, i);
            long t0 = System.nanoTime();
            Object result;
            try {
                GroovyShell shell = new GroovyShell(new Binding());
                result = shell.evaluate(script);
            } catch (Throwable t) {
                failures++;
                result = null;
                if (failures <= 5) {
                    System.out.println("FAIL at i=" + i + ": " + t);
                }
            }
            long elapsedNanos = System.nanoTime() - t0;
            perIterationMs[i] = elapsedNanos / 1_000_000;
            totalNanos += elapsedNanos;

            System.out.println("iter=" + i + " ms=" + perIterationMs[i] + " result=" + result);
            System.out.flush();
        }

        double avgMs = (totalNanos / 1_000_000.0) / iterations;
        System.out.println("DONE iterations=" + iterations + " failures=" + failures
                + " totalMs=" + (totalNanos / 1_000_000) + " avgMs=" + avgMs);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
