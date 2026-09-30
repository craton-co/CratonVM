/*
 * Interpreter round i1, lane L6: identity of non-capturing lambdas and method
 * references (one linked call site per invokedynamic INSTRUCTION, JVMS
 * 5.4.3.6), and the cost of evaluating a lambda and of calling through one.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   sameSiteTwice=true
 *   twoMethodsSharedEntry=false
 *   twoOccurrencesOneMethod=false
 *   warmMismatches=0
 *   concurrentFirstUse=1
 *
 * Timings go to STDERR (compare medians of interleaved runs; see the
 * microbench-noise note): ns per non-capturing lambda evaluation, per
 * capturing lambda evaluation, and per call through a lambda proxy.
 *
 * CratonVM before i1-L6: `twoMethodsSharedEntry=true` (ref1 and ref2 share one
 * CONSTANT_InvokeDynamic entry AND bci 0, and the singleton cache was keyed on
 * the bci alone). Before wave 5 (see
 * docs/internal/fixed-bugs/interpreter-L6-lambda-singleton-identity-splits-between-interpreter-and-jit-FIXED-20260924.md):
 * `warmMismatches=298761 concurrentFirstUse=2` with the JIT on, because the
 * compiled bridge keyed the singleton differently from the interpreter.
 * `concurrentFirstUse` can exceed 1 only under a race; before i1-L6, two
 * threads minting the same cold site could keep two different instances.
 */
import java.util.IdentityHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.function.Function;
import java.util.function.IntSupplier;
import java.util.function.Supplier;

public class LambdaIdentityProbe {
    static Supplier<String> make() {
        return () -> "k";
    }

    static Function<String, Integer> ref1() {
        return String::length;
    }

    static Function<String, Integer> ref2() {
        return String::length;
    }

    @SuppressWarnings("unchecked")
    static Function<String, Integer>[] both() {
        Function<String, Integer>[] r = new Function[2];
        r[0] = String::length;
        r[1] = String::length;
        return r;
    }

    static Runnable cold() {
        return () -> {};
    }

    public static void main(String[] args) throws Exception {
        System.out.println("sameSiteTwice=" + (make() == make()));
        System.out.println("twoMethodsSharedEntry=" + (ref1() == ref2()));
        Function<String, Integer>[] b = both();
        System.out.println("twoOccurrencesOneMethod=" + (b[0] == b[1]));

        Supplier<String> first = make();
        int mismatches = 0;
        for (int i = 0; i < 300_000; i++) {
            if (make() != first) {
                mismatches++;
            }
        }
        System.out.println("warmMismatches=" + mismatches);

        int threads = 8;
        Runnable[] got = new Runnable[threads];
        CountDownLatch start = new CountDownLatch(1);
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int slot = t;
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
                got[slot] = cold();
            });
            ts[t].start();
        }
        start.countDown();
        for (Thread t : ts) {
            t.join();
        }
        IdentityHashMap<Runnable, Boolean> distinct = new IdentityHashMap<>();
        for (Runnable r : got) {
            distinct.put(r, Boolean.TRUE);
        }
        distinct.put(cold(), Boolean.TRUE);
        System.out.println("concurrentFirstUse=" + distinct.size());

        // ---- timings (stderr only) ----
        final int n = 2_000_000;
        long sink = 0;
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += make().hashCode() & 1;
            }
            long t1 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                final int x = i;
                IntSupplier s = () -> x;
                sink += s.getAsInt() & 1;
            }
            long t2 = System.nanoTime();
            Function<String, Integer> f = String::length;
            for (int i = 0; i < n; i++) {
                sink += f.apply("abc");
            }
            long t3 = System.nanoTime();
            System.err.printf(
                    "rep %d: nonCapturingEval=%.1f ns  capturingEvalAndCall=%.1f ns  callThroughProxy=%.1f ns%n",
                    rep,
                    (t1 - t0) / (double) n,
                    (t2 - t1) / (double) n,
                    (t3 - t2) / (double) n);
        }
        System.err.println("sink=" + sink);
    }
}
