import java.util.*;

/** Repro for SC-env-classreading RC-A: System.getenv()/getProperties() singleton identity,
 *  plus the side-table-liveness invariant the fix must preserve. */
public class EnvSingletonRepro {
    static int pass = 0, fail = 0;
    static void check(String name, boolean ok) {
        if (ok) pass++; else fail++;
        System.out.println((ok ? "PASS " : "FAIL ") + name);
    }

    public static void main(String[] a) {
        // --- Identity (the bug) ---
        check("getenv() == getenv()", System.getenv() == System.getenv());
        check("getProperties() == getProperties()", System.getProperties() == System.getProperties());

        // getenv() equals itself by content too
        Map<String, String> e1 = System.getenv();
        Map<String, String> e2 = System.getenv();
        check("getenv().equals(getenv())", e1.equals(e2));
        check("getenv() non-empty", !e1.isEmpty());

        // --- Liveness: a property set AFTER the first getProperties() must be
        //     visible both via getProperty and via enumeration on the singleton. ---
        Properties p1 = System.getProperties();
        System.setProperty("craton.env.test.key", "craton-value-123");
        Properties p2 = System.getProperties();
        check("getProperties singleton stable across setProperty", p1 == p2);
        check("new prop visible via getProperty", "craton-value-123".equals(p2.getProperty("craton.env.test.key")));
        check("new prop visible via stringPropertyNames",
                p2.stringPropertyNames().contains("craton.env.test.key"));
        check("new prop visible via System.getProperty",
                "craton-value-123".equals(System.getProperty("craton.env.test.key")));

        // A pre-existing standard property is still readable through the singleton.
        check("standard prop readable (java.version)",
                System.getProperties().getProperty("java.version") != null);

        // Mutation through the singleton propagates to System.getProperty.
        System.getProperties().setProperty("craton.env.test.key2", "v2");
        check("setProperty via getProperties propagates",
                "v2".equals(System.getProperty("craton.env.test.key2")));

        System.out.println("RESULT pass=" + pass + " fail=" + fail);
        if (fail != 0) System.exit(1);
    }
}
