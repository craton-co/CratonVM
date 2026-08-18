/**
 * Does an exception raised OUTSIDE every protected range of an OSR'd method get
 * caught by a handler that does not cover it?
 *
 * Since the RBC.6b lift (2026-08-17) a method with an exception table can be
 * OSR-compiled. When the OSR'd body raises an exception,
 * `route_osr_exception_out_of_artifact` decides where it goes; with no precise
 * reason-9 frame stashed it correctly deduces "the throw site is outside every
 * protected range" and answers `Propagate`. But `OsrBackoffOutcome::ThrowJava`
 * then hands the throwable to the dispatch loop's `unwind_to_handler` keyed on
 * `entry_pc` — the BACK-EDGE the OSR'd body was ENTERED at, not the throw site.
 *
 * When the loop sits inside the `try` (`try { for (..) {..} } catch`), that
 * back-edge IS inside a protected range. So the unwinder searches this frame's
 * table at a pc the throw never reached, finds the `catch`, and enters it —
 * with the stale pre-OSR locals, since the OSR'd body advanced its own copies
 * and never wrote them back.
 *
 * `afterLoop` is that shape reduced to one method:
 *
 *   try { for (...) { work } }        <- the back edge, INSIDE the range
 *   catch (RuntimeException e) { ... }
 *   trip();                           <- throws, OUTSIDE every range
 *
 * Java says the `trip()` throw is NOT caught here: it is textually after the
 * `try` block. So the correct answer is `caught=0 escaped=1`, and HotSpot is the
 * control that says so. `caught=1` is the bug — an exception swallowed by a
 * handler that does not guard it.
 *
 * The method is called ONCE, which is what makes it an OSR question at all: a
 * second call would open the method-entry tier-up door and the OSR path would
 * stop being the one under test.
 *
 *   javac -d . OsrThrowOutsideTryProbe.java
 *   java     -cp . OsrThrowOutsideTryProbe 400000        # the control
 *   cratonvm --java-home <jdk> -cp . OsrThrowOutsideTryProbe 400000
 *   CRATONVM_JIT_OSR_EXC_TABLE=0 cratonvm … OsrThrowOutsideTryProbe 400000
 */
public final class OsrThrowOutsideTryProbe {
    static long sink;
    static int caught;
    static int escaped;

    static final RuntimeException E = new RuntimeException("outside") {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static int leaf(int i) { return i + 1; }

    /** Throws unconditionally; a callee so the throw is not an inline `athrow`. */
    static void trip() { throw E; }

    /**
     * The loop is INSIDE the `try`, so the back edge the OSR entry uses is a pc
     * the `catch` covers. The throw is AFTER the `try`, so it is a pc the
     * `catch` does not cover.
     */
    static void afterLoop(int n) {
        // The accumulator is a LOCAL and the static write happens after the
        // loop. A `putstatic` inside the protected range is refused admission
        // outright (`osr-DENY (osr-exc-site-unpublished pc=.. opcode=0xb3)`),
        // which would make this arm measure the interpreter in every binary —
        // the vacuous green this probe exists to avoid.
        long a = 0;
        try {
            for (int i = 0; i < n; i++) {
                a += leaf(i);
            }
        } catch (RuntimeException e) {
            caught++;          // must stay 0: nothing inside the try throws
        }
        sink += a;
        trip();                // outside every protected range — must NOT be caught
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        try {
            afterLoop(n);
        } catch (RuntimeException e) {
            if (e == E) {
                escaped++;     // correct: the throw left the method uncaught
            }
        }
        System.out.println("caught=" + caught + " escaped=" + escaped + " sink=" + sink);
        System.out.println(caught == 0 && escaped == 1
                ? "PASS the throw outside the try was not caught by it"
                : "FAIL a handler that does not cover the throw site took it");
    }
}
