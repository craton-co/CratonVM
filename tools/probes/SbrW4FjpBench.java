// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.RecursiveTask;
import java.util.stream.LongStream;

/**
 * SBR wave 4, lane {@code fjp}: timing micro for the retired families that sit
 * on a hot path. Each row warms, then times {@code reps} calls and prints
 * microseconds per call and a checksum (so a wrong answer is visible next to
 * its time).
 *
 * <pre>
 *   java SbrW4FjpBench [rows] [div]      rows: comma list of fj,fjc,ps,sw,swc,rt (default all);
 *                                        div divides every warm-up and rep count (default 1)
 * </pre>
 */
public class SbrW4FjpBench {
    static final class Sum extends RecursiveTask<Long> {
        final long lo, hi;

        Sum(long lo, long hi) {
            this.lo = lo;
            this.hi = hi;
        }

        @Override
        protected Long compute() {
            if (hi - lo <= 1000) {
                long s = 0;
                for (long i = lo; i < hi; i++) s += i;
                return s;
            }
            long mid = (lo + hi) >>> 1;
            Sum l = new Sum(lo, mid);
            Sum r = new Sum(mid, hi);
            l.fork();
            long rv = r.compute();
            return l.join() + rv;
        }
    }

    interface Op {
        long run();
    }

    static int div = 1;

    static void time(String name, int warm, int reps, Op op) {
        warm = Math.max(1, warm / div);
        reps = Math.max(1, reps / div);
        long cs = 0;
        for (int i = 0; i < warm; i++) cs += op.run();
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) cs += op.run();
        long t1 = System.nanoTime();
        System.out.printf("%-6s %10.1f us/op  reps=%d  cs=%d%n", name, (t1 - t0) / 1000.0 / reps, reps, cs);
    }

    static long deepWalk(int n) {
        if (n == 0) {
            return StackWalker.getInstance().walk(s -> s.filter(f -> f.getMethodName().equals("main")).count());
        }
        return deepWalk(n - 1);
    }

    static final StackWalker RETAIN = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static long caller() {
        return RETAIN.getCallerClass().getName().length();
    }

    public static void main(String[] args) {
        String rows = args.length > 0 ? args[0] : "fj,fjc,ps,sw,swc,rt";
        div = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        ForkJoinPool pool = new ForkJoinPool(4);
        for (String r : rows.split(",")) {
            switch (r) {
                case "fj" -> time("fj", 20, 100, () -> pool.invoke(new Sum(0, 1_000_000)));
                case "fjc" -> time("fjc", 20, 100, () -> new Sum(0, 1_000_000).invoke());
                case "ps" -> time("ps", 20, 100, () -> LongStream.range(0, 1_000_000).parallel().map(x -> x ^ 5).sum());
                case "sw" -> time("sw", 200, 2000, () -> deepWalk(20));
                case "swc" -> time("swc", 2000, 20000, SbrW4FjpBench::caller);
                case "rt" -> time("rt", 2000, 200000, () -> Runtime.version().feature() + Runtime.getRuntime().hashCode() * 0L);
                default -> System.out.println("unknown row " + r);
            }
        }
        pool.shutdown();
    }
}
