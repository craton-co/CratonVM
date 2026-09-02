import java.util.*;
import java.util.concurrent.TimeUnit;

/**
 * Do enums actually work?
 *
 * Four failing tests in `vm/src/vm/tests.rs` were triaged as one
 * "enum-constant cluster" on 2026-09-02 and the grouping was WRONG — all four
 * turned out to be stale tests (two pass `null` where the JDK requires
 * non-null and get HotSpot's exact NPE back; two address an enum CONSTANT as a
 * zero-arg native, which is a retired shape). That refutes the cluster but
 * answers nothing about enums, because none of those tests was exercising one.
 *
 * This asks the question directly. Every row is a fixed string; diff against
 * HotSpot. `--synthetic-jdk` is the mode under suspicion, since it has no real
 * `java.base` bytecode to fall back on.
 */
public final class SyntheticEnumSurface {
    enum Local { ALPHA, BETA, GAMMA }

    public static void main(String[] args) {
        row("values().length", () -> String.valueOf(Local.values().length));
        row("values() order", () -> Arrays.toString(Local.values()));
        row("name/ordinal", () -> Local.BETA.name() + "/" + Local.BETA.ordinal());
        row("valueOf", () -> Local.valueOf("GAMMA").toString());
        row("valueOf(bad)", () -> { try { return Local.valueOf("NOPE").toString(); }
                                    catch (IllegalArgumentException e) { return "IllegalArgumentException"; } });
        row("compareTo", () -> String.valueOf(Local.ALPHA.compareTo(Local.GAMMA)));
        row("switch", () -> { switch (Local.BETA) { case ALPHA: return "a"; case BETA: return "b"; default: return "d"; } });
        row("getDeclaringClass", () -> Local.ALPHA.getDeclaringClass().getName());
        row("Class.isEnum", () -> String.valueOf(Local.class.isEnum()));
        row("Class.getEnumConstants", () -> String.valueOf(Local.class.getEnumConstants().length));
        row("EnumMap", () -> { EnumMap<Local, String> m = new EnumMap<>(Local.class);
                               m.put(Local.BETA, "x"); return m.size() + "/" + m.get(Local.BETA); });
        row("EnumSet.of", () -> EnumSet.of(Local.ALPHA, Local.GAMMA).toString());
        row("EnumSet.allOf", () -> String.valueOf(EnumSet.allOf(Local.class).size()));
        // JDK enums, which is where the four tests were pointing.
        row("TimeUnit constant", () -> TimeUnit.MILLISECONDS.name());
        row("TimeUnit.toNanos", () -> String.valueOf(TimeUnit.MILLISECONDS.toNanos(2)));
        row("TimeUnit.values", () -> String.valueOf(TimeUnit.values().length));
        row("HttpClient.Version", () -> java.net.http.HttpClient.Version.HTTP_1_1.name());
        row("HttpClient.Redirect", () -> java.net.http.HttpClient.Redirect.NEVER.name());
        row("DayOfWeek.MONDAY", () -> java.time.DayOfWeek.MONDAY.name());
        row("EnumSet over a JDK enum", () -> String.valueOf(EnumSet.allOf(TimeUnit.class).size()));
    }

    interface Body { String run() throws Exception; }

    static void row(String label, Body b) {
        String v;
        try {
            v = b.run();
        } catch (Throwable t) {
            v = "ERROR " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + " | " + v);
    }
}
