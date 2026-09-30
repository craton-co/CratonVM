import java.lang.reflect.Field;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;

/**
 * `Handler.getLevel()` / `setLevel(Level)` measured OFF THE REAL FIELDS, not
 * through each other: a self-consistent wrong slot is invisible to a round
 * trip. Needs --add-opens=java.logging/java.util.logging=ALL-UNNAMED.
 */
public class JulHandlerLevel {
    static int checks, bad;

    static void ck(String what, boolean ok, String detail) {
        checks++;
        if (!ok) { bad++; System.out.println("FAIL " + what + ": " + detail); }
    }

    static final class Sink extends Handler {
        public void publish(LogRecord r) {}
        public void flush() {}
        public void close() {}
    }

    static Object read(Handler h, String name) throws Exception {
        Field f = Handler.class.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(h);
    }

    public static void main(String[] a) throws Exception {
        Sink h = new Sink();

        // 1. A FRESH handler's default level is ALL, read without setLevel.
        ck("getLevel on a fresh Handler", Level.ALL.equals(h.getLevel()),
                "got=" + h.getLevel() + " want=ALL");

        // 2. setLevel must reach the REAL logLevel field.
        h.setLevel(Level.WARNING);
        Object logLevel = read(h, "logLevel");
        ck("setLevel reached Handler.logLevel", Level.WARNING.equals(logLevel),
                "logLevel=" + logLevel);

        // 3. and must NOT touch `manager` (slot 0).
        Object manager = read(h, "manager");
        ck("setLevel left Handler.manager alone", !(manager instanceof Level),
                "manager=" + manager);

        // 4. the round trip still agrees.
        ck("getLevel reads back setLevel", Level.WARNING.equals(h.getLevel()),
                "got=" + h.getLevel());

        h.setLevel(Level.SEVERE);
        ck("second setLevel", Level.SEVERE.equals(h.getLevel()), "got=" + h.getLevel());
        ck("second setLevel field", Level.SEVERE.equals(read(h, "logLevel")),
                "logLevel=" + read(h, "logLevel"));
        ck("manager still not a Level", !(read(h, "manager") instanceof Level),
                "manager=" + read(h, "manager"));

        // 5. isLoggable honours the level.
        ck("isLoggable(INFO) false at SEVERE", !h.isLoggable(new LogRecord(Level.INFO, "x")), "");
        ck("isLoggable(SEVERE) true at SEVERE", h.isLoggable(new LogRecord(Level.SEVERE, "x")), "");

        // 6. setLevel(null) is an NPE, not a silent write.
        String got = "<none>";
        try { h.setLevel(null); } catch (Throwable t) { got = t.getClass().getSimpleName(); }
        ck("setLevel(null) throws NPE", got.equals("NullPointerException"), "got=" + got);

        // §6 of the record: `Handler.manager` is a separate gap, reported not
        // asserted, because HotSpot's value is a real LogManager and this VM's
        // is whatever its own boot left there.
        Object mgr = read(h, "manager");
        System.out.println("CK JulHandlerLevel manager=" + (mgr == null ? "null" : mgr.getClass().getName()));
        System.out.println("CK JulHandlerLevel logManager=" + java.util.logging.LogManager.getLogManager().getClass().getName());
        System.out.println("JulHandlerLevel checks=" + checks + " bad=" + bad);
        if (bad == 0) { System.out.println("PASS JulHandlerLevel (" + checks + " checks)"); }
    }
}
