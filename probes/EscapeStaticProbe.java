/**
 * An implicit trap raised inside a compiled callee, NOT caught there, must land
 * on the CALLER's own `catch`.
 *
 * `ExcTableDirectCallOracle` pins the other direction — the callee's OWN
 * handler — and finds nothing wrong with it in any arm. This is the direction
 * the ban that `direct_call_exc_table_publish_enabled` lifted actually named
 * ("a raw `CALL` has no Rust frame to notice the `i64::MIN` sentinel"), and it
 * is the one no probe covered.
 *
 * The driver's outer `try` COUNTS escapes rather than dying on the first, and
 * records the index of the first one, because the two failure shapes are
 * different diagnoses: `first` landing exactly on a tiering threshold and
 * `escapes` running to the end of the loop says the caller lost its handler
 * permanently when it was compiled, which is not a race and not a window.
 *
 * Keep this arm ALONE in `main`. An earlier version wrapped five call kinds in
 * one `switch` inside the loop and the static case stopped reproducing
 * entirely: the switch changes what `main`'s loop compiles to, and with it the
 * compile order that decides whether the callee is direct-bound at all. The
 * other kinds live in `EscapeKindProbe`.
 *
 *   java -cp out EscapeStaticProbe [n]
 */
public final class EscapeStaticProbe {

    static long sink;

    /** Declares a handler of the WRONG type, so the trap must propagate out. */
    static int guardedWrongType(int i) {
        try { return 10 / (i - i); }
        catch (IllegalStateException e) { return -1; }
    }

    static long callerGuarded(int i) {
        try { return guardedWrongType(i); } catch (ArithmeticException e) { return 7; }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        long got = 0;
        int escapes = 0, first = -1, last = -1;
        for (int i = 0; i < n; i++) {
            try {
                got += callerGuarded(i);
            } catch (ArithmeticException e) {
                escapes++;
                if (first < 0) { first = i; }
                last = i;
                got += 7;
            }
        }
        sink = got;
        System.out.println("n=" + n + " escapes=" + escapes + " first=" + first + " last=" + last
                + " total=" + got + " (want " + (7L * n) + ") "
                + (escapes == 0 ? "OK" : "CALLER-HANDLER-LOST"));
    }
}
