// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, lane L3: which class an `invokestatic` initializes,
// and whether a call site cached during a failing <clinit> keeps working.
//
// Run with the interpreter only (e.g. --nojit) as well as the default, and
// diff against HotSpot 25. HotSpot 25 prints exactly:
//
//   1 P.<clinit>
//   1 m=7
//   1 C not yet initialized
//   1 C.<clinit>
//   1 C.tag=c
//   2 Fail.foo ran
//   2 EIIE java.lang.RuntimeException: boom
//   2 NCDFE Could not initialize class InvokeStaticInit$Fail
//   2 NCDFE Could not initialize class InvokeStaticInit$Fail
//
// Case 1 (JVMS 5.5 / JLS 12.4.1): `C.m()` for a static `m` declared in `P`
// compiles to `invokestatic InvokeStaticInit$C.m` and must run P.<clinit>
// only. Before the fix CratonVM initialized C too, printing "C.<clinit>"
// before "m=7".
//
// Case 2: Fail.<clinit> reaches `Fail.foo()` through Caller.callFoo(). The
// initializing thread may make that call (JVMS 5.5 step 3), but once the
// initializer throws, every later invokestatic of Fail must raise
// NoClassDefFoundError. Before the fix the interpreter cached Caller's call
// site during the recursive request and the later calls printed
// "Fail.foo ran" instead of the NCDFE lines.
public class InvokeStaticInit {
    static class P {
        static {
            System.out.println("1 P.<clinit>");
        }

        static int m() {
            return 7;
        }
    }

    static class C extends P {
        static String tag;

        static {
            System.out.println("1 C.<clinit>");
            tag = "c";
        }
    }

    static class Fail {
        static {
            Caller.callFoo();
            if (Boolean.parseBoolean("true")) {
                throw new RuntimeException("boom");
            }
        }

        static void foo() {
            System.out.println("2 Fail.foo ran");
        }
    }

    static class Caller {
        static void callFoo() {
            Fail.foo();
        }
    }

    public static void main(String[] args) {
        System.out.println("1 m=" + C.m());
        System.out.println("1 C not yet initialized");
        System.out.println("1 C.tag=" + C.tag);

        try {
            Fail.foo();
        } catch (ExceptionInInitializerError e) {
            System.out.println("2 EIIE " + e.getCause());
        }
        for (int i = 0; i < 2; i++) {
            try {
                Caller.callFoo();
                System.out.println("2 no error (wrong)");
            } catch (NoClassDefFoundError e) {
                System.out.println("2 NCDFE " + e.getMessage());
            }
        }
    }
}
