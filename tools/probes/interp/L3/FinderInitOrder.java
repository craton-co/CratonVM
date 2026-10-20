// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 17, lane L4: `Lookup.findVirtual`,
// `findConstructor`, `unreflect` and `findGetter` initialize NOTHING at
// lookup time (HotSpot's `DirectMethodHandle` initializes a static member's
// or a constructor's declaring class at its first call; an instance member
// is initialized by its receiver's `new`). Before wave 17 CratonVM ran the
// class's `<clinit>` inside the finder, so each marker printed one line
// early (right after the finder's own label).
//
// Run with the default settings and with --nojit; HotSpot 25 prints exactly:
//
//   findVirtual
//   new V
//   V.<clinit>
//   v=1
//   findConstructor
//   invoke ctor
//   K.<clinit>
//   k=true
//   unreflect
//   invoke static
//   U.<clinit>
//   u=2
//   findGetter
//   new G
//   G.<clinit>
//   g=3

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

public class FinderInitOrder {
    static class V {
        static {
            System.out.println("V.<clinit>");
        }

        int m() {
            return 1;
        }
    }

    static class K {
        static {
            System.out.println("K.<clinit>");
        }

        K() {}
    }

    static class U {
        static {
            System.out.println("U.<clinit>");
        }

        static int s() {
            return 2;
        }
    }

    static class G {
        static {
            System.out.println("G.<clinit>");
        }

        int x = 3;
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();

        System.out.println("findVirtual");
        MethodHandle v = l.findVirtual(V.class, "m", MethodType.methodType(int.class));
        System.out.println("new V");
        V vi = new V();
        System.out.println("v=" + (int) v.invoke(vi));

        System.out.println("findConstructor");
        MethodHandle ctor = l.findConstructor(K.class, MethodType.methodType(void.class));
        System.out.println("invoke ctor");
        Object k = ctor.invoke();
        System.out.println("k=" + (k instanceof K));

        System.out.println("unreflect");
        Method sm = U.class.getDeclaredMethod("s");
        MethodHandle u = l.unreflect(sm);
        System.out.println("invoke static");
        System.out.println("u=" + (int) u.invoke());

        System.out.println("findGetter");
        MethodHandle g = l.findGetter(G.class, "x", int.class);
        System.out.println("new G");
        G gi = new G();
        System.out.println("g=" + (int) g.invoke(gi));
    }
}
