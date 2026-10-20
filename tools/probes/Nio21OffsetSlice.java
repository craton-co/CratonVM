// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 6, lane L5: loops over an array SLICE, `a[i + off]` with `off`
// loop-invariant and not a constant -- the shape of `HeapByteBuffer.put`'s
// `hb[p + offset]` and of every `buf[off + i]` copy loop. The optimizing tier
// now plants, at the loop entry, `n <= a.length - off`, `0 <= off` and
// `0 <= start` guards that deopt to the loop header
// (`CRATONVM_JIT_IR_OFFSET_PREDICATE`), and `ir_check_elim` drops the
// per-element bounds check under them (`CRATONVM_JIT_IR_BCE_VAR_OFFSET`).
// Switches (each arm must print the same `conf` text):
//   CRATONVM_JIT_IR_OFFSET_PREDICATE=0   no entry guards (the checks stay)
//   CRATONVM_JIT_IR_BCE_VAR_OFFSET=0     guards planted, the arm that reads them off
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes): for
//          each kernel and each (offset, count, start) case -- in range,
//          off by one at either end, a negative offset, a negative start, an
//          offset whose sum with the index wraps past Integer.MAX_VALUE --
//          the result or the exception class and message, and a hash of the
//          destination (the elements written before an exception must be
//          exactly HotSpot's). The table runs cold, after a warm-up on
//          in-range cases (so the compiled loops carry the entry guards and
//          the failing cases trap them), and again after the failing cases
//          have run enough times to withdraw the guards (the de-spec
//          registry). All three passes must print the same lines.
//   rows   `<row> <N> ns/op` lines:
//          slice-sum    sum of a 4096-int slice at offset 7
//          slice-fill   fill of a 4096-byte slice at offset 3
//          slice-copy   char -> byte copy of 4096 elements between two slices
//
//   java Nio21OffsetSlice conf
//   java -Dnio21.rows=slice-sum,slice-fill,slice-copy Nio21OffsetSlice rows

public class Nio21OffsetSlice {
    static final int HOT = 30_000;
    static volatile long sink;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "slice-sum,slice-fill,slice-copy"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- kernels

    /** `for (i = 0; i < n; i++) s += a[i + off]`. */
    static long sum(int[] a, int off, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += a[i + off];
        }
        return s;
    }

    /** `while (p < limit) b[p + off] = v + p, p++` from `start`: a buffer's put loop. */
    static int fill(byte[] b, int off, int start, int limit) {
        int p = start;
        while (p < limit) {
            b[p + off] = (byte) (p * 7 + 1);
            p++;
        }
        return p;
    }

    /** `dst[doff + i] = (byte) src[soff + i]` for `i < n`. */
    static int copy(char[] src, int soff, byte[] dst, int doff, int n) {
        int i = 0;
        for (; i < n; i++) {
            dst[doff + i] = (byte) src[soff + i];
        }
        return i;
    }

    // --------------------------------------------------------------- conf

    static long hash(byte[] b) {
        long h = 17;
        for (byte x : b) {
            h = h * 31 + x;
        }
        return h;
    }

    static long hash(int[] a) {
        long h = 17;
        for (int x : a) {
            h = h * 31 + x;
        }
        return h;
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static final int[][] SUM_CASES = {
        // {off, n}
        {0, 16}, {3, 13}, {3, 14}, {-1, 4}, {16, 0}, {16, 1}, {Integer.MAX_VALUE, 1},
        {Integer.MAX_VALUE, 2}, {5, -3},
    };

    static final int[][] FILL_CASES = {
        // {off, start, limit}
        {0, 0, 16}, {4, 0, 12}, {4, 0, 13}, {4, 2, 12}, {-2, 2, 10}, {-2, 1, 10},
        {1, -1, 5}, {Integer.MAX_VALUE, 0, 1}, {Integer.MAX_VALUE - 3, 2, 6}, {0, 9, 3},
    };

    static final int[][] COPY_CASES = {
        // {soff, doff, n}
        {0, 0, 16}, {2, 5, 11}, {2, 5, 12}, {6, 0, 11}, {-1, 0, 3}, {0, -1, 3},
        {Integer.MAX_VALUE, 0, 1}, {0, Integer.MAX_VALUE, 1}, {3, 3, 0},
    };

    static void table(String pass) {
        System.out.println("== " + pass);
        int[] a = new int[16];
        for (int i = 0; i < a.length; i++) {
            a[i] = i * i - 5;
        }
        for (int[] c : SUM_CASES) {
            String r;
            try {
                r = "= " + sum(a, c[0], c[1]);
            } catch (Throwable t) {
                r = "! " + describe(t);
            }
            System.out.println("sum off=" + c[0] + " n=" + c[1] + " " + r);
        }
        for (int[] c : FILL_CASES) {
            byte[] b = new byte[16];
            String r;
            try {
                r = "= " + fill(b, c[0], c[1], c[2]);
            } catch (Throwable t) {
                r = "! " + describe(t);
            }
            System.out.println("fill off=" + c[0] + " start=" + c[1] + " limit=" + c[2] + " " + r
                    + " #" + hash(b));
        }
        char[] src = new char[16];
        for (int i = 0; i < src.length; i++) {
            src[i] = (char) ('a' + i);
        }
        for (int[] c : COPY_CASES) {
            byte[] dst = new byte[16];
            String r;
            try {
                r = "= " + copy(src, c[0], dst, c[1], c[2]);
            } catch (Throwable t) {
                r = "! " + describe(t);
            }
            System.out.println("copy soff=" + c[0] + " doff=" + c[1] + " n=" + c[2] + " " + r
                    + " #" + hash(dst));
        }
    }

    static void warmInRange() {
        int[] a = new int[64];
        byte[] b = new byte[64];
        char[] src = new char[64];
        long s = 0;
        for (int k = 0; k < HOT; k++) {
            int off = k & 7;
            s += sum(a, off, 40 + (k & 3));
            s += fill(b, off, k & 1, 30 + (k & 7));
            s += copy(src, off, b, (k >> 3) & 7, 33);
        }
        sink = s;
    }

    static void conf() {
        table("cold");
        warmInRange();
        table("warm");
        // Every failing case again, enough times for the per-bci de-spec
        // limit, then re-warm so the next compile runs without the guards.
        for (int r = 0; r < 12; r++) {
            table("quiet");
            warmInRange();
        }
        table("despec");
        System.out.println("h " + hash(new int[] {1, 2, 3}));
    }

    // --------------------------------------------------------------- rows

    static void rows(String which) {
        for (String row : which.split(",")) {
            switch (row) {
                case "slice-sum": {
                    int[] a = new int[4096 + 16];
                    bench(row, () -> sum(a, 7, 4096));
                    break;
                }
                case "slice-fill": {
                    byte[] b = new byte[4096 + 16];
                    bench(row, () -> fill(b, 3, 0, 4096));
                    break;
                }
                case "slice-copy": {
                    char[] src = new char[4096 + 16];
                    byte[] dst = new byte[4096 + 16];
                    bench(row, () -> copy(src, 5, dst, 9, 4096));
                    break;
                }
                default:
                    System.out.println(row + " unknown");
            }
        }
    }

    interface Op {
        long run();
    }

    static void bench(String row, Op op) {
        long s = 0;
        for (int i = 0; i < HOT; i++) {
            s += op.run();
        }
        int n = 20_000;
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                s += op.run();
            }
            best = Math.min(best, (System.nanoTime() - t0) / n);
        }
        sink = s;
        System.out.println(row + " " + best + " ns/op");
    }
}
