// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// j27 wave 6, lane T1LOOPFFM (book docs/feature-designs/native-dispatch-architecture.md):
//   VEC27-1  reductions over MemorySegment element reads (heap, slice, native, long) -- the vector pre-loop
//            (default ON since the r27 close; CRATONVM_JIT_IR_VECTOR_SCOPED_GET_REDUCE=0 kills); and the
//            same reductions into a `long` accumulator (`h += s.getAtIndex(INT, k)`, `*-wide` rows)
//   iota     reductions that read the induction itself (`s += a[k] + k`, `s -= a[k] * k`,
//            `m = Math.max(m, a[k] + k)`, long lanes): planned by default since round 24 B1; before the
//            j27 fix every vector step after the first re-added the first step's `j + i` lanes
//
// Facts (deterministic; each printed cold, then again after WARM calls of every kernel -- both passes must
// equal HotSpot 25, both modes, every arm). Expected HotSpot 25 output, worked out from the JLS (int / long
// wraparound) over N = 1003 elements a[i] = (i * 7919) % 1000 - 500, b[i] = i * i * 1000003 - 7:
//   fact <pass> heap-sum -243
//   fact <pass> slice-sum -933
//   fact <pass> native-sum -243
//   fact <pass> long-sum 335840512511494
//   fact <pass> heap-sum-13 -818
//   fact <pass> heap-sum-wide -243
//   fact <pass> native-sum-wide -243
//   fact <pass> heap-sum-wide-13 -818
//   fact <pass> heap-sum-wide-max -2147483588
//   fact <pass> iota-sum 502260
//   fact <pass> iota-sum-13 -740
//   fact <pass> iota-sub 25405
//   fact <pass> iota-max 1460
//   fact <pass> iota-long-sum 335840513013997
//   fact <pass> heap-oob java.lang.IndexOutOfBoundsException
//   fact <pass> heap-wide-oob java.lang.IndexOutOfBoundsException
//   fact <pass> native-closed java.lang.IllegalStateException: Already closed
//   fact <pass> native-wide-closed java.lang.IllegalStateException: Already closed
// with <pass> = cold, then warm.
//
// Timing (only with -Dj27.time=true): `time <row> <median> ns/op spread <max/min> settled <bool>` per row,
// one op = one N-element pass; reps repeat until three in a row agree within 10 % (at most -Dj27.maxreps,
// default 20) -- the MEAS27-1 rule: a row whose reps never settle cannot judge a 10 % switch.
//   java -cp tools/probes/out J27T1LoopFfmRows
//   cratonvm [--compatible] -cp tools/probes/out J27T1LoopFfmRows
//   CRATONVM_DBG_JITC=1 cratonvm ... J27T1LoopFfmRows
//     (engagement: `[ir-vec] ... heapSum ... reduction ... scoped heap` / `scoped native`)
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.Locale;

public class J27T1LoopFfmRows {
    static final ValueLayout.OfInt LE_INT = ValueLayout.JAVA_INT.withOrder(ByteOrder.LITTLE_ENDIAN);
    static final ValueLayout.OfLong LE_LONG = ValueLayout.JAVA_LONG.withOrder(ByteOrder.LITTLE_ENDIAN);
    static final int N = 1003;
    static final int WARM = 20_000;
    static volatile long sink;

    // ------------------------------------------------------------- kernels

    static int heapSum(MemorySegment s, int n) {
        int h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(LE_INT, k);
        }
        return h;
    }

    static int sliceSum(MemorySegment s, int n) {
        int h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(LE_INT, k);
        }
        return h;
    }

    static int nativeSum(MemorySegment s, int n) {
        int h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(LE_INT, k);
        }
        return h;
    }

    static long heapSumWide(MemorySegment s, int n) {
        long h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(LE_INT, k);
        }
        return h;
    }

    static long nativeSumWide(MemorySegment s, int n) {
        long h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(LE_INT, k);
        }
        return h;
    }

    static long longSum(MemorySegment s, int n) {
        long h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(LE_LONG, k);
        }
        return h;
    }

    static int iotaSum(int[] a, int n) {
        int s = 0;
        for (int k = 0; k < n; k++) {
            s += a[k] + k;
        }
        return s;
    }

    static int iotaSub(int[] a, int n) {
        int s = 0;
        for (int k = 0; k < n; k++) {
            s -= a[k] * k;
        }
        return s;
    }

    static int iotaMax(int[] a, int n) {
        int m = Integer.MIN_VALUE;
        for (int k = 0; k < n; k++) {
            m = Math.max(m, a[k] + k);
        }
        return m;
    }

    static long iotaLongSum(long[] b, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += b[k] + k;
        }
        return s;
    }

    // --------------------------------------------------------------- facts

    static int[] ints() {
        int[] a = new int[N];
        for (int i = 0; i < N; i++) {
            a[i] = (i * 7919) % 1000 - 500;
        }
        return a;
    }

    static long[] longs() {
        long[] b = new long[N];
        for (int i = 0; i < N; i++) {
            b[i] = (long) i * i * 1000003L - 7;
        }
        return b;
    }

    static String exc(Runnable r) {
        try {
            r.run();
            return "no exception";
        } catch (IndexOutOfBoundsException e) {
            // Its message names the segment's array identity: the class only.
            return e.getClass().getName();
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static void facts(String pass, int[] a, long[] b, MemorySegment heap, MemorySegment nat, MemorySegment lseg) {
        System.out.println("fact " + pass + " heap-sum " + heapSum(heap, N));
        System.out.println("fact " + pass + " slice-sum " + sliceSum(heap.asSlice(4L * 5), N - 5));
        System.out.println("fact " + pass + " native-sum " + nativeSum(nat, N));
        System.out.println("fact " + pass + " long-sum " + longSum(lseg, N));
        System.out.println("fact " + pass + " heap-sum-13 " + heapSum(heap, 13));
        System.out.println("fact " + pass + " heap-sum-wide " + heapSumWide(heap, N));
        System.out.println("fact " + pass + " native-sum-wide " + nativeSumWide(nat, N));
        System.out.println("fact " + pass + " heap-sum-wide-13 " + heapSumWide(heap, 13));
        System.out.println("fact " + pass + " heap-sum-wide-max " + heapSumWide(MemorySegment.ofArray(
                new int[] {Integer.MIN_VALUE, Integer.MIN_VALUE, Integer.MAX_VALUE, -1, 7, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10}), 15));
        System.out.println("fact " + pass + " iota-sum " + iotaSum(a, N));
        System.out.println("fact " + pass + " iota-sum-13 " + iotaSum(a, 13));
        System.out.println("fact " + pass + " iota-sub " + iotaSub(a, N));
        System.out.println("fact " + pass + " iota-max " + iotaMax(a, N));
        System.out.println("fact " + pass + " iota-long-sum " + iotaLongSum(b, N));
        System.out.println("fact " + pass + " heap-oob " + exc(() -> heapSum(heap, N + 1)));
        System.out.println("fact " + pass + " heap-wide-oob " + exc(() -> heapSumWide(heap, N + 1)));
        Arena gone = Arena.ofConfined();
        MemorySegment closed = gone.allocate(4L * N, 8);
        gone.close();
        System.out.println("fact " + pass + " native-closed " + exc(() -> nativeSum(closed, N)));
        System.out.println("fact " + pass + " native-wide-closed " + exc(() -> nativeSumWide(closed, N)));
    }

    static void warm(int[] a, long[] b, MemorySegment heap, MemorySegment nat, MemorySegment lseg) {
        long acc = 0;
        MemorySegment slice = heap.asSlice(4L * 5);
        for (int r = 0; r < WARM; r++) {
            int n = (r & 7) == 0 ? 13 + (r & 31) : N;
            acc += heapSum(heap, n) + sliceSum(slice, n - 5) + nativeSum(nat, n) + longSum(lseg, n);
            acc += heapSumWide(heap, n) + nativeSumWide(nat, n);
            acc += iotaSum(a, n) + iotaSub(a, n) + iotaMax(a, n) + iotaLongSum(b, n);
        }
        sink = acc;
    }

    // -------------------------------------------------------------- timing

    interface Op {
        long run();
    }

    static void time(String row, Op op) {
        int maxReps = Integer.getInteger("j27.maxreps", 20);
        int iters = 20_000;
        double[] ns = new double[maxReps];
        int reps = 0;
        boolean settled = false;
        long s = 0;
        while (reps < maxReps && !settled) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                s += op.run();
            }
            ns[reps++] = (System.nanoTime() - t0) / (double) iters;
            if (reps >= 3) {
                double lo = Math.min(ns[reps - 1], Math.min(ns[reps - 2], ns[reps - 3]));
                double hi = Math.max(ns[reps - 1], Math.max(ns[reps - 2], ns[reps - 3]));
                settled = hi <= lo * 1.10;
            }
        }
        sink += s;
        double[] last = Arrays.copyOfRange(ns, Math.max(0, reps - 3), reps);
        Arrays.sort(last);
        double[] all = Arrays.copyOf(ns, reps);
        Arrays.sort(all);
        System.out.printf(Locale.ROOT, "time %s %.1f ns/op spread %.2f settled %b reps %d%n",
                row, last[last.length / 2], all[all.length - 1] / all[0], settled, reps);
    }

    static void times(int[] a, long[] b, MemorySegment heap, MemorySegment nat, MemorySegment lseg) {
        time("heap-sum", () -> heapSum(heap, N));
        time("native-sum", () -> nativeSum(nat, N));
        time("long-sum", () -> longSum(lseg, N));
        time("heap-sum-wide", () -> heapSumWide(heap, N));
        time("native-sum-wide", () -> nativeSumWide(nat, N));
        time("iota-sum", () -> iotaSum(a, N));
        time("iota-max", () -> iotaMax(a, N));
    }

    public static void main(String[] args) {
        int[] a = ints();
        long[] b = longs();
        MemorySegment heap = MemorySegment.ofArray(a);
        MemorySegment lseg = MemorySegment.ofArray(b);
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment nat = arena.allocate(4L * N, 8);
            MemorySegment.copy(heap, 0, nat, 0, 4L * N);
            facts("cold", a, b, heap, nat, lseg);
            warm(a, b, heap, nat, lseg);
            facts("warm", a, b, heap, nat, lseg);
            if (Boolean.getBoolean("j27.time")) {
                times(a, b, heap, nat, lseg);
            }
        }
    }
}
