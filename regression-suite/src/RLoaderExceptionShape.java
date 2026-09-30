/**
 * Regression: a `ClassNotFoundException` the class loader mints is a
 * CONSTRUCTED throwable, not a bag of fields.
 *
 * Lane T's retirement of the throwable family made `Throwable.getMessage()`
 * real bytecode and found that the loader's natives write the detail message
 * into slot 0 by hand (`alloc_single_message_exception`). Fixing the message
 * left the wider shape untouched and the lane recorded it as a residual: an
 * object that never ran `<init>` has no stack trace, so
 *
 *     try { Class.forName("no.such.Klass"); } catch (ClassNotFoundException e)
 *
 * handed the application an exception whose `getStackTrace()` was EMPTY. Every
 * logging framework prints that trace; a plugin loader that reports "which of
 * my callers asked for this class" reads it. HotSpot's is the real call stack.
 *
 * Frame CONTENT is not HotSpot-comparable (the loader's internal frames and
 * line numbers differ legitimately between the two VMs), so this asks only for
 * what the specification fixes: that the trace is non-empty, that it reaches
 * the application frame that made the call, that it bottoms out in `main`, and
 * that the message/`toString`/cause shape is the one `ClassNotFoundException`'s
 * own constructor produces — `super(s, null)`, which is why `initCause` on a
 * caught one throws `IllegalStateException`. That last check is the sharpest:
 * a hand-filled object leaves `cause == this` and lets `initCause` succeed.
 */
public class RLoaderExceptionShape {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    static boolean reaches(Throwable t, String method) {
        for (StackTraceElement e : t.getStackTrace()) {
            if (e.getClassName().equals("RLoaderExceptionShape")
                && e.getMethodName().equals(method)) {
                return true;
            }
        }
        return false;
    }

    static String bottom(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length == 0 ? "EMPTY"
             : st[st.length - 1].getClassName() + "." + st[st.length - 1].getMethodName();
    }

    /** A named frame, so "the caller that asked for the class" is a name. */
    static void askForwardName() throws ClassNotFoundException {
        Class.forName("no.such.Klass");
    }

    static void askLoader() throws ClassNotFoundException {
        RLoaderExceptionShape.class.getClassLoader().loadClass("no.such.Other");
    }

    static void askThreeArg() throws ClassNotFoundException {
        Class.forName("no.such.Third", true, RLoaderExceptionShape.class.getClassLoader());
    }

    static String initCauseOutcome(Throwable t) {
        try {
            t.initCause(new RuntimeException("x"));
            return "ACCEPTED";
        } catch (IllegalStateException e) {
            return "IllegalStateException";
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    static void shape(Throwable e, String asker, String name) {
        check(e.getMessage() != null && e.getMessage().equals(name),
              asker + ": message is the class name, got " + e.getMessage());
        check(e.toString().equals("java.lang.ClassNotFoundException: " + name),
              asker + ": toString carries the message, got " + e.toString());
        check(e.getCause() == null, asker + ": cause is null, got " + e.getCause());
        // The residual this vector exists for.
        check(e.getStackTrace().length > 0, asker + ": the trace is not empty");
        check(reaches(e, asker), asker + ": the trace reaches its caller, got " + bottom(e));
        check(bottom(e).equals("RLoaderExceptionShape.main"),
              asker + ": the trace bottoms out in main, got " + bottom(e));
        // `ClassNotFoundException(String)` is `super(s, null)`: the cause is
        // SET (to null), so a second one is refused. A hand-filled object
        // leaves the `cause == this` sentinel and accepts it.
        check(initCauseOutcome(e).equals("IllegalStateException"),
              asker + ": initCause is refused, got " + initCauseOutcome(e));
    }

    public static void main(String[] args) throws Exception {
        try { askForwardName(); throw new AssertionError("forName found no.such.Klass"); }
        catch (ClassNotFoundException e) { shape(e, "askForwardName", "no.such.Klass"); }

        try { askLoader(); throw new AssertionError("loadClass found no.such.Other"); }
        catch (ClassNotFoundException e) { shape(e, "askLoader", "no.such.Other"); }

        try { askThreeArg(); throw new AssertionError("forName/3 found no.such.Third"); }
        catch (ClassNotFoundException e) { shape(e, "askThreeArg", "no.such.Third"); }

        // The same object, printed: `printStackTrace`'s first line is
        // `toString`, and the lines after it are the trace the checks above
        // assert exists.
        try { askForwardName(); }
        catch (ClassNotFoundException e) {
            java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
            e.printStackTrace(new java.io.PrintStream(bytes, true, "UTF-8"));
            String[] lines = bytes.toString("UTF-8").split("\\R");
            check(lines[0].equals("java.lang.ClassNotFoundException: no.such.Klass"),
                  "printStackTrace's first line is toString, got " + lines[0]);
            check(lines.length > 1 && lines[1].trim().startsWith("at "),
                  "printStackTrace prints at least one frame, got " + lines.length + " lines");
        }

        System.out.println("PASS RLoaderExceptionShape (" + checks + " checks)");
    }
}
