/** Does a JIT-compiled method still produce JEP 358's helpful NPE message?
 *
 *  Retiring `BigInteger`'s shadows moved six rows from a native (whose message
 *  was a hand-installed constant) to real bytecode, and with the JIT on those
 *  six lost their message: `NullPointerException: null` where HotSpot and this
 *  VM's own interpreter both name the field. `--nojit` on the same binary and
 *  the same probe is 0-diff on all six, so the message is lost in COMPILED code
 *  and the finding has nothing to do with `BigInteger`.
 *
 *  This asks it with no JDK class involved: the same call, cold and then hot.
 *  Each shape prints its cold answer, warms the method past the compile
 *  threshold with non-null arguments, and asks again -- so a difference between
 *  the two lines of a pair is the compiler and cannot be anything else.
 *
 *  Driven by `vm/tests/jit_npe_message_hot_equals_cold.rs`, which asserts
 *  exactly that: the hot row of every pair equals its cold row. It lived under
 *  `apps/probes/` while it was a shadow-campaign instrument; it moved to the
 *  tracked `probes/` home when it became a test's fixture, because `apps/` is
 *  gitignored and a fixture there is one a fresh checkout does not have (see
 *  `vm/tests/probe_fixture_census.rs` for the 23 that went that way).
 */
public class L2JitNpeProbe {
    static int rows = 0;
    /** Keeps a warm-up call's result from being dead code. */
    static int sink = 0;
    static final int WARM = 200_000;

    static class Holder {
        int value = 7;
        int[] arr = new int[3];
        String s = "x";
        int get() { return value; }
    }

    static void row(String label, Object v) {
        System.out.println(label + " |" + v + "|");
        rows++;
    }

    static String msg(Runnable r) {
        try {
            r.run();
            return "NO THROW";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    // Each shape is its own method so the JIT compiles it independently.
    static int readField(Holder h) { return h.value; }
    static int readArrayLen(Holder h) { return h.arr.length; }
    static int invokeOn(Holder h) { return h.get(); }
    static int invokeOnString(Holder h) { return h.s.length(); }
    static void writeField(Holder h) { h.value = 1; }

    /**
     * The sixth shape, and the one that reaches a different DOOR.
     *
     * The five above let the NPE escape the compiled method, so the
     * interpreter's post-JIT drain builds the throwable. This one CATCHES it in
     * the compiled method's own handler, where the throwable is built by
     * `jit::helpers::materialize_implicit_signal` instead — a second
     * constructor, on a path the drain never runs, that had the identical
     * defect and no probe that could see it.
     *
     * It reports the message itself rather than letting one escape, so it is a
     * `String`-returning shape and takes `pairDirect` rather than `pair`.
     *
     * <p>ORACLE NOTE. This is the one row whose HotSpot column depends on a
     * `-XX:` flag. `OmitStackTraceInFastThrow` is ON by default and lets C2
     * replace an implicit exception it catches in its OWN compiled body with a
     * shared, preallocated, message-less one — so default `java` prints
     * `null` here and `java -XX:-OmitStackTraceInFastThrow` prints the message,
     * cold and hot alike. CratonVM has no such optimisation, so the message is
     * the answer it must give; compare it against the flagged arm, as
     * `probes/StackTraceAfterOsr.java` does for the same flag and the same
     * reason.
     */
    /**
     * The three ARRAY shapes, and the half of this defect the page that filed
     * it did not ask about.
     *
     * They never lost their message outright -- the compiled null-check stub
     * records a JEP 358 ACTION code and the drain turned it into a
     * HotSpot-verbatim action half. What they lost was the `because "..." is
     * null` CLAUSE, silently, leaving a message that reads exactly like one
     * HotSpot would print when it cannot name the expression. That is worse
     * than a missing message, because a reader cannot tell the two apart.
     *
     * Kept as `int[]` shapes rather than folded into `Holder` so the trapping
     * opcode is the array one (`arraylength`, `iaload`, `iastore`) and not a
     * `getfield` reaching an array-typed field.
     */
    static int lenOf(int[] a) { return a.length; }
    static int loadOf(int[] a) { return a[0]; }
    static void storeTo(int[] a) { a[0] = 3; }

    static String caughtHere(Holder h) {
        try {
            return "NO THROW " + readField(h);
        } catch (NullPointerException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static void pair(String tag, java.util.function.Consumer<Holder> f) {
        row(tag + " cold", msg(() -> f.accept(null)));
        Holder live = new Holder();
        for (int i = 0; i < WARM; i++) {
            f.accept(live);
        }
        row(tag + " hot", msg(() -> f.accept(null)));
    }

    /** `pair` for a shape whose argument is an `int[]` rather than a `Holder`. */
    static void pairArray(String tag, java.util.function.Consumer<int[]> f) {
        row(tag + " cold", msg(() -> f.accept(null)));
        int[] live = new int[3];
        for (int i = 0; i < WARM; i++) {
            f.accept(live);
        }
        row(tag + " hot", msg(() -> f.accept(null)));
    }

    /** `pair` for a shape that reports its own answer instead of throwing. */
    static void pairDirect(String tag, java.util.function.Function<Holder, String> f) {
        row(tag + " cold", f.apply(null));
        Holder live = new Holder();
        for (int i = 0; i < WARM; i++) {
            f.apply(live);
        }
        row(tag + " hot", f.apply(null));
    }

    public static void main(String[] a) {
        pair("readField", h -> readField(h));
        pair("readArrayLen", h -> readArrayLen(h));
        pair("invokeOn", h -> invokeOn(h));
        pair("invokeOnString", h -> invokeOnString(h));
        pair("writeField", h -> writeField(h));
        pairDirect("caughtHere", h -> caughtHere(h));
        pairArray("lenOf", arr -> sink = lenOf(arr));
        pairArray("loadOf", arr -> sink = loadOf(arr));
        pairArray("storeTo", arr -> storeTo(arr));
        System.out.println("rows " + rows);
        System.out.println("DONE L2JitNpeProbe");
    }
}
