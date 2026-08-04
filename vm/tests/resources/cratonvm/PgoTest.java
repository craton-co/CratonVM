// The corpus invokes this as `cratonvm/PgoTest` and the file lives in the
// `cratonvm/` directory — but it declared no package, so javac staged it in
// the DEFAULT package (`$OUT_DIR/test-classes/PgoTest.class`) and all 10 of
// its tests failed with `ClassNotFound: cratonvm/PgoTest`. The committed
// .class beside this file hid that: its `this_class` says `PgoTest` while its
// path says `cratonvm/PgoTest` — a contradiction HotSpot rejects outright and
// only CratonVM's loader tolerated.
package cratonvm;

public class PgoTest {

    // --- Branch profiling: biased branches ---

    // This method has a loop where the exit branch (i >= n) is rarely taken.
    // The loop body runs n times, accumulating branch profile data.
    // After enough iterations, the JIT should see the loop-exit branch as
    // usually-not-taken and emit a branch prediction hint.
    public static int hotLoop(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += i;
        }
        return sum;
    }

    // Gauss sum: tests that PGO loop branch hints produce correct results
    public static int testGaussSum() {
        return hotLoop(100); // 0+1+...+99 = 4950
    }

    // Repeated invocation builds up branch profile data
    public static int testRepeatedHotLoop() {
        int total = 0;
        for (int j = 0; j < 50; j++) {
            total += hotLoop(100);
        }
        return total; // 50 * 4950 = 247500
    }

    // --- Biased branch: one direction almost never taken ---

    public static int biasedBranch(int x) {
        if (x > 0) {
            return x * 2; // hot path (> 90% of calls)
        } else {
            return -x;    // cold path
        }
    }

    public static int testBiasedBranch() {
        int sum = 0;
        // Call with positive values 95 times, negative 5 times
        for (int i = 0; i < 95; i++) {
            sum += biasedBranch(i + 1); // always positive
        }
        for (int i = 0; i < 5; i++) {
            sum += biasedBranch(-(i + 1)); // negative
        }
        // positive: 2*(1+2+...+95) = 2*4560 = 9120
        // negative: abs(-1)+abs(-2)+...+abs(-5) = 15
        return sum; // 9135
    }

    // --- Loop trip count profiling ---

    // Short trip count: should suggest higher unroll factor
    public static int shortLoop() {
        int sum = 0;
        for (int i = 0; i < 8; i++) {
            sum += i;
        }
        return sum; // 28
    }

    public static int testShortLoopRepeated() {
        int total = 0;
        for (int j = 0; j < 200; j++) {
            total += shortLoop();
        }
        return total; // 200 * 28 = 5600
    }

    // Medium trip count
    public static int mediumLoop() {
        int sum = 0;
        for (int i = 0; i < 20; i++) {
            sum += i;
        }
        return sum; // 190
    }

    public static int testMediumLoopRepeated() {
        int total = 0;
        for (int j = 0; j < 100; j++) {
            total += mediumLoop();
        }
        return total; // 100 * 190 = 19000
    }

    // --- Receiver type profiling (via static method calls, avoiding inner class dispatch issues) ---

    // Monomorphic-style: same computation repeated, testing MIC prepopulation
    public static int testMonomorphicDispatch() {
        int total = 0;
        for (int i = 0; i < 100; i++) {
            total += squareArea(5);
        }
        return total; // 100 * 25 = 2500
    }

    private static int squareArea(int side) {
        return side * side;
    }

    // Bimorphic-style: alternating computations via branch
    public static int testBimorphicDispatch() {
        int total = 0;
        for (int i = 0; i < 100; i++) {
            if (i % 2 == 0) {
                total += squareArea(3); // 9
            } else {
                total += rectArea(4, 5); // 20
            }
        }
        // 50*9 + 50*20 = 450 + 1000 = 1450
        return total;
    }

    private static int rectArea(int w, int h) {
        return w * h;
    }

    // --- Combined PGO scenario ---

    // Exercises loops with profiled trip counts and biased branches.
    public static int testCombinedPgo() {
        int sum = 0;

        for (int i = 0; i < 50; i++) {
            // Loop with biased branch
            if (i < 45) {
                sum += squareArea(4); // hot path: 45 * 16 = 720
            } else {
                sum += i; // cold path: 45+46+47+48+49 = 235
            }
        }
        return sum; // 955
    }

    // --- Correctness after JIT with PGO data ---

    public static int testGaussSumLarge() {
        return hotLoop(1000); // 0+1+...+999 = 499500
    }

    public static int testNestedLoops() {
        int sum = 0;
        for (int i = 0; i < 10; i++) {
            for (int j = 0; j < 10; j++) {
                sum += i * j;
            }
        }
        // sum = (0+1+...+9)*(0+1+...+9) = 45*45 = 2025
        return sum;
    }

    public static void main(String[] args) {
        check("gauss_sum", testGaussSum(), 4950);
        check("repeated_hot_loop", testRepeatedHotLoop(), 247500);
        check("biased_branch", testBiasedBranch(), 9135);
        check("short_loop_repeated", testShortLoopRepeated(), 5600);
        check("medium_loop_repeated", testMediumLoopRepeated(), 19000);
        check("monomorphic_dispatch", testMonomorphicDispatch(), 2500);
        check("bimorphic_dispatch", testBimorphicDispatch(), 1450);
        check("combined_pgo", testCombinedPgo(), 955);
        check("gauss_sum_large", testGaussSumLarge(), 499500);
        check("nested_loops", testNestedLoops(), 2025);
        System.out.println("ALL PGO TESTS PASSED");
    }

    static void check(String name, int got, int expected) {
        if (got != expected) {
            System.out.println("FAIL " + name + ": expected " + expected + " got " + got);
            System.exit(1);
        }
    }
}
