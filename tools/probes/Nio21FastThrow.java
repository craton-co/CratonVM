/**
 * Round nio21 wave 16, lane L5: HotSpot's {@code OmitStackTraceInFastThrow} contract.
 *
 * <p>With its default {@code -XX:+OmitStackTraceInFastThrow}, HotSpot's C2 recompiles an implicit
 * exception site that keeps throwing (null check, range check, zero divisor, cast check, array
 * store check) to throw ONE preallocated instance per kind: no message, no stack trace, built
 * without a constructor (so {@code cause} and {@code suppressedExceptions} are null: {@code
 * initCause} throws {@code IllegalStateException}, {@code addSuppressed} is dropped, {@code
 * fillInStackTrace} is a no-op), and the same object every time. With {@code
 * -XX:-OmitStackTraceInFastThrow} every throw is a fresh, fully built exception.
 *
 * <p>Each row throws one kind from one site {@code nio21.n} times (default 300 000, far past
 * HotSpot's and CratonVM's hot thresholds), caught by a handler in the same method, and prints
 * only identity-independent facts about the LAST throw, then decorates it and prints the facts of
 * one more throw from the same site. The output must equal HotSpot 25's under BOTH flags:
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21FastThrow.java
 *   java -cp /tmp/p21 Nio21FastThrow &gt; hs-on.txt
 *   java -XX:-OmitStackTraceInFastThrow -cp /tmp/p21 Nio21FastThrow &gt; hs-off.txt
 *   cratonvm -cp /tmp/p21 Nio21FastThrow                                    # == hs-on.txt
 *   cratonvm --compatible -cp /tmp/p21 Nio21FastThrow                       # == hs-on.txt
 *   cratonvm -XX:-OmitStackTraceInFastThrow -cp /tmp/p21 Nio21FastThrow     # == hs-off.txt
 *   cratonvm --compatible -XX:-OmitStackTraceInFastThrow -cp /tmp/p21 Nio21FastThrow  # == hs-off.txt
 *   CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=0 cratonvm -cp /tmp/p21 Nio21FastThrow    # == hs-off.txt
 *   # r9w9 arm (fresh stackless object per throw): differs from hs-on.txt on same= / initCause=
 *   CRATONVM_FAST_THROW_SHARED_INSTANCE=0 cratonvm -cp /tmp/p21 Nio21FastThrow
 * </pre>
 *
 * <p>{@code -Dnio21.rows=npe,aioobe,div0,cce,ase} selects rows.
 */
public class Nio21FastThrow {
    static final int N = Integer.getInteger("nio21.n", 300_000);

    static final int[] A2 = new int[2];
    static final Object[] STRINGS = new String[2];
    static Object boxed = Integer.valueOf(7);
    static Throwable last;

    static int npe(int[] a) {
        try {
            return a[0];
        } catch (NullPointerException e) {
            last = e;
            return -1;
        }
    }

    static int aioobe(int[] a, int i) {
        try {
            return a[i];
        } catch (ArrayIndexOutOfBoundsException e) {
            last = e;
            return -1;
        }
    }

    static int div0(int x, int y) {
        try {
            return x / y;
        } catch (ArithmeticException e) {
            last = e;
            return -1;
        }
    }

    static int cce(Object o) {
        try {
            return ((String) o).length();
        } catch (ClassCastException e) {
            last = e;
            return -1;
        }
    }

    static int ase(Object[] a, Object v) {
        try {
            a[0] = v;
            return 0;
        } catch (ArrayStoreException e) {
            last = e;
            return -1;
        }
    }

    /** One throw of {@code row}; returns the exception it caught. */
    static Throwable once(String row, int k) {
        last = null;
        int r;
        switch (row) {
            case "npe":
                r = npe(null);
                break;
            case "aioobe":
                r = aioobe(A2, 2 + (k & 1));
                break;
            case "div0":
                r = div0(k, 0);
                break;
            case "cce":
                r = cce(boxed);
                break;
            default:
                r = ase(STRINGS, boxed);
                break;
        }
        if (r != -1) {
            throw new AssertionError(row + " did not throw");
        }
        return last;
    }

    static String facts(Throwable t, Throwable prev) {
        return "class=" + t.getClass().getName()
                + " msg=" + (t.getMessage() == null ? "null" : "set")
                + " trace=" + t.getStackTrace().length
                + " same=" + (t == prev)
                + " cause=" + (t.getCause() == null ? "null" : "set")
                + " suppressed=" + t.getSuppressed().length;
    }

    static void row(String row, StringBuilder out) {
        Throwable prev = null;
        Throwable cur = null;
        for (int k = 0; k < N; k++) {
            prev = cur;
            cur = once(row, k);
        }
        out.append(row).append(" last ").append(facts(cur, prev)).append('\n');

        // Decorate the last throw. HotSpot's preallocated instance refuses initCause (its cause is
        // null, not `this`), drops addSuppressed (suppressedExceptions is null), and ignores
        // fillInStackTrace (stackTrace and backtrace are both null).
        String init;
        try {
            cur.initCause(new RuntimeException("x"));
            init = "ok";
        } catch (IllegalStateException e) {
            init = "IllegalStateException";
        }
        cur.addSuppressed(new RuntimeException("y"));
        int suppressed = cur.getSuppressed().length;
        cur.fillInStackTrace();
        int filled = cur.getStackTrace().length;
        out.append(row).append(" decorate initCause=").append(init)
                .append(" suppressed=").append(suppressed)
                .append(" filled=").append(filled > 0 ? "frames" : "empty").append('\n');

        // One more throw from the same site: under the fast throw it is the same object, and
        // nothing the decoration tried may show on it.
        Throwable next = once(row, N);
        out.append(row).append(" next ").append(facts(next, cur)).append('\n');
    }

    public static void main(String[] args) {
        String rows = System.getProperty("nio21.rows", "npe,aioobe,div0,cce,ase");
        StringBuilder out = new StringBuilder();
        for (String r : rows.split(",")) {
            row(r.trim(), out);
        }
        System.out.print(out);
    }
}
