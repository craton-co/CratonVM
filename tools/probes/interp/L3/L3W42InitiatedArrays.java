// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L3: the rest of HotSpot's
// `Instrumentation.getInitiatedClasses(loader)` list, and the primitive array
// classes in `getAllLoadedClasses`
// (docs/internal/fixed-bugs/interpreter-L3-getinitiatedclasses-ignores-its-loader-FIXED-20261008.md).
//
// HotSpot's per-loader list (measured, JDK 25.0.3) is the loader's dictionary
// (the classes it defined and the ones it initiated), the array classes of
// each of those that exist (`Gen[]`, `Object[]`, `Object[][]`), and, for
// every loader, the eight primitive array classes and every existing array
// of them (`int[][]`). A hidden class is in no dictionary. A user loader that
// defined `Gen` and delegated the rest lists `Gen`, `Gen[]`, `Object` (it
// initiated `Gen`'s superclass), `Object[]`, `Object[][]` and the primitive
// arrays.
// Rows (`prims` counts the eight `[Z [C [F [D [B [S [I [J`):
//   all        -- getAllLoadedClasses: the primitive arrays, `int[][]`, no
//                 `int.class`, the arrays of an application and a user class;
//   all-hidden -- getAllLoadedClasses holds a lambda's class;
//   boot       -- getInitiatedClasses(null): primitive arrays, `int[][]`,
//                 `Object[]`, `String[]`, not the arrays of other loaders'
//                 classes;
//   app        -- the application loader: its class, that class's arrays,
//                 `Object` (its classes' superclass), not `Gen[]`;
//   app-hidden -- the application loader's list holds the lambda's class;
//   custom     -- the user loader: `Gen`, `Gen[]`, `Object`, `Object[]`,
//                 not `Mine[]`, not `String`.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     all prims=8 int[][]=true int=false mine[]=true gen[]=true
//     all-hidden true
//     boot prims=8 int[][]=true object[]=true string[]=true mine[]=false gen[]=false
//     app prims=8 mine=true mine[]=true mine[][]=true object=true gen[]=false
//     app-hidden false
//     custom prims=8 gen=true gen[]=true object=true object[]=true mine[]=false string=false
// CratonVM before wave 42 (read from the code, not run): `getInitiatedClasses`
// listed the classes whose loader id is the loader's, arrays included by
// THEIR loader id -- an array of an application class is filed under the
// bootstrap loader (`ClassManager::array_defining_loader`), so `boot mine[]`
// was `true` and `app mine[]` `false`; no primitive array for the
// application or user loader; no `Object` for a user loader (the JDK-global
// names are not recorded). `all prims` counted only the primitive array
// classes some code had used. Wave 42 builds HotSpot's list
// (`NativeContextImpl::list_initiated_class_ids`) and makes the eight
// primitive array classes before either list. Host run of wave 42: every row
// matched but `all-hidden false` (a lambda's class is a VM-minted proxy
// outside the class store) and `boot ... string[]=false`. The follow-up adds
// the lambda proxy classes to `getAllLoadedClasses`
// (`NativeContext::list_lambda_proxy_class_ids`) and resolves the leaf of an
// array minted without `array_info` by its descriptor
// (`list_initiated_class_ids`); `CRATONVM_DBG_RETRANSFORM=1` prints
// `[RETRANSFORM] getInitiatedClasses(...): array ... names no leaf class`
// for any array still unresolved.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W42InitiatedArrays$Agent
// containing L3W42InitiatedArrays*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W42InitiatedArrays
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Array;
import java.util.function.Supplier;

public class L3W42InitiatedArrays {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** Defined by `Own` from its bytes. */
    public static class Gen {
        public static int value() {
            return 7;
        }
    }

    /** Defined by the application loader. */
    public static class Mine {
    }

    /** Defines `Gen` itself, delegating everything else to the bootstrap loader. */
    static final class Own extends ClassLoader {
        Own() {
            super(null);
        }

        Class<?> defineGen() throws Exception {
            String name = L3W42InitiatedArrays.class.getName() + "$Gen";
            try (InputStream in = L3W42InitiatedArrays.class
                    .getResourceAsStream("L3W42InitiatedArrays$Gen.class")) {
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

    static int prims(Class<?>[] classes) {
        int n = 0;
        Class<?>[] p = {boolean[].class, char[].class, float[].class, double[].class,
                byte[].class, short[].class, int[].class, long[].class};
        for (Class<?> c : p) {
            if (holds(classes, c)) {
                n++;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Class<?> mineArr = Array.newInstance(Mine.class, 0).getClass();
        Class<?> mineArr2 = Array.newInstance(Mine.class, 0, 0).getClass();
        Class<?> intArr2 = new int[0][0].getClass();
        Supplier<String> lambda = () -> "x";
        Class<?> hidden = lambda.getClass();
        Own own = new Own();
        Class<?> gen = own.defineGen();
        Class<?> genArr = Array.newInstance(gen, 0).getClass();

        Class<?>[] all = i.getAllLoadedClasses();
        System.out.println("all prims=" + prims(all) + " int[][]=" + holds(all, intArr2)
                + " int=" + holds(all, int.class) + " mine[]=" + holds(all, mineArr)
                + " gen[]=" + holds(all, genArr));
        System.out.println("all-hidden " + holds(all, hidden));

        Class<?>[] boot = i.getInitiatedClasses(null);
        System.out.println("boot prims=" + prims(boot) + " int[][]=" + holds(boot, intArr2)
                + " object[]=" + holds(boot, Object[].class)
                + " string[]=" + holds(boot, String[].class)
                + " mine[]=" + holds(boot, mineArr) + " gen[]=" + holds(boot, genArr));

        ClassLoader app = L3W42InitiatedArrays.class.getClassLoader();
        Class<?>[] appList = i.getInitiatedClasses(app);
        System.out.println("app prims=" + prims(appList) + " mine=" + holds(appList, Mine.class)
                + " mine[]=" + holds(appList, mineArr) + " mine[][]=" + holds(appList, mineArr2)
                + " object=" + holds(appList, Object.class) + " gen[]=" + holds(appList, genArr));
        System.out.println("app-hidden " + holds(appList, hidden));

        Class<?>[] ownList = i.getInitiatedClasses(own);
        System.out.println("custom prims=" + prims(ownList) + " gen=" + holds(ownList, gen)
                + " gen[]=" + holds(ownList, genArr) + " object=" + holds(ownList, Object.class)
                + " object[]=" + holds(ownList, Object[].class)
                + " mine[]=" + holds(ownList, mineArr)
                + " string=" + holds(ownList, String.class));
    }
}
