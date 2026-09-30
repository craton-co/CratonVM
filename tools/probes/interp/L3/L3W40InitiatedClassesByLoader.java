// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L3: `Instrumentation.getInitiatedClasses`
// answers for the loader it is given
// (docs/internal/fixed-bugs/interpreter-L3-getinitiatedclasses-ignores-its-loader-FIXED-20261008.md).
// Rows (each `true`/`false`):
//   boot    -- getInitiatedClasses(null) holds java.lang.String and not this
//              probe's class;
//   app     -- getInitiatedClasses(the application loader) holds this probe's
//              class;
//   custom  -- getInitiatedClasses(a loader that defined `Gen` itself) holds
//              `Gen`, and the application loader's answer does not.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     boot string=true probe=false
//     app probe=true
//     custom gen=true app-has-gen=false
// CratonVM before wave 41 (read from the code, not run): `getInitiatedClasses0`'s
// native (`instrument::native_get_initiated_classes0`) ignored its argument and
// always listed the classes whose loader id is the application namespace, so
// `boot string=false probe=true` and `custom gen=false ...` were expected.
// Wave 41 (lane L3) maps the argument to the loader's namespace (null is the
// bootstrap loader): HotSpot's three lines expected in every mode. HotSpot
// also lists, for every loader, the eight primitive array classes, the array
// classes of its classes and the JDK classes it initiated (`Object` for a
// user loader); CratonVM did not until wave 42, so no row here asks
// (`L3W42InitiatedArrays` does).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W40InitiatedClassesByLoader$Agent
// containing L3W40InitiatedClassesByLoader*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W40InitiatedClassesByLoader
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.Instrumentation;

public class L3W40InitiatedClassesByLoader {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** Defined by `Own` from its bytes, not by the application loader. */
    public static class Gen {
        public static int value() {
            return 7;
        }
    }

    /** Defines `Gen` itself. */
    static final class Own extends ClassLoader {
        Own() {
            super(null);
        }

        Class<?> defineGen() throws Exception {
            String name = L3W40InitiatedClassesByLoader.class.getName() + "$Gen";
            try (InputStream in = L3W40InitiatedClassesByLoader.class
                    .getResourceAsStream("L3W40InitiatedClassesByLoader$Gen.class")) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static boolean holds(Class<?>[] classes, Class<?> c) {
        for (Class<?> k : classes) {
            if (k == c) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Class<?>[] boot = i.getInitiatedClasses(null);
        System.out.println("boot string=" + holds(boot, String.class)
                + " probe=" + holds(boot, L3W40InitiatedClassesByLoader.class));
        ClassLoader app = L3W40InitiatedClassesByLoader.class.getClassLoader();
        System.out.println("app probe=" + holds(i.getInitiatedClasses(app), L3W40InitiatedClassesByLoader.class));
        Own own = new Own();
        Class<?> gen = own.defineGen();
        System.out.println("custom gen=" + holds(i.getInitiatedClasses(own), gen)
                + " app-has-gen=" + holds(i.getInitiatedClasses(app), gen));
    }
}
