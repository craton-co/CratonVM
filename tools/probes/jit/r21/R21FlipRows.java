// JIT round 21 wave 24, lane flipprep: the probe rows `r21-optin-flip-review-20261019.md` says
// are missing before the opt-in perf switches are flipped.
//
//  (e) #21 / #22 CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD + CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM:
//      a 9..64-element primitive array that is unused on the hot path (only its folded length is
//      read) but LIVE across a rare arm that transfers to the interpreter (the rare arm loads and
//      initialises a class nothing has touched, `LateN`, so an optimizing compile has an
//      uncommon trap there whose frame state names the array). The elimination must refuse it
//      (a frame-state reference is a `Guard` use, not `Dead`), and the interpreter must find a
//      zeroed array of the right length after the transfer. Controls: 8 and 65 elements (outside
//      the 9..64 band), a dead array of every primitive type, and a dead `new int[-1]`, which
//      must still throw NegativeArraySizeException (a constant length <= 64 that is negative).
//  (b) #3 / #20 CRATONVM_JIT_IR_ALLOC_PREFETCH + CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC:
//      a multi-threaded allocation row. THREADS threads allocate byte[16] / Object[16] / an
//      object with fields in a tight loop, check that every fresh allocation is zeroed / null,
//      stamp it with the thread's mark, keep the last 256 in a per-thread ring, and after the
//      join re-check every ring entry still carries its own thread's mark (two threads' TLABs
//      never overlap, a prefetch past a TLAB end writes nothing). `time mt-*` rows: wall ns per
//      allocation over all threads (PREFETCHW past a TLAB end can steal a neighbour's line; this
//      is the A/B the review asks for before the flip).
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21FlipRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21FlipRows                          # ZGC (the default collector)
//   cratonvm -XX:+UseG1GC -Xmx512m -cp $P R21FlipRows
//   cratonvm -XX:+UseGenerationalGC -Xmx512m -cp $P R21FlipRows
//   cratonvm --compatible -Xmx512m -cp $P R21FlipRows             # both modes, same lines
//   cratonvm --nojit -Xmx512m -cp $P R21FlipRows                  # interpreter reference
//   java -Xmx512m -cp $P R21FlipRows                              # HotSpot 25
// Arms (cratonvm, each must print the same non-`time` lines as the default and as HotSpot):
//   CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1 CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1   (e), together
//   CRATONVM_JIT_IR_ALLOC_PREFETCH=1                                                (b), every GC
//   CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=1                                  (b), Gen / G1
//   all four at once (the flip set)
// Engagement (NOT for timing): CRATONVM_DBG_SCALAR_DEOPT=1 must print NO `MATERIALIZE` for a
// deopt-* row's array (the array was never elided); CRATONVM_DBG_JITC=1 prints the per-site
// `ir inline-TLAB newarray EMITTED` / `DECLINED: <why>` lines for the mt-* kernels.
//
// HotSpot expectation (by reading; C2 keeps an array a deopt state names, and eliminates dead
// arrays of at most EliminateAllocationArraySizeLimit = 64 elements):
//   row deopt-int32 rare=40032 warm=1280000      (rare: one element of a zeroed array set, then
//   row deopt-byte9 rare=16 warm=360000           sum + length; warm: WARM * length)
//   row deopt-long64 rare=40064 warm=2560000
//   row deopt-double17 rare=40017 warm=680000
//   row deopt-char48 rare=113 warm=1920000
//   row deopt-bool64 rare=65 warm=2560000
//   row deopt-short8 rare=13 warm=320000
//   row deopt-int65 rare=40065 warm=2600000
//   row dead-int64 warm=799980000                 (and dead-byte64 / dead-long64 / dead-double64)
//   row dead-negative caught=40000 class=java.lang.NegativeArraySizeException msg=-1
//   row late-hits 22222222
//   row mt-byte16 threads=4 bad=0 ring=130560     (and mt-objarr16 / mt-fields)
//   acc true
// Every non-`time` line is identical in every arm, mode and collector; `time` lines are timings
// (dead-* rows ~1 ns under C2, mt-* rows a few ns per allocation, scaled by the core count).

public class R21FlipRows {
    static final int WARM = 40_000;

    // Classes the rare arms load for the first time (one per row, so every rare arm really loads).
    static final class Late1 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late2 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late3 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late4 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late5 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late6 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late7 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late8 { static int hits; static { hits = 1; } static void touch() { hits++; } }

    // ---------------------------------------------------------------- (e) deopt rows

    static long sum(int[] a) {
        long s = 0;
        for (int v : a) {
            s += v;
        }
        return s;
    }

    static long sum(byte[] a) {
        long s = 0;
        for (byte v : a) {
            s += v;
        }
        return s;
    }

    static long sum(long[] a) {
        long s = 0;
        for (long v : a) {
            s += v;
        }
        return s;
    }

    static long sum(double[] a) {
        double s = 0;
        for (double v : a) {
            s += v;
        }
        return (long) s;
    }

    static long sum(char[] a) {
        long s = 0;
        for (char v : a) {
            s += v;
        }
        return s;
    }

    static long sum(short[] a) {
        long s = 0;
        for (short v : a) {
            s += v;
        }
        return s;
    }

    static long sum(boolean[] a) {
        long s = 0;
        for (boolean v : a) {
            s += v ? 1 : 0;
        }
        return s;
    }

    // Each kernel: the array's only hot-path use is its length (the fold's target); the rare arm
    // is the deopt point whose state names the array, then the interpreter writes and reads it.
    static long deoptInt32(int i, boolean rare) {
        int[] a = new int[32];
        int len = a.length;
        if (rare) {
            Late1.touch();
            a[i & 31] = i;
            return sum(a) + len;
        }
        return len;
    }

    static long deoptByte9(int i, boolean rare) {
        byte[] a = new byte[9];
        int len = a.length;
        if (rare) {
            Late2.touch();
            a[i % 9] = 7;
            return sum(a) + len;
        }
        return len;
    }

    static long deoptLong64(int i, boolean rare) {
        long[] a = new long[64];
        int len = a.length;
        if (rare) {
            Late3.touch();
            a[63] = i;
            return sum(a) + len;
        }
        return len;
    }

    static long deoptDouble17(int i, boolean rare) {
        double[] a = new double[17];
        int len = a.length;
        if (rare) {
            Late4.touch();
            a[16] = i;
            return sum(a) + len;
        }
        return len;
    }

    static long deoptChar48(int i, boolean rare) {
        char[] a = new char[48];
        int len = a.length;
        if (rare) {
            Late5.touch();
            a[0] = 'A';
            return sum(a) + len;
        }
        return len;
    }

    static long deoptBool64(int i, boolean rare) {
        boolean[] a = new boolean[64];
        int len = a.length;
        if (rare) {
            Late6.touch();
            a[i & 63] = true;
            return sum(a) + len;
        }
        return len;
    }

    // Controls just outside the band: 8 (scalar-replacement territory) and 65 (past the limit).
    static long deoptShort8(int i, boolean rare) {
        short[] a = new short[8];
        int len = a.length;
        if (rare) {
            Late7.touch();
            a[7] = 5;
            return sum(a) + len;
        }
        return len;
    }

    static long deoptInt65(int i, boolean rare) {
        int[] a = new int[65];
        int len = a.length;
        if (rare) {
            Late8.touch();
            a[64] = i;
            return sum(a) + len;
        }
        return len;
    }

    interface Row {
        long run(int i, boolean rare);
    }

    static void row(String name, Row r) {
        long warm = 0;
        for (int i = 0; i < WARM; i++) {
            warm += r.run(i, false);
        }
        long rare = r.run(WARM, true);
        System.out.println("row " + name + " rare=" + rare + " warm=" + warm);
    }

    // ---------------------------------------------------------------- (e) dead arrays

    static int deadInt64(int i) {
        int[] a = new int[64];
        return i + a.length - 64;
    }

    static int deadByte64(int i) {
        byte[] a = new byte[64];
        return i + a.length - 64;
    }

    static int deadLong64(int i) {
        long[] a = new long[64];
        return i + a.length - 64;
    }

    static int deadDouble64(int i) {
        double[] a = new double[64];
        return i + a.length - 64;
    }

    static int deadNegative(int i) {
        int[] a = new int[-1];
        return i + a.length;
    }

    interface Dead {
        int run(int i);
    }

    static void dead(String name, Dead d) {
        long warm = 0;
        for (int i = 0; i < WARM; i++) {
            warm += d.run(i);
        }
        System.out.println("row " + name + " warm=" + warm);
    }

    static void deadNegativeRow() {
        int caught = 0;
        String cls = "none";
        String msg = "none";
        for (int i = 0; i < WARM; i++) {
            try {
                deadNegative(i);
            } catch (NegativeArraySizeException e) {
                caught++;
                cls = e.getClass().getName();
                msg = String.valueOf(e.getMessage());
            }
        }
        System.out.println("row dead-negative caught=" + caught + " class=" + cls + " msg=" + msg);
    }

    // ---------------------------------------------------------------- (b) multi-threaded allocation

    static final int THREADS = 4;
    static final int RING = 256;
    static final int CHUNK = 65_536; // a multiple of RING, so the ring's last pass is whole
    static final int CHUNKS = 16;
    static final Object[] MARKS = new Object[THREADS];

    static {
        for (int t = 0; t < THREADS; t++) {
            MARKS[t] = "mark" + t;
        }
    }

    static final class P {
        int a;
        long b;
        Object c;
        byte d;
    }

    // Each kernel: `n` allocations; returns the number of fresh allocations that were not zeroed.
    static int byte16(int t, Object[] ring, int n) {
        int bad = 0;
        for (int i = 0; i < n; i++) {
            byte[] a = new byte[16];
            for (int k = 0; k < 16; k++) {
                bad += a[k];
            }
            a[0] = (byte) (t + 1);
            a[15] = (byte) i;
            ring[i & (RING - 1)] = a;
        }
        return bad;
    }

    static int objArr16(int t, Object[] ring, int n) {
        int bad = 0;
        Object mark = MARKS[t];
        for (int i = 0; i < n; i++) {
            Object[] a = new Object[16];
            for (int k = 0; k < 16; k++) {
                if (a[k] != null) {
                    bad++;
                }
            }
            a[0] = mark;
            a[15] = Integer.valueOf(i & 0xff);
            ring[i & (RING - 1)] = a;
        }
        return bad;
    }

    static int fields(int t, Object[] ring, int n) {
        int bad = 0;
        Object mark = MARKS[t];
        for (int i = 0; i < n; i++) {
            P p = new P();
            if (p.a != 0 || p.b != 0 || p.c != null || p.d != 0) {
                bad++;
            }
            p.a = i & 0xff;
            p.c = mark;
            ring[i & (RING - 1)] = p;
        }
        return bad;
    }

    interface Kernel {
        int run(int t, Object[] ring, int n);
    }

    // The ring after the join: every entry carries its own thread's mark, and the per-slot
    // payload is the slot index (the last pass over the ring is whole), so each thread's ring
    // sums to 0 + 1 + ... + 255 = 32640. A mismatch counts as bad.
    static int checkRing(String kind, int t, Object[] ring, long[] ringSum) {
        int bad = 0;
        long s = 0;
        for (int j = 0; j < RING; j++) {
            Object o = ring[j];
            if (kind.equals("byte16")) {
                byte[] a = (byte[]) o;
                if (a.length != 16 || a[0] != (byte) (t + 1)) {
                    bad++;
                }
                s += a[15] & 0xff;
            } else if (kind.equals("objarr16")) {
                Object[] a = (Object[]) o;
                if (a.length != 16 || a[0] != MARKS[t]) {
                    bad++;
                }
                s += (Integer) a[15];
            } else {
                P p = (P) o;
                if (p.c != MARKS[t]) {
                    bad++;
                }
                s += p.a;
            }
        }
        ringSum[t] = s;
        return bad;
    }

    static long mt(String kind, Kernel k, boolean report) throws InterruptedException {
        Object[][] rings = new Object[THREADS][RING];
        int[] bad = new int[THREADS];
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int tt = t;
            ts[t] = new Thread(() -> {
                int b = 0;
                for (int c = 0; c < CHUNKS; c++) {
                    b += k.run(tt, rings[tt], CHUNK);
                }
                bad[tt] = b;
            }, "flip-alloc-" + t);
        }
        long t0 = System.nanoTime();
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long elapsed = System.nanoTime() - t0;
        if (report) {
            long[] ringSum = new long[THREADS];
            int totalBad = 0;
            long total = 0;
            for (int t = 0; t < THREADS; t++) {
                totalBad += bad[t] + checkRing(kind, t, rings[t], ringSum);
                total += ringSum[t];
            }
            System.out.println("row mt-" + kind + " threads=" + THREADS + " bad=" + totalBad + " ring=" + total);
        }
        return elapsed;
    }

    static void warmKernel(Kernel k) {
        Object[] ring = new Object[RING];
        for (int w = 0; w < 200; w++) {
            k.run(w % THREADS, ring, RING);
        }
    }

    // ---------------------------------------------------------------- time rows

    static final int REPS = 5;
    static int acc;

    static void timeDead(String name, Dead d) {
        int n = 2_000_000;
        long[] t = new long[REPS];
        for (int w = 0; w < 2; w++) {
            for (int i = 0; i < n; i++) {
                acc += d.run(i);
            }
        }
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                acc += d.run(i);
            }
            t[r] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(t);
        System.out.printf(java.util.Locale.ROOT, "time %s %.2f ns/op%n", name, (double) t[REPS / 2] / n);
    }

    static void timeMt(String kind, Kernel k) throws InterruptedException {
        long[] t = new long[REPS];
        mt(kind, k, false);
        for (int r = 0; r < REPS; r++) {
            t[r] = mt(kind, k, false);
        }
        java.util.Arrays.sort(t);
        double perAlloc = (double) t[REPS / 2] / ((long) THREADS * CHUNKS * CHUNK);
        System.out.printf(java.util.Locale.ROOT, "time mt-%s-t%d %.2f ns/op%n", kind, THREADS, perAlloc);
    }

    public static void main(String[] args) throws Exception {
        row("deopt-int32", R21FlipRows::deoptInt32);
        row("deopt-byte9", R21FlipRows::deoptByte9);
        row("deopt-long64", R21FlipRows::deoptLong64);
        row("deopt-double17", R21FlipRows::deoptDouble17);
        row("deopt-char48", R21FlipRows::deoptChar48);
        row("deopt-bool64", R21FlipRows::deoptBool64);
        row("deopt-short8", R21FlipRows::deoptShort8);
        row("deopt-int65", R21FlipRows::deoptInt65);
        dead("dead-int64", R21FlipRows::deadInt64);
        dead("dead-byte64", R21FlipRows::deadByte64);
        dead("dead-long64", R21FlipRows::deadLong64);
        dead("dead-double64", R21FlipRows::deadDouble64);
        deadNegativeRow();
        System.out.println("row late-hits " + Late1.hits + Late2.hits + Late3.hits + Late4.hits
                + Late5.hits + Late6.hits + Late7.hits + Late8.hits);

        Kernel byte16 = R21FlipRows::byte16;
        Kernel objArr16 = R21FlipRows::objArr16;
        Kernel fields = R21FlipRows::fields;
        warmKernel(byte16);
        warmKernel(objArr16);
        warmKernel(fields);
        mt("byte16", byte16, true);
        mt("objarr16", objArr16, true);
        mt("fields", fields, true);

        timeDead("dead-int64", R21FlipRows::deadInt64);
        timeDead("dead-long64", R21FlipRows::deadLong64);
        timeMt("byte16", byte16);
        timeMt("objarr16", objArr16);
        timeMt("fields", fields);
        System.out.println("acc " + (acc != 0x7fffffff));
    }
}
