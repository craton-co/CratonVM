// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L5 -- the `ClassLoader` null-resource-name
// contract on hot call sites, for the retirement of the by-name intercepts
// (`CRATONVM_RETIRE_CLASSLOADER_NULL_NAME_INTERCEPTS`;
// `docs/internal/fixed-bugs/interpreter-L5-proposal-retire-the-classloader-null-name-intercepts-FIXED-20261022.md`).
// Eight doors (`invoke_or_native`, the two `native_override` interceptors, the
// vtable door and its cache eviction, the JIT's three) sent a null name on an
// inherited `ClassLoader` method to `ClassLoader`'s native. With them retired,
// ordinary dispatch must still raise the NPE on every call, in every tier, for
// a loader that inherits the method, and must still run a non-loader type's
// own `getResource(String)` (round 11's `R11RtGetResourceNull` threw an NPE
// there 60 000 times).
//
// Rows (the cold rows first, and again last; counts in `long`):
//   cold-*    one call each: the NPE's class and message (no trace text)
//   hot-*     N calls on one site each (compiled after warm-up): how many
//             threw `NullPointerException`, how many answered
//   user-type a `getResource(String)` of a non-loader type, N calls: answers
//
// Run (no setup):
//   javac -d out L5W55NullResourceNameHot.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W55NullResourceNameHot
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   cold-plain-getResource-1=java.lang.NullPointerException msg=null
//   cold-app-getResource-1=java.lang.NullPointerException msg=null
//   cold-plain-getResources-1=java.lang.NullPointerException msg=null
//   cold-app-getResources-1=java.lang.NullPointerException msg=null
//   cold-plain-getResourceAsStream-1=java.lang.NullPointerException msg=null
//   cold-app-getResourceAsStream-1=java.lang.NullPointerException msg=null
//   cold-plain-loadClass-1=java.lang.NullPointerException msg=null
//   cold-app-loadClass-1=java.lang.NullPointerException msg=null
//   cold-plain-resources-1=java.lang.NullPointerException msg=null
//   cold-app-resources-1=java.lang.NullPointerException msg=null
//   hot-plain-getResource=npe=60000 answered=0
//   hot-plain-getResources=npe=60000 answered=0
//   hot-plain-getResourceAsStream=npe=60000 answered=0
//   hot-app-getResource=npe=60000 answered=0
//   user-type=answered=60000
//   cold-plain-getResource-2=java.lang.NullPointerException msg=null
//   cold-app-getResource-2=java.lang.NullPointerException msg=null
//   cold-plain-getResources-2=java.lang.NullPointerException msg=null
//   cold-app-getResources-2=java.lang.NullPointerException msg=null
//   cold-plain-getResourceAsStream-2=java.lang.NullPointerException msg=null
//   cold-app-getResourceAsStream-2=java.lang.NullPointerException msg=null
//   cold-plain-loadClass-2=java.lang.NullPointerException msg=null
//   cold-app-loadClass-2=java.lang.NullPointerException msg=null
//   cold-plain-resources-2=java.lang.NullPointerException msg=null
//   cold-app-resources-2=java.lang.NullPointerException msg=null
// CratonVM on the base (debug build of 63d2087232, Windows, both modes):
// `cold-plain-loadClass-N=null` (the `loadClass` natives answered null for a
// null name; the intercepts hid it for the built-in loaders only); fixed in
// wave 55 with the retirement.

import java.io.InputStream;
import java.net.URL;
import java.util.Enumeration;

public class L5W55NullResourceNameHot {
    static final long N = 60_000L;

    /** Inherits every `getResource*` from `ClassLoader`. */
    static final class Plain extends ClassLoader {
        Plain() {
            super(L5W55NullResourceNameHot.class.getClassLoader());
        }
    }

    /** Not a loader: its own `getResource(String)` accepts null. */
    static final class Box {
        URL getResource(String name) {
            return null;
        }
    }

    static String cold(String what, ClassLoader l) {
        try {
            switch (what) {
                case "getResource": l.getResource(null); break;
                case "getResources": l.getResources(null); break;
                case "getResourceAsStream": l.getResourceAsStream(null); break;
                case "loadClass": l.loadClass(null); break;
                case "resources": l.resources(null); break;
                default: throw new IllegalStateException(what);
            }
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static long[] hotGetResource(ClassLoader l) {
        long npe = 0, answered = 0;
        for (long i = 0; i < N; i++) {
            try {
                URL u = l.getResource(null);
                answered++;
            } catch (NullPointerException e) {
                npe++;
            }
        }
        return new long[] {npe, answered};
    }

    static long[] hotGetResources(ClassLoader l) throws Exception {
        long npe = 0, answered = 0;
        for (long i = 0; i < N; i++) {
            try {
                Enumeration<URL> e = l.getResources(null);
                answered++;
            } catch (NullPointerException e) {
                npe++;
            }
        }
        return new long[] {npe, answered};
    }

    static long[] hotGetResourceAsStream(ClassLoader l) {
        long npe = 0, answered = 0;
        for (long i = 0; i < N; i++) {
            try {
                InputStream s = l.getResourceAsStream(null);
                answered++;
            } catch (NullPointerException e) {
                npe++;
            }
        }
        return new long[] {npe, answered};
    }

    static long userType(Box b) {
        long answered = 0;
        for (long i = 0; i < N; i++) {
            if (b.getResource(null) == null) {
                answered++;
            }
        }
        return answered;
    }

    static void coldRows(String tag, ClassLoader plain, ClassLoader app) {
        for (String m : new String[] {"getResource", "getResources", "getResourceAsStream", "loadClass", "resources"}) {
            System.out.println("cold-plain-" + m + "-" + tag + "=" + cold(m, plain));
            System.out.println("cold-app-" + m + "-" + tag + "=" + cold(m, app));
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader plain = new Plain();
        ClassLoader app = L5W55NullResourceNameHot.class.getClassLoader();
        coldRows("1", plain, app);
        long[] r = hotGetResource(plain);
        System.out.println("hot-plain-getResource=npe=" + r[0] + " answered=" + r[1]);
        r = hotGetResources(plain);
        System.out.println("hot-plain-getResources=npe=" + r[0] + " answered=" + r[1]);
        r = hotGetResourceAsStream(plain);
        System.out.println("hot-plain-getResourceAsStream=npe=" + r[0] + " answered=" + r[1]);
        r = hotGetResource(app);
        System.out.println("hot-app-getResource=npe=" + r[0] + " answered=" + r[1]);
        System.out.println("user-type=answered=" + userType(new Box()));
        coldRows("2", new Plain(), app);
    }
}
