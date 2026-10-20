/**
 * Round nio21 wave 5, lane L2: an IMPLICIT exception -- a null array, an index out of bounds, a zero
 * integer divisor -- raised inside a {@code try} whose handler is in the same method, in every shape
 * the optimizing tier's implicit exception edges ({@code CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES},
 * default OFF; needs {@code CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1} too) take or decline.
 *
 * <p>The conf output must be identical under HotSpot 25 with its default
 * {@code -XX:+OmitStackTraceInFastThrow} AND with {@code -XX:-OmitStackTraceInFastThrow}, so:
 *
 * <ul>
 *   <li>{@code cold} lines print the exception class, the message and the top two frames, from the
 *       FIRST throws of each shape (interpreted in HotSpot, so never a fast throw);</li>
 *   <li>{@code hot} lines print only counts: every throw of the hot loop is checked against the
 *       cold answer for the same input, where a message must be the cold one OR null and a trace
 *       must have the cold top two frames OR be empty (HotSpot's preallocated fast-throw
 *       exceptions). A wrong message or a wrong line from compiled code is a non-zero {@code
 *       badmsg} / {@code badtrace}.</li>
 * </ul>
 *
 * <p>{@code hotmsg} mode prints the messages and top frames of the LAST hot throws themselves: diff
 * it only against {@code java -XX:-OmitStackTraceInFastThrow}.
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21ImplicitCatch.java
 *   java -cp /tmp/p21 Nio21ImplicitCatch &gt; hs.txt                                  # HotSpot
 *   java -XX:-OmitStackTraceInFastThrow -cp /tmp/p21 Nio21ImplicitCatch &gt; hs2.txt # must equal hs.txt
 *   E="CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES=1"
 *   env $E cratonvm -cp /tmp/p21 Nio21ImplicitCatch                  # --jdk-only, edges on
 *   env $E cratonvm --compatible -cp /tmp/p21 Nio21ImplicitCatch     # --compatible, edges on
 *   cratonvm -cp /tmp/p21 Nio21ImplicitCatch                          # edges off
 *   # the miss path: every lookup declines, the reason-9 exit builds and routes the throwable
 *   env $E CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS=0 cratonvm -cp /tmp/p21 Nio21ImplicitCatch
 *   # messages from compiled code (diff against -XX:-OmitStackTraceInFastThrow only)
 *   java -XX:-OmitStackTraceInFastThrow -cp /tmp/p21 Nio21ImplicitCatch hotmsg
 *   env $E cratonvm -cp /tmp/p21 Nio21ImplicitCatch hotmsg
 *   # engagement
 *   env $E CRATONVM_DBG_JITC=1 cratonvm -cp /tmp/p21 Nio21ImplicitCatch 2&gt;&amp;1 | grep ir-implicit-throw
 *   # bench rows (ns/op per caught throw), interleave arms and take medians
 *   env $E cratonvm -Dnio21.rows=aioobe,npe,div0,nothrow -cp /tmp/p21 Nio21ImplicitCatch bench
 *   # nio21 wave 6 L2: field rows, and the passing path with and without the pad's JMP
 *   env $E cratonvm -Dnio21.rows=fieldnpe,fieldok,nothrow -cp /tmp/p21 Nio21ImplicitCatch bench
 *   env $E CRATONVM_JIT_IR_IMPLICIT_PAD_FALLTHROUGH=0 cratonvm -Dnio21.rows=fieldok,nothrow  *       -cp /tmp/p21 Nio21ImplicitCatch bench
 * </pre>
 *
 * <p>nio21 wave 6, lane L2 added {@code getfield-npe}, {@code putfield-npe} (proposal L2W5-1) and
 * three {@code fence-*} rows: a handler that re-reads the array its trapping access failed on,
 * inside a nested {@code try}, so a check {@code ir_check_elim} wrongly elided there would read
 * through null or out of bounds instead of throwing ({@code fence-shared}: the handler two
 * accesses share, a merge neither true edge dominates).
 */
public class Nio21ImplicitCatch {
    static final int N = Integer.getInteger("nio21.n", 200_000);

    static Throwable last;
    static int fin;

    // ── the shapes: each catches its own implicit exception ──────────────

    static int loadAioobe(int[] a, int i) {
        try {
            return a[i];
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return -1;
        }
    }

    static int loadNpe(int[] a, int i) {
        try {
            return a[i];
        } catch (NullPointerException e) {
            last = e;
            return -2;
        }
    }

    static int storeAioobe(int[] a, int i, int v) {
        try {
            a[i] = v;
            return 0;
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return -1;
        }
    }

    static int storeNpe(long[] a, int i) {
        try {
            a[i] = 5L;
            return 0;
        } catch (NullPointerException e) {
            last = e;
            return -2;
        }
    }

    static int div0(int x, int y) {
        try {
            return x / y;
        } catch (ArithmeticException e) {
            last = e;
            return -3;
        }
    }

    static long ldiv0(long x, long y) {
        try {
            return x / y;
        } catch (ArithmeticException e) {
            last = e;
            return -3L;
        }
    }

    static int irem0(int x, int y) {
        try {
            return x % y;
        } catch (ArithmeticException e) {
            last = e;
            return -4;
        }
    }

    static long lrem0(long x, long y) {
        try {
            return x % y;
        } catch (ArithmeticException e) {
            last = e;
            return -4L;
        }
    }

    static int rtCatch(int[] a, int i) {
        try {
            return a[i] * 2;
        } catch (RuntimeException e) {
            last = e;
            return -5;
        }
    }

    static int ioobeCatch(byte[] a, int i) {
        try {
            return a[i];
        } catch (IndexOutOfBoundsException e) {
            last = e;
            return -6;
        }
    }

    static int twoRows(int[] a, int i) {
        try {
            return a[i];
        } catch (NullPointerException e) {
            last = e;
            return -7;
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return -8;
        }
    }

    static int stateBefore(int[] a, int i) {
        int s = 0;
        try {
            s = 1;
            int x = a[i];
            s = 2 + x;
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return s * 100;
        }
        return s;
    }

    static int storeNotDone(int[] b, int i) {
        try {
            b[0] = 7;
            b[i] = 9;
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return b[0] * 10 + b[1];
        }
        return -9;
    }

    static Object refLoad(Object[] a, int i) {
        Object o;
        try {
            o = a[i];
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            o = "dflt";
        }
        return o;
    }

    static double fpLoad(double[] a, int i) {
        try {
            return a[i];
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return -1.5;
        }
    }

    static int missInner(int[] a, int i) {
        try {
            return a[i];
        } catch (ArithmeticException e) {
            return -10;
        }
    }

    static int missOuter(int[] a, int i) {
        try {
            return missInner(a, i);
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return -11;
        }
    }

    static int finallyRow(int[] a, int i) {
        try {
            return a[i];
        } finally {
            fin++;
        }
    }

    static int finallyOuter(int[] a, int i) {
        try {
            return finallyRow(a, i);
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return -12;
        }
    }

    static final class Box {
        int v;
    }

    static int getfieldNpe(Box b) {
        try {
            return b.v;
        } catch (NullPointerException e) {
            last = e;
            return -16;
        }
    }

    static int putfieldNpe(Box b, int v) {
        try {
            b.v = v;
            return 0;
        } catch (NullPointerException e) {
            last = e;
            return -17;
        }
    }

    static int fenceNpe(int[] a, int i) {
        try {
            return a[i];
        } catch (NullPointerException e) {
            try {
                return a.length * 100;
            } catch (NullPointerException e2) {
                last = e2;
                return -13;
            }
        }
    }

    static int fenceAioobe(int[] a, int i) {
        try {
            return a[i];
        } catch (ArrayIndexOutOfBoundsException e) {
            try {
                return a[i] + 1;
            } catch (ArrayIndexOutOfBoundsException e2) {
                last = e2;
                return -14;
            }
        }
    }

    static int fenceShared(int[] a, int[] b, int i) {
        try {
            int x = a[i];
            int y = b[i];
            return x + y;
        } catch (RuntimeException e) {
            try {
                return a.length * 100;
            } catch (NullPointerException e2) {
                last = e2;
                return -15;
            }
        }
    }

    static int loopCatch(int[] a, int n) {
        int s = 0;
        for (int k = 0; k < n; k++) {
            try {
                s += a[k];
            } catch (ArrayIndexOutOfBoundsException e) {
                last = e;
                s -= 1000;
            }
        }
        return s;
    }

    // ── the dispatcher: cold and hot reach every shape through it, so the
    // ── second frame of every trace is `call`, at one line per shape ─────

    static final String[] ROWS = {
        "load-aioobe", "load-npe", "store-aioobe", "store-npe", "idiv", "ldiv", "irem", "lrem",
        "catch-runtime", "catch-ioobe", "two-rows", "state-before", "store-not-done", "aaload",
        "daload", "miss", "finally", "loop", "getfield-npe", "putfield-npe", "fence-npe",
        "fence-aioobe", "fence-shared",
    };

    static final int[] A3 = {10, 20, 30};
    static final byte[] B3 = {1, 2, 3};
    static final double[] D3 = {1.25, 2.5, 3.75};
    static final Object[] O3 = {"x", "y", "z"};
    static final int[] B2 = new int[2];

    /** {@code bad}: 0 = no throw, 1 = the first failing input, 2 = the second. */
    static long call(int row, int bad, int k) {
        int good = k % 3;
        int idx = bad == 0 ? good : (bad == 1 ? 5 : -1);
        switch (row) {
            case 0:
                return loadAioobe(A3, idx);
            case 1:
                return loadNpe(bad == 0 ? A3 : null, good);
            case 2:
                return storeAioobe(new int[3], idx, k);
            case 3:
                return storeNpe(bad == 0 ? new long[3] : null, good);
            case 4:
                return div0(k + 7, bad == 0 ? good + 1 : 0);
            case 5:
                return ldiv0(k + 7L, bad == 0 ? good + 1L : 0L);
            case 6:
                return irem0(k + 7, bad == 0 ? good + 1 : 0);
            case 7:
                return lrem0(k + 7L, bad == 0 ? good + 1L : 0L);
            case 8:
                return bad == 2 ? rtCatch(null, good) : rtCatch(A3, idx);
            case 9:
                return ioobeCatch(B3, idx);
            case 10:
                return bad == 2 ? twoRows(null, good) : twoRows(A3, idx);
            case 11:
                return stateBefore(A3, idx);
            case 12:
                B2[0] = 0;
                B2[1] = 3;
                return storeNotDone(B2, bad == 0 ? 1 : (bad == 1 ? 5 : -1));
            case 13: {
                Object o = refLoad(O3, idx);
                return o.hashCode();
            }
            case 14:
                return (long) (fpLoad(D3, idx) * 4);
            case 15:
                return missOuter(A3, idx);
            case 16:
                return finallyOuter(A3, idx);
            case 17:
                return loopCatch(A3, bad == 0 ? 3 : 3 + bad);
            case 18: {
                Box b = new Box();
                b.v = k;
                return getfieldNpe(bad == 0 ? b : null);
            }
            case 19:
                return putfieldNpe(bad == 0 ? new Box() : null, k);
            case 20:
                return fenceNpe(bad == 0 ? A3 : null, good);
            case 21:
                return fenceAioobe(A3, idx);
            default:
                // bad == 2: the SECOND access fails, `a` is fine.
                return bad == 2 ? fenceShared(A3, null, good)
                        : fenceShared(bad == 0 ? A3 : null, A3, good);
        }
    }

    /** The cold answers: class, message and top two frames per (row, bad). */
    static final Throwable[][] COLD = new Throwable[ROWS.length][3];

    static String frame(StackTraceElement e) {
        return e.getMethodName() + ":" + e.getLineNumber();
    }

    static String describe(Throwable t) {
        if (t == null) {
            return "none";
        }
        StackTraceElement[] st = t.getStackTrace();
        String top = st.length > 1 ? frame(st[0]) + "<" + frame(st[1]) : "(" + st.length + " frames)";
        return t.getClass().getName() + " msg=" + t.getMessage() + " at=" + top;
    }

    static void cold(StringBuilder out) {
        for (int row = 0; row < ROWS.length; row++) {
            for (int bad = 1; bad <= 2; bad++) {
                last = null;
                long r;
                try {
                    r = call(row, bad, 1);
                } catch (Throwable t) {
                    out.append("cold ").append(ROWS[row]).append(" bad=").append(bad)
                            .append(" ESCAPED ").append(describe(t)).append('\n');
                    continue;
                }
                COLD[row][bad] = last;
                out.append("cold ").append(ROWS[row]).append(" bad=").append(bad).append(" r=")
                        .append(r).append(' ').append(describe(last)).append('\n');
            }
        }
    }

    /**
     * Diagnostics only: {@code -Dnio21.dumpbad=1} (or {@code =true}) prints, on STDOUT after the
     * conformance lines, the first bad trace of the run. nio21 wave 20 L1: this was
     * {@code Boolean.getBoolean}, which is true only for the text {@code "true"}, so {@code =1}
     * silently disabled the dump (the w19 hunt's "empty stderr" was the probe, not the VM).
     */
    static final boolean DUMP_BAD = dumpBadProperty();

    static boolean dumpBadProperty() {
        String v = System.getProperty("nio21.dumpbad");
        return v != null && (v.equals("1") || v.equalsIgnoreCase("true"));
    }

    /** The first bad trace of the run, recorded on the (rare) bad path whatever the property. */
    static String firstBad;

    static void recordBad(int pass, int row, int k, int bad, Throwable t, StackTraceElement[] st,
            Throwable cold) {
        if (firstBad != null) {
            return;
        }
        StringBuilder b = new StringBuilder();
        b.append("BADTRACE pass=").append(pass).append(" row=").append(ROWS[row]).append(" k=")
                .append(k).append(" bad=").append(bad).append(" class=")
                .append(t.getClass().getName()).append(" msg=").append(t.getMessage())
                .append(" depth=").append(st.length).append('\n');
        for (StackTraceElement e : st) {
            b.append("  got  ").append(e).append('\n');
        }
        for (StackTraceElement e : cold.getStackTrace()) {
            b.append("  want ").append(e).append('\n');
        }
        // Whose trace is it? A trace equal to another (row, bad)'s cold one names a stale
        // per-address trace record; none names a mis-built capture.
        for (int r = 0; r < ROWS.length; r++) {
            for (int bb = 1; bb <= 2; bb++) {
                Throwable c = COLD[r][bb];
                if (c != null && sameFrames(st, c.getStackTrace())) {
                    b.append("  matches-cold ").append(ROWS[r]).append(" bad=").append(bb)
                            .append('\n');
                }
            }
        }
        firstBad = b.toString();
    }

    static int pass;

    static boolean sameFrames(StackTraceElement[] a, StackTraceElement[] b) {
        return a.length > 1 && b.length > 1
                && frame(a[0]).equals(frame(b[0])) && frame(a[1]).equals(frame(b[1]));
    }

    static void hot(StringBuilder out, boolean printLast) {
        for (int row = 0; row < ROWS.length; row++) {
            long sum = 0;
            int throwsSeen = 0;
            int badClass = 0;
            int badMsg = 0;
            int badTrace = 0;
            Throwable lastHot = null;
            fin = 0;
            for (int k = 0; k < N; k++) {
                int bad = (k & 15) != 0 ? 0 : ((k & 16) == 0 ? 1 : 2);
                last = null;
                sum += call(row, bad, k);
                if (bad == 0) {
                    if (last != null) {
                        badClass++;
                    }
                    continue;
                }
                Throwable cold = COLD[row][bad];
                Throwable t = last;
                if (cold == null) {
                    continue;
                }
                throwsSeen++;
                if (t == null || t.getClass() != cold.getClass()) {
                    badClass++;
                    continue;
                }
                lastHot = t;
                String m = t.getMessage();
                if (m != null && !m.equals(cold.getMessage())) {
                    badMsg++;
                }
                StackTraceElement[] st = t.getStackTrace();
                if (st.length != 0 && !sameFrames(st, cold.getStackTrace())) {
                    badTrace++;
                    recordBad(pass, row, k, bad, t, st, cold);
                }
            }
            out.append("hot ").append(ROWS[row]).append(" calls=").append(N).append(" throws=")
                    .append(throwsSeen).append(" sum=").append(sum).append(" fin=").append(fin)
                    .append(" badclass=").append(badClass).append(" badmsg=").append(badMsg)
                    .append(" badtrace=").append(badTrace).append('\n');
            if (printLast) {
                out.append("hotmsg ").append(ROWS[row]).append(' ').append(describe(lastHot))
                        .append('\n');
            }
        }
    }

    // ── bench rows: one caught throw per op ──────────────────────────────

    static long benchRow(String name, int reps) {
        long acc = 0;
        int ops = 1_000_000;
        long best = Long.MAX_VALUE;
        int id = name.equals("aioobe") ? 0 : name.equals("npe") ? 1 : name.equals("div0") ? 2
                : name.equals("fieldnpe") ? 4 : name.equals("fieldok") ? 5 : 3;
        Box box = new Box();
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            for (int k = 0; k < ops; k++) {
                switch (id) {
                    case 0:
                        acc += loadAioobe(A3, 5);
                        break;
                    case 1:
                        acc += loadNpe(null, 1);
                        break;
                    case 2:
                        acc += div0(k, 0);
                        break;
                    case 4:
                        acc += getfieldNpe(null);
                        break;
                    case 5:
                        box.v = k;
                        acc += getfieldNpe(box);
                        break;
                    default:
                        acc += loadAioobe(A3, k % 3);
                        break;
                }
            }
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
        }
        System.out.println(name + " " + (best / ops) + " ns/op");
        return acc;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        StringBuilder out = new StringBuilder();
        if (mode.equals("bench")) {
            String rows = System.getProperty("nio21.rows", "aioobe,npe,div0,nothrow");
            long acc = 0;
            for (String r : rows.split(",")) {
                acc += benchRow(r.trim(), 5);
            }
            if (acc == 42) {
                System.out.println("unlikely");
            }
            return;
        }
        cold(out);
        pass = 1;
        hot(out, mode.equals("hotmsg"));
        pass = 2;
        hot(out, false);
        System.out.print(out);
        if (DUMP_BAD) {
            System.out.print(firstBad == null ? "BADTRACE none\n" : firstBad);
        }
    }
}
