import org.jboss.logging.Logger;

public class JbossLogProbe {
    public static void main(String[] a) {
        Logger log = Logger.getLogger("com.example.Foo");

        System.out.println("logger impl=" + log.getClass().getName());
        System.out.println("isEnabled(TRACE)=" + log.isEnabled(Logger.Level.TRACE));
        System.out.println("isEnabled(DEBUG)=" + log.isEnabled(Logger.Level.DEBUG));
        System.out.println("isEnabled(INFO)=" + log.isEnabled(Logger.Level.INFO));
        System.out.println("-- emitting: only the INFO lines should appear --");
        log.tracef("TRACE-SHOULD-NOT-APPEAR %s", "x");
        log.debugf("DEBUG-SHOULD-NOT-APPEAR %s", "x");
        log.infof("INFO-fmt d=%d s=%s b=%b f=%.2f x=%x c=%c pct=%%", 42, "str", true, 3.14159, 255, 'Z');
        log.info("INFO-plain");
        System.out.println("== DONE OK ==");
    }
}
