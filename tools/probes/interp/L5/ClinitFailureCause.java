/*
 * Interpreter round i1, wave 7, lane L5: since JDK 21 (JDK-8048190) the
 * "Could not initialize class" NoClassDefFoundError carries, as its cause, an
 * ExceptionInInitializerError recorded when the class's initialization failed
 * (InstanceKlass::add_initialization_error), whose message names the original
 * exception and the thread: `Exception <class>: <message> [in thread "<name>"]`.
 * A class that fails because its SUPERCLASS's initialization failed (JVMS 5.5
 * step 7) records that exception -- here the superclass's NoClassDefFoundError.
 *
 * No setup; plain --compatible, with and without --nojit.
 *
 * Expected (HotSpot 25, from the HotSpot source; the orchestrator's HotSpot run
 * is the reference):
 *
 *   first: java.lang.ExceptionInInitializerError
 *   first cause: java.lang.RuntimeException: boom
 *   second: java.lang.NoClassDefFoundError: Could not initialize class ClinitFailureCause$Bad
 *   second cause: java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: boom [in thread "main"]
 *   second cause trace: ClinitFailureCause$Bad.boom ClinitFailureCause$Bad.<clinit> ClinitFailureCause.main
 *   sub first: java.lang.NoClassDefFoundError: Could not initialize class ClinitFailureCause$Bad
 *   sub first cause: java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: boom [in thread "main"]
 *   sub second: java.lang.NoClassDefFoundError: Could not initialize class ClinitFailureCause$Sub
 *   sub second cause: java.lang.ExceptionInInitializerError: Exception java.lang.NoClassDefFoundError: Could not initialize class ClinitFailureCause$Bad [in thread "main"]
 *
 * Before wave 7 CratonVM printed "cause: null" for every NoClassDefFoundError.
 * The "second cause trace" line is wave 8 (lane L5): HotSpot gives the
 * recorded error the ORIGINAL exception's stack trace
 * (java_lang_Throwable::create_initialization_error); CratonVM used to give it
 * the current site's, "ClinitFailureCause.main" alone.
 */
public class ClinitFailureCause {
    static class Bad {
        static int x = boom();

        static int boom() {
            throw new RuntimeException("boom");
        }
    }

    static class Sub extends Bad {
        static int y = 1;
    }

    static void show(String label, Throwable t) {
        System.out.println(label + ": " + t);
        System.out.println(label + " cause: " + t.getCause());
    }

    public static void main(String[] args) {
        try {
            new Bad();
        } catch (Throwable t) {
            show("first", t);
        }
        try {
            new Bad();
        } catch (Throwable t) {
            show("second", t);
            StringBuilder trace = new StringBuilder("second cause trace:");
            for (StackTraceElement e : t.getCause().getStackTrace()) {
                trace.append(' ').append(e.getClassName()).append('.').append(e.getMethodName());
            }
            System.out.println(trace);
        }
        try {
            new Sub();
        } catch (Throwable t) {
            show("sub first", t);
        }
        try {
            new Sub();
        } catch (Throwable t) {
            show("sub second", t);
        }
    }
}
