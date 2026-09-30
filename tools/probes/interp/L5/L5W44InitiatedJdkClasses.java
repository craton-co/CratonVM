// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L5: `Instrumentation.getInitiatedClasses`
// lists the JDK classes a loader initiated by RESOLVING their names
// (docs/internal/fixed-bugs/interpreter-L3-getinitiatedclasses-ignores-its-loader-FIXED-20261008.md,
// its last item).
//
// Rows (each `true`/`false`):
//   app     -- the application loader's list holds java.lang.String (named
//              and resolved by this class), java.util.concurrent.
//              ConcurrentSkipListSet (resolved here by `ldc`), java.sql.Date
//              (a platform-loader class, resolved here by `ldc`), and not
//              the user loader's `Gen`;
//   custom  -- the list of a loader that defined `Gen` itself (parent: the
//              bootstrap loader) holds `Gen`, java.util.ArrayDeque (resolved
//              by `new` in `Gen.use`, which ran), java.lang.Object (its
//              superclass), and not this probe's class.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     app string=true skiplist=true sqldate=true gen=false
//     custom gen=true deque=true object=true probe=false
// CratonVM on the base `5248262b7` (from the code): a JDK class reaches a
// loader's list only as a direct supertype of a class it defined, so
// `app string=false skiplist=false sqldate=false gen=false` and `custom
// gen=true deque=false object=true probe=false`.
//
// Not a row: a JDK class named by a constant a loader's class never
// resolved. HotSpot lists it only when its verifier loaded it through the
// loader; CratonVM (wave 44) lists every loaded JDK class its classes'
// constant pools name, resolved or not
// (docs/known-issues/interpreter/i44-L5-getinitiatedclasses-lists-a-jdk-class-an-unresolved-constant-names-20261008.md).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L5W44InitiatedJdkClasses$Agent
// containing L5W44InitiatedJdkClasses*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L5W44InitiatedJdkClasses
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.Instrumentation;

public class L5W44InitiatedJdkClasses {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** Defined by `Own` from its bytes. */
    public static class Gen {
        public static int use() {
            return new java.util.ArrayDeque<Object>().size();
        }
    }

    /** Defines `Gen` itself, delegating everything else to the bootstrap loader. */
    static final class Own extends ClassLoader {
        Own() {
            super(null);
        }

        Class<?> defineGen() throws Exception {
            String name = L5W44InitiatedJdkClasses.class.getName() + "$Gen";
            try (InputStream in = L5W44InitiatedJdkClasses.class
                    .getResourceAsStream("L5W44InitiatedJdkClasses$Gen.class")) {
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
        Class<?> skiplist = java.util.concurrent.ConcurrentSkipListSet.class;
        Class<?> sqldate = java.sql.Date.class;
        Own own = new Own();
        Class<?> gen = own.defineGen();
        gen.getMethod("use").invoke(null);

        Class<?>[] app = i.getInitiatedClasses(L5W44InitiatedJdkClasses.class.getClassLoader());
        System.out.println("app string=" + holds(app, String.class) + " skiplist=" + holds(app, skiplist)
                + " sqldate=" + holds(app, sqldate) + " gen=" + holds(app, gen));

        Class<?>[] mine = i.getInitiatedClasses(own);
        System.out.println("custom gen=" + holds(mine, gen)
                + " deque=" + holds(mine, java.util.ArrayDeque.class)
                + " object=" + holds(mine, Object.class)
                + " probe=" + holds(mine, L5W44InitiatedJdkClasses.class));
    }
}
