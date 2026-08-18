/**
 * What does a one-line array accessor cost when it is a CALL instead of a load?
 *
 * `BOBYQAOptimizerTest` is ~80x HotSpot with the JIT fully engaged, and a Java
 * frame sampler puts ~57% of the run in `ArrayRealVector.getEntry` /
 * `Array2DRowRealMatrix.getEntry` / `setEntry` — three methods whose entire
 * body is a field read and an array load
 * (docs/known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md).
 * A 45-second run is far too slow an instrument to iterate a fix on, and the
 * suite needs a commons-math checkout. This is that shape with nothing else in
 * it.
 *
 * The accessors are copied from commons-math bytecode-for-bytecode, because two
 * details of the real ones are load-bearing and a hand-simplified getter loses
 * both:
 *
 *   * `ArrayRealVector.getEntry(int)` is `aload_0; getfield data:[D; iload_1;
 *     daload; dreturn` **plus an exception table** covering the whole body
 *     (`catch IndexOutOfBoundsException` → build an `OutOfRangeException`);
 *   * `Array2DRowRealMatrix.getEntry(int,int)` is the same with `aaload; daload`
 *     and its handler contains an **`athrow`**.
 *
 * A callee that declares an exception table has its own history in this VM: it
 * was barred from the inline machine-code MIC/PIC cascade and cost 11.4x per
 * call for it (`CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH`, now default-ON; the
 * statically-bound door's `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH` is still
 * opt-in and OFF). So the table is not a detail to drop — it is one of the two
 * things this probe is here to price.
 *
 * ## The arms, and what each isolates
 *
 *   * `raw`            — the loop with the array indexed inline. The FLOOR:
 *                        whatever a perfect inliner would produce.
 *   * `plainGetter`    — the same loop through a getter with NO exception
 *                        table. `raw` vs this is the cost of not inlining.
 *   * `tableGetter`    — the same getter WITH commons-math's exception table.
 *                        `plainGetter` vs this is the cost of the table alone,
 *                        with the call held constant.
 *   * `matrixGetter`   — the 2-D shape, table and `athrow` included.
 *
 * Every arm walks the same indices and accumulates the same closed-form sum, so
 * a wrong answer is a mismatch rather than a judgement call, and each is called
 * exactly ONCE so the loop's only compile door is OSR — the same door the real
 * `trsbox`/`bobyqb` use.
 *
 * ## Why each arm reports the MINIMUM of its rounds, not the mean
 *
 * This host builds several worktrees at once. A first draft timed each arm as
 * one span and read `raw` at 5.62, 4.85, 5.14, 6.13, 4.99 and 4.32 ns/iter
 * across six runs of the SAME binary — the baseline moved 30% while the levers
 * under test were expected to move 10%, so nothing in that sweep was readable.
 *
 * Each arm now runs `rounds` timed chunks inside its single call and keeps the
 * fastest. A competing build can only ever make a chunk slower, so the minimum
 * is the arm's own floor and contention shows up as spread rather than as bias.
 * It also drops the warm-up for free: the early chunks are interpreted, the
 * later ones compiled, and the minimum is the compiled steady state — which is
 * what a page about compiled throughput wants. The `spread` column is printed
 * so a run made during a storm is visible as one rather than quietly believed.
 *
 *   javac -d . AccessorDispatchProbe.java
 *   java     -cp . AccessorDispatchProbe 20000000     # the oracle
 *   cratonvm --java-home <jdk> -cp . AccessorDispatchProbe 20000000
 */
public final class AccessorDispatchProbe {

    /** `ArrayRealVector`'s accessor shape, exception table and all. */
    static final class Vec {
        private final double[] data;
        Vec(int n) {
            data = new double[n];
            for (int i = 0; i < n; i++) { data[i] = i; }
        }
        /** Byte-identical in shape to `ArrayRealVector.getEntry(int)`. */
        double getEntry(int index) {
            try {
                return data[index];
            } catch (IndexOutOfBoundsException e) {
                throw new IllegalArgumentException(MSG, e);
            }
        }
        /** The same read with NO exception table — the discriminator. */
        double getEntryNoTable(int index) {
            return data[index];
        }
        void setEntry(int index, double value) {
            try {
                data[index] = value;
            } catch (IndexOutOfBoundsException e) {
                throw new IllegalArgumentException(MSG, e);
            }
        }
        double[] raw() { return data; }
    }

    /** `Array2DRowRealMatrix`'s accessor shape — note the `athrow` in the handler. */
    static final class Mat {
        private final double[][] data;
        Mat(int rows, int cols) {
            data = new double[rows][cols];
            for (int r = 0; r < rows; r++) {
                for (int c = 0; c < cols; c++) { data[r][c] = r + c; }
            }
        }
        double getEntry(int row, int col) {
            try {
                return data[row][col];
            } catch (IndexOutOfBoundsException e) {
                checkIndex(row, col);
                throw e;
            }
        }
        static void checkIndex(int row, int col) {
            throw new IllegalArgumentException(MSG);
        }
        double[][] raw() { return data; }
    }

    /** A plain constant, deliberately: a `+` here compiles to `invokedynamic`,
     *  and an indy in the callee trips a SEPARATE compile gate (`indy-trap`),
     *  which would make this probe measure that instead of the exception table. */
    static final String MSG = "index out of range";

    static double sink;
    static int failures;
    /** Per-arm best and worst ns/iter over its rounds, filled by `timed`. */
    static double[] best = new double[8];
    static double[] worst = new double[8];

    static void check(String what, double got, double want) {
        boolean ok = got == want;
        if (!ok) { failures++; }
        System.out.println((ok ? "PASS " : "FAIL ") + what + " got=" + got + " want=" + want);
    }

    // ---- arms: each called ONCE, so OSR is the only door ------------------
    //
    // Each runs `rounds` timed chunks of `per` iterations and records the
    // fastest and slowest into `best`/`worst` at `slot`. The loop bodies are
    // the only difference between them.

    static void record(int slot, long ns, int per) {
        double v = ns / (double) per;
        if (best[slot] == 0 || v < best[slot]) { best[slot] = v; }
        if (v > worst[slot]) { worst[slot] = v; }
    }

    static double raw(int rounds, int per, Vec v, int mask) {
        double[] d = v.raw();
        double a = 0;
        for (int r = 0; r < rounds; r++) {
            long s = System.nanoTime();
            for (int i = 0; i < per; i++) { a += d[i & mask]; }
            record(0, System.nanoTime() - s, per);
        }
        return a;
    }

    static double plainGetter(int rounds, int per, Vec v, int mask) {
        double a = 0;
        for (int r = 0; r < rounds; r++) {
            long s = System.nanoTime();
            for (int i = 0; i < per; i++) { a += v.getEntryNoTable(i & mask); }
            record(1, System.nanoTime() - s, per);
        }
        return a;
    }

    static double tableGetter(int rounds, int per, Vec v, int mask) {
        double a = 0;
        for (int r = 0; r < rounds; r++) {
            long s = System.nanoTime();
            for (int i = 0; i < per; i++) { a += v.getEntry(i & mask); }
            record(2, System.nanoTime() - s, per);
        }
        return a;
    }

    static double matrixRaw(int rounds, int per, Mat m, int mask) {
        double[][] d = m.raw();
        double a = 0;
        for (int r = 0; r < rounds; r++) {
            long s = System.nanoTime();
            for (int i = 0; i < per; i++) { a += d[i & mask][i & mask]; }
            record(3, System.nanoTime() - s, per);
        }
        return a;
    }

    static double matrixGetter(int rounds, int per, Mat m, int mask) {
        double a = 0;
        for (int r = 0; r < rounds; r++) {
            long s = System.nanoTime();
            for (int i = 0; i < per; i++) { a += m.getEntry(i & mask, i & mask); }
            record(4, System.nanoTime() - s, per);
        }
        return a;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        int per = args.length > 1 ? Integer.parseInt(args[1]) : 2_000_000;
        final int len = 256;
        final int mask = len - 1;
        Vec v = new Vec(len);
        Mat m = new Mat(len, len);

        // Closed form: `per` is a multiple of `len` in every sane invocation,
        // but do not assume it — walk the remainder explicitly.
        long cycles = per / len;
        long rem = per % len;
        double cycleSum = (double) mask * (mask + 1) / 2;
        double perRound = cycles * cycleSum + (double) (rem - 1) * rem / 2;
        double wantVec = rounds * perRound;
        double wantMat = 2 * wantVec;

        check("raw", raw(rounds, per, v, mask), wantVec);
        check("plainGetter", plainGetter(rounds, per, v, mask), wantVec);
        check("tableGetter", tableGetter(rounds, per, v, mask), wantVec);
        check("matrixRaw", matrixRaw(rounds, per, m, mask), wantMat);
        check("matrixGetter", matrixGetter(rounds, per, m, mask), wantMat);
        sink += best[0] + best[1] + best[2] + best[3] + best[4];

        String[] names = {"raw", "plainGetter", "tableGetter", "matrixRaw", "matrixGetter"};
        StringBuilder line = new StringBuilder("ns/iter ");
        StringBuilder spread = new StringBuilder("spread  ");
        for (int i = 0; i < names.length; i++) {
            line.append(String.format(" %s=%.2f", names[i], best[i]));
            spread.append(String.format(" %s=%.1fx", names[i], worst[i] / best[i]));
        }
        System.out.println(line);
        System.out.println(spread);
        System.out.println("callTax=" + String.format("%.2f", best[1] - best[0])
                + " tableTax=" + String.format("%.2f", best[2] - best[1]));
        System.out.println("sinkNonZero=" + (sink != 0 ? 1 : 0));
        System.out.println("failures=" + failures);
        System.out.println("OK");
        if (failures != 0) { System.exit(1); }
    }
}
