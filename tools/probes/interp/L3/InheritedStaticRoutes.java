// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 14, lane L5: an inherited static named through
// its subclass initializes the DECLARING class only (JVMS 5.5, JLS 12.4.1)
// on every route that reaches it, not just the interpreter's `invokestatic`:
//
//   direct  - `C1.m()`: `invokestatic InheritedStaticRoutes$C1.m`
//   mh      - `MethodHandles.lookup().findStatic(C2.class, "m", ..)`
//   reflect - `C3.class.getMethod("m").invoke(null)`
//   lambda  - `IntSupplier s = C4::m; s.getAsInt()` (a REF_invokeStatic
//             implementation handle; the native-callback lambda route runs
//             it through the by-name static entry `invoke_static_or_native`)
//
// Before wave 14 the by-name static tails initialized the symbolic owner
// (`Cn.<clinit>` printed after `Pn.<clinit>`). Run with the default settings
// and with --nojit; diff against HotSpot 25, which prints exactly:
//
//   direct
//   P1.<clinit>
//   7
//   mh
//   P2.<clinit>
//   7
//   reflect
//   P3.<clinit>
//   7
//   lambda
//   P4.<clinit>
//   7
//   done
//
// Row `mh` printed an extra `C2.<clinit>` until wave 16, because
// `Lookup.findStatic` initialized the class it is asked through;
// docs/internal/fixed-bugs/interpreter-L0-lookup-find-static-initializes-the-named-class-FIXED-20260925.md
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.IntSupplier;

public class InheritedStaticRoutes {
    public static class P1 {
        static {
            System.out.println("P1.<clinit>");
        }

        public static int m() {
            return 7;
        }
    }

    public static class C1 extends P1 {
        static {
            System.out.println("C1.<clinit>");
        }
    }

    public static class P2 {
        static {
            System.out.println("P2.<clinit>");
        }

        public static int m() {
            return 7;
        }
    }

    public static class C2 extends P2 {
        static {
            System.out.println("C2.<clinit>");
        }
    }

    public static class P3 {
        static {
            System.out.println("P3.<clinit>");
        }

        public static int m() {
            return 7;
        }
    }

    public static class C3 extends P3 {
        static {
            System.out.println("C3.<clinit>");
        }
    }

    public static class P4 {
        static {
            System.out.println("P4.<clinit>");
        }

        public static int m() {
            return 7;
        }
    }

    public static class C4 extends P4 {
        static {
            System.out.println("C4.<clinit>");
        }
    }

    public static void main(String[] args) throws Throwable {
        System.out.println("direct");
        System.out.println(C1.m());

        System.out.println("mh");
        MethodHandle mh = MethodHandles.lookup()
                .findStatic(C2.class, "m", MethodType.methodType(int.class));
        System.out.println((int) mh.invokeExact());

        System.out.println("reflect");
        Object r = C3.class.getMethod("m").invoke(null);
        System.out.println(r);

        System.out.println("lambda");
        IntSupplier s = C4::m;
        System.out.println(s.getAsInt());

        System.out.println("done");
    }
}
