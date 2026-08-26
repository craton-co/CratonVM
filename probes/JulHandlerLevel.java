import java.util.logging.*;

/** Does Handler.getLevel()/setLevel() read the real `logLevel` field?
 *
 *  On the real JDK java.util.logging.Handler declares, in order:
 *    0 manager(LogManager) 1 filter 2 formatter 3 logLevel 4 errorManager 5 encoding
 *  so a native reading SLOT 0 answers the LogManager, and one WRITING slot 0
 *  overwrites it. Both are observable from Java with no VM internals.
 *
 *  Deliberately NOT ConsoleHandler: constructing one reaches
 *  Charset.newEncoder(), which is a separate --jdk-only defect
 *  (AbstractMethodError, no Code attribute) and would stop this probe before it
 *  measured anything. A bare subclass with no-op sinks isolates the field. */
public class JulHandlerLevel {
    static int checks = 0, bad = 0;

    static final class Probe extends Handler {
        public void publish(LogRecord r) { }
        public void flush() { }
        public void close() { }
    }

    static void eq(String what, Object got, Object want) {
        checks++;
        boolean ok = (want == null) ? got == null : want.equals(got);
        if (!ok) { bad++; System.out.println("  DIFF " + what + ": got=" + got + " want=" + want); }
        else System.out.println("  ok   " + what + " = " + got);
    }

    public static void main(String[] a) throws Exception {
        Handler h = new Probe();

        // 0. a fresh Handler inherits: getLevel() is Level.ALL on the real JDK.
        eq("getLevel on a fresh Handler", h.getLevel(), Level.ALL);

        // 1. round-trip: what we set is what we get.
        h.setLevel(Level.WARNING);
        eq("getLevel after setLevel(WARNING)", h.getLevel(), Level.WARNING);
        h.setLevel(Level.FINE);
        eq("getLevel after setLevel(FINE)", h.getLevel(), Level.FINE);

        // 2. the returned object must BE a Level, not something else typed as one.
        Object lvl = h.getLevel();
        checks++;
        if (lvl != null && !(lvl instanceof Level)) {
            bad++; System.out.println("  DIFF getLevel returned a " + lvl.getClass().getName());
        } else System.out.println("  ok   getLevel returns a Level: "
                                  + (lvl == null ? "null" : lvl.getClass().getName()));

        // 3. isLoggable consults logLevel -- an INDEPENDENT reader of the same
        //    field, running real JDK bytecode. If it disagrees with getLevel(),
        //    the two are not reading the same slot.
        h.setLevel(Level.SEVERE);
        eq("isLoggable(INFO) with level=SEVERE", h.isLoggable(new LogRecord(Level.INFO, "m")), Boolean.FALSE);
        eq("isLoggable(SEVERE) with level=SEVERE", h.isLoggable(new LogRecord(Level.SEVERE, "m")), Boolean.TRUE);

        // 4. setLevel(null) must throw -- the real body's first statement.
        checks++;
        try { h.setLevel(null); bad++; System.out.println("  DIFF setLevel(null) did not throw"); }
        catch (NullPointerException e) { System.out.println("  ok   setLevel(null) threw NPE"); }

        // 5. did a slot-0 write damage a NEIGHBOURING field? getErrorManager()
        //    and getFormatter() read slots 4 and 2 of the same object.
        h.setLevel(Level.ALL);
        checks++;
        try {
            Object em = h.getErrorManager();
            System.out.println("  ok   getErrorManager() after setLevel: "
                               + (em == null ? "null" : em.getClass().getName()));
        } catch (Throwable t) { bad++; System.out.println("  DIFF getErrorManager() threw " + t); }

        // 6. THE WRITE SIDE, read straight off the real field. If setLevel wrote
        //    slot 0 instead of `logLevel`, then `logLevel` is still its default
        //    and `manager` now holds a Level.
        try {
            java.lang.reflect.Field fl = Handler.class.getDeclaredField("logLevel");
            java.lang.reflect.Field fm = Handler.class.getDeclaredField("manager");
            fl.setAccessible(true); fm.setAccessible(true);
            Handler w = new Probe();
            w.setLevel(Level.WARNING);
            Object realLevel = fl.get(w), realManager = fm.get(w);
            checks++;
            if (!Level.WARNING.equals(realLevel)) {
                bad++;
                System.out.println("  DIFF setLevel(WARNING) did not reach Handler.logLevel: logLevel=" + realLevel);
            } else System.out.println("  ok   Handler.logLevel after setLevel = " + realLevel);
            checks++;
            if (realManager instanceof Level) {
                bad++;
                System.out.println("  DIFF setLevel OVERWROTE Handler.manager with a Level: " + realManager);
            } else System.out.println("  ok   Handler.manager intact: "
                                      + (realManager == null ? "null" : realManager.getClass().getName()));
        } catch (Throwable t) {
            System.out.println("  n/a  reflective field check unavailable: " + t);
        }

        System.out.println(bad == 0 ? "PASS JulHandlerLevel (" + checks + " checks)"
                                    : "FAIL JulHandlerLevel (" + bad + " of " + checks + " wrong)");
    }
}
