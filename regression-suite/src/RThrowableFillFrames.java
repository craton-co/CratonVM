/**
 * Regression: the two rules that decide what a throwable's stack trace STARTS
 * with, and the message an `invokevirtual` on a null ARRAY owes.
 *
 * Both were found by arming `CRATONVM_ENFORCE_NATIVE_SHADOW` over the throwable
 * family — lane T's cross-cutting registrar — and both turned out to be
 * present-tense defects in BOTH compatibility modes rather than artefacts of
 * the arming. See
 * `docs/internal/jdk-only/lane-t-cross-cutting-registrars-...` for the wave.
 *
 * 1. `Throwable.fillInStackTrace()` is REAL bytecode in every mode (only the
 *    private `fillInStackTrace(int)` it calls is a native), so an explicit
 *    `t.fillInStackTrace()` captured its own `Throwable.fillInStackTrace` frame
 *    and reported it as the throw site. HotSpot's `fill_in_stack_trace` skips
 *    exactly two prefixes of the innermost end: `fillInStackTrace*` frames and
 *    then `<init>` frames, in both cases only where the throwable `is_a` the
 *    frame's holder.
 *
 * 2. An array-typed call site with a null receiver
 *    (`String[] a = null; a.clone()`) never reached the interpreter's
 *    null-receiver arm — the array branch is chosen before it — so it fell
 *    through to `native_object_clone`, whose bare `clone on null` is all a
 *    native can say. `arraylength` and `aaload` on the same null field were
 *    already right, which is why this one row hid.
 *
 * Every assertion here is a HotSpot-comparable invariant: frame CONTENT is not
 * (line numbers and internal frames differ legitimately), so this asks only for
 * the top frame's declaring class and method name, which the specification
 * fixes, and for NPE messages, which JEP 358 fixes.
 */
public class RThrowableFillFrames {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length == 0 ? "EMPTY" : st[0].getClassName() + "." + st[0].getMethodName();
    }

    /** A named frame so "the throw site" is a name the assertion can state. */
    static Throwable buildHere() { return new IllegalStateException("built"); }

    static Throwable makeApp() { return new AppEx("app"); }

    static Throwable makeDeepApp() { return new DeepAppEx("deep"); }

    /** A throwable whose own constructor calls `fillInStackTrace()` again. */
    static class Refills extends RuntimeException {
        Refills(String m) { super(m); fillInStackTrace(); }
    }

    /** An ORDINARY application exception: its `<init>` is real bytecode calling
     *  `super(m)`, and that native `super` is where the trace is captured. This
     *  is the shape every framework's exception hierarchy has, which is why the
     *  `<init>` half of the skip is not a niche rule. */
    static class AppEx extends RuntimeException {
        AppEx(String m) { super(m); }
    }

    /** Two levels of application constructor above the capture. */
    static class DeepAppEx extends AppEx {
        DeepAppEx(String m) { super(m); }
    }

    /** A subclass that OVERRIDES `fillInStackTrace` — HotSpot skips the
     *  override too, because the skip is keyed on the method name and on the
     *  throwable being an instance of the frame's holder, not on the holder
     *  being `java.lang.Throwable`. */
    static class Overrides extends RuntimeException {
        Overrides(String m) { super(m); }
        @Override public synchronized Throwable fillInStackTrace() {
            return super.fillInStackTrace();
        }
    }

    static String[] nullStrings;
    static StackTraceElement[] nullElements;
    static int[] nullInts;

    static String npe(Runnable r) {
        try { r.run(); return "NO-THROW"; }
        catch (NullPointerException e) { return String.valueOf(e.getMessage()); }
        catch (Throwable t) { return "WRONG-TYPE " + t.getClass().getName(); }
    }

    public static void main(String[] args) {
        // --- 1. the frame skip -------------------------------------------
        check(top(buildHere()).equals("RThrowableFillFrames.buildHere"),
              "constructor capture starts at the throw site, got " + top(buildHere()));

        Throwable refilled = new RuntimeException("r");
        refilled.fillInStackTrace();
        check(top(refilled).equals("RThrowableFillFrames.main"),
              "explicit fillInStackTrace() starts at its CALLER, got " + top(refilled));

        // Twice in a row: the skip is a prefix walk, so a second call must not
        // leave one frame behind where the first left none.
        refilled.fillInStackTrace();
        refilled.fillInStackTrace();
        check(top(refilled).equals("RThrowableFillFrames.main"),
              "repeated fillInStackTrace() is idempotent, got " + top(refilled));

        Throwable ctorRefill = new Refills("c");
        check(top(ctorRefill).equals("RThrowableFillFrames.main"),
              "fillInStackTrace() from a constructor starts at `new`, got " + top(ctorRefill));

        Throwable overridden = new Overrides("o");
        overridden.fillInStackTrace();
        check(top(overridden).equals("RThrowableFillFrames.main"),
              "an OVERRIDDEN fillInStackTrace is skipped too, got " + top(overridden));

        // An application exception subclass: `AppEx.<init>` is a real bytecode
        // frame above the capture, and it is `<init>` of a class the throwable
        // IS an instance of, so phase 2 of the skip drops it. Before the skip
        // existed, EVERY application exception in every framework reported its
        // own constructor as the throw site.
        check(top(makeApp()).equals("RThrowableFillFrames.makeApp"),
              "an application exception starts at `new`, got " + top(makeApp()));
        check(top(makeDeepApp()).equals("RThrowableFillFrames.makeDeepApp"),
              "two constructor levels are both skipped, got " + top(makeDeepApp()));
        try {
            throw new DeepAppEx("t");
        } catch (Throwable t) {
            check(top(t).equals("RThrowableFillFrames.main"),
                  "a thrown application exception starts at the throw, got " + top(t));
        }
        // ... and the frame BELOW the skipped ones is still there, so the skip
        // is a prefix and not a filter that ate the caller too.
        check(makeApp().getStackTrace().length >= 2,
              "the caller of the throw site survives the skip");

        // A trace is still non-empty after the skip — a skip that ate every
        // frame would satisfy nothing above and everything about "no wrong
        // frame at the top".
        check(refilled.getStackTrace().length > 0, "the skip did not empty the trace");

        // `getStackTrace` is not the only reader: `printStackTrace`'s first
        // `\tat ` line comes from the same array and must agree with it.
        java.io.StringWriter sw = new java.io.StringWriter();
        refilled.printStackTrace(new java.io.PrintWriter(sw, true));
        String firstAt = "";
        for (String line : sw.toString().split("\n")) {
            if (line.trim().startsWith("at ")) { firstAt = line.trim(); break; }
        }
        check(firstAt.startsWith("at RThrowableFillFrames.main"),
              "printStackTrace's first frame agrees with getStackTrace()[0], got " + firstAt);

        // --- 2. the null-array invoke message ----------------------------
        check(npe(() -> nullStrings.clone()).equals(
                  "Cannot invoke \"[Ljava.lang.String;.clone()\" "
                  + "because \"RThrowableFillFrames.nullStrings\" is null"),
              "String[] clone on null names the array type and the field, got "
              + npe(() -> nullStrings.clone()));
        check(npe(() -> nullElements.clone()).equals(
                  "Cannot invoke \"[Ljava.lang.StackTraceElement;.clone()\" "
                  + "because \"RThrowableFillFrames.nullElements\" is null"),
              "StackTraceElement[] clone on null, got " + npe(() -> nullElements.clone()));
        check(npe(() -> nullInts.clone()).equals(
                  "Cannot invoke \"[I.clone()\" "
                  + "because \"RThrowableFillFrames.nullInts\" is null"),
              "int[] clone on null uses the descriptor spelling, got "
              + npe(() -> nullInts.clone()));
        // The two opcodes that were ALREADY right, kept here so a change that
        // fixes `clone` by breaking them cannot pass.
        check(npe(() -> { int n = nullStrings.length; }).equals(
                  "Cannot read the array length because "
                  + "\"RThrowableFillFrames.nullStrings\" is null"),
              "arraylength on null is unchanged");
        check(npe(() -> { String s = nullStrings[0]; }).equals(
                  "Cannot load from object array because "
                  + "\"RThrowableFillFrames.nullStrings\" is null"),
              "aaload on null is unchanged");
        // And the type is still NPE, not a native's own error: a message fix
        // that changed the exception would pass every string assertion above.
        check(npe(() -> nullStrings.clone()).startsWith("Cannot invoke"),
              "a null-array clone is still a NullPointerException");

        System.out.println("PASS RThrowableFillFrames (" + checks + " checks)");
    }
}
