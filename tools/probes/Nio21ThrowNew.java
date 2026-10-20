/**
 * Round nio21 wave 20, lane L1: {@code throw new X(msg)} of a JDK throwable from hot compiled
 * code, the shape {@code CRATONVM_JIT_DIRECT_THROW_NEW} builds without interpreting the
 * constructor chain ({@code exceptions::jit_new_throwable_direct}), next to shapes it must refuse
 * (a user subclass's {@code super(msg)}, a JDK class whose chain is not a bare delegation).
 *
 * <p>The throwable is built in {@code make}, so its trace starts {@code make < throwIt < main}.
 *
 * <p>Conformance mode prints, per row, the cold answer (class, message, top two frames, cause,
 * suppressed count, {@code initCause} verdict) and then the hot loop's counts of throws that
 * differ from it; HotSpot prints {@code bad=0} on every row with or without
 * {@code -XX:-OmitStackTraceInFastThrow} (an explicit {@code new} is never a fast throw).
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21ThrowNew.java
 *   java -cp /tmp/p21 Nio21ThrowNew &gt; hs.txt
 *   cratonvm -cp /tmp/p21 Nio21ThrowNew                  # --jdk-only, must equal hs.txt
 *   cratonvm --compatible -cp /tmp/p21 Nio21ThrowNew
 *   CRATONVM_JIT_DIRECT_THROW_NEW=0 cratonvm -cp /tmp/p21 Nio21ThrowNew
 *   cratonvm -Dnio21.rows=ise,npe,user -cp /tmp/p21 Nio21ThrowNew bench   # ns/op per throw
 * </pre>
 */
public class Nio21ThrowNew {
    static final int N = Integer.getInteger("nio21.n", 100_000);

    static final class UserEx extends RuntimeException {
        UserEx(String m) {
            super(m);
        }
    }

    static final String[] ROWS = {"ise", "iae-null", "rte-empty", "npe", "uoe", "user", "ioobe-int"};

    static RuntimeException make(int row, int k) {
        switch (row) {
            case 0:
                return new IllegalStateException("s" + (k & 1));
            case 1:
                return new IllegalArgumentException((String) null);
            case 2:
                return new RuntimeException();
            case 3:
                return new NullPointerException("n" + (k & 1));
            case 4:
                return new UnsupportedOperationException("u");
            case 5:
                return new UserEx("x" + (k & 1));
            default:
                return new IndexOutOfBoundsException(k & 3);
        }
    }

    /** Thrown and caught in one frame, so the trace's top frame is {@code throwIt}. */
    static Throwable throwIt(int row, int k) {
        try {
            throw make(row, k);
        } catch (RuntimeException e) {
            return e;
        }
    }

    static String frame(StackTraceElement e) {
        return e.getMethodName() + ":" + e.getLineNumber();
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length > 2 ? frame(st[0]) + "<" + frame(st[1]) + "<" + frame(st[2])
                : "(" + st.length + " frames)";
    }

    /** The two frames a hot throw shares with the cold one ({@code make < throwIt}). */
    static String top2(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length > 1 ? frame(st[0]) + "<" + frame(st[1]) : "(" + st.length + " frames)";
    }

    static String describe(Throwable t) {
        String verdict;
        try {
            t.initCause(new Error("c"));
            verdict = "initCause=ok";
        } catch (IllegalStateException e) {
            verdict = "initCause=refused";
        }
        t.addSuppressed(new Error("s"));
        return t.getClass().getName() + " msg=" + t.getMessage() + " at=" + top(t)
                + " suppressed=" + t.getSuppressed().length + " " + verdict + " str=" + t;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("bench")) {
            for (String r : System.getProperty("nio21.rows", "ise,npe,user").split(",")) {
                int row = java.util.Arrays.asList(ROWS).indexOf(r.trim());
                long best = Long.MAX_VALUE;
                long acc = 0;
                for (int rep = 0; rep < 5; rep++) {
                    long t0 = System.nanoTime();
                    for (int k = 0; k < 200_000; k++) {
                        acc += throwIt(row, k).hashCode() & 1;
                    }
                    best = Math.min(best, System.nanoTime() - t0);
                }
                System.out.println(r + " " + (best / 200_000) + " ns/op" + (acc == -1 ? "!" : ""));
            }
            return;
        }
        StringBuilder out = new StringBuilder();
        for (int row = 0; row < ROWS.length; row++) {
            Throwable cold = throwIt(row, 1);
            String want = top2(cold);
            out.append("cold ").append(ROWS[row]).append(' ').append(describe(cold)).append('\n');
            int bad = 0;
            String firstBad = null;
            for (int k = 0; k < N; k++) {
                Throwable t = throwIt(row, 1);
                String got = t.getClass().getName() + " " + t.getMessage() + " " + top2(t)
                        + " " + (t.getCause() == null) + " " + t.getSuppressed().length;
                String exp = cold.getClass().getName() + " " + cold.getMessage() + " " + want
                        + " true 0";
                if (!got.equals(exp)) {
                    bad++;
                    if (firstBad == null) {
                        firstBad = got;
                    }
                }
            }
            out.append("hot ").append(ROWS[row]).append(" n=").append(N).append(" bad=").append(bad)
                    .append('\n');
            if (firstBad != null && !"0".equals(System.getProperty("nio21.dumpbad", "0"))) {
                out.append("  first-bad ").append(firstBad).append('\n');
            }
            Throwable last = throwIt(row, 1);
            out.append("last ").append(ROWS[row]).append(' ').append(describe(last)).append('\n');
        }
        System.out.print(out);
    }
}
