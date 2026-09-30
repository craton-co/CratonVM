// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 16, lane L3: a direct method handle for a static
// member that a class INHERITS, looked up through that class, initializes
// nothing at lookup time and only the member's DECLARING class when it is
// first used (JVMS 5.5; `DirectMethodHandle.ensureInitialized` names
// `member.getDeclaringClass()`), and a static field handle reads and writes
// the declaring class's field.
//
//   findStatic       - `findStatic(C1.class, "m", ..)`, `m` declared in P1
//   findStaticGetter - `findStaticGetter(C2.class, "f", int.class)`, `f` in P2
//   findStaticSetter - `findStaticSetter(C3.class, "f", int.class)`, `f` in P3
//   varhandle        - `findStaticVarHandle(C4.class, "f", int.class).get()`
//                      (no marker between lookup and access: only the
//                      declaring class may be initialized, whenever it is)
//
// Before wave 16 CratonVM ran `C1.<clinit>` at lookup time, and the field
// rows ran `Cn.<clinit>` and read or wrote slot 0 of `Cn` (the getter printed
// 0, the setter lost its write, the VarHandle answered the default).
// Run with the default settings and with --nojit; HotSpot 25 prints exactly:
//
//   findStatic
//   looked up
//   P1.<clinit>
//   7
//   findStaticGetter
//   looked up
//   P2.<clinit>
//   5
//   findStaticSetter
//   looked up
//   P3.<clinit>
//   9
//   varhandle
//   P4.<clinit>
//   5
//   done
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class InheritedStaticHandles {
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
        public static int f = 5;

        static {
            System.out.println("P2.<clinit>");
        }
    }

    public static class C2 extends P2 {
        static {
            System.out.println("C2.<clinit>");
        }
    }

    public static class P3 {
        public static int f = 5;

        static {
            System.out.println("P3.<clinit>");
        }
    }

    public static class C3 extends P3 {
        static {
            System.out.println("C3.<clinit>");
        }
    }

    public static class P4 {
        public static int f = 5;

        static {
            System.out.println("P4.<clinit>");
        }
    }

    public static class C4 extends P4 {
        static {
            System.out.println("C4.<clinit>");
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();

        System.out.println("findStatic");
        MethodHandle m = lookup.findStatic(C1.class, "m", MethodType.methodType(int.class));
        System.out.println("looked up");
        System.out.println((int) m.invokeExact());

        System.out.println("findStaticGetter");
        MethodHandle g = lookup.findStaticGetter(C2.class, "f", int.class);
        System.out.println("looked up");
        System.out.println((int) g.invokeExact());

        System.out.println("findStaticSetter");
        MethodHandle s = lookup.findStaticSetter(C3.class, "f", int.class);
        System.out.println("looked up");
        s.invokeExact(9);
        System.out.println(P3.f);

        System.out.println("varhandle");
        VarHandle vh = lookup.findStaticVarHandle(C4.class, "f", int.class);
        System.out.println((int) vh.get());

        System.out.println("done");
    }
}
