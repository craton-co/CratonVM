// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L2 (hunt, SUSPECTED; page
// i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range):
// an exception raised by a compiled body's RUNTIME HELPER (class
// initialization from getstatic / invokestatic / new) at a bci that no try
// covers, in a method that also has a try catching that same type over a
// DIFFERENT range. HotSpot propagates it (JVMS 2.10: a handler applies only
// to its [start_pc, end_pc) range). The eager first-call door
// (CRATONVM_BG_COMPILE=0) compiles each method before its first run, so the
// helper throws from compiled code; when the body stamps no throw bci for a
// helper exception, `execute`'s drain takes the pc-unknown search
// (exception_dispatch.rs `search_compiled_frame_rows`,
// `CompiledThrowSite::Unknown`), which matches a TYPED row on the exception
// class alone, ignoring its range -- and the exception would be "caught" by
// the other try's handler.
//
// Run, each mode, JIT default, eager door, and --nojit (all must print the
// expected lines):
//   cratonvm L2W101EagerUncoveredInitError
//   CRATONVM_BG_COMPILE=0 cratonvm L2W101EagerUncoveredInitError
//   cratonvm --nojit L2W101EagerUncoveredInitError
//
// HOTSPOT_EXPECTED_BEGIN
// getstatic: escaped java.lang.ExceptionInInitializerError
// invokestatic: escaped java.lang.ExceptionInInitializerError
// new: escaped java.lang.NoClassDefFoundError
// covered: returned -7
// HOTSPOT_EXPECTED_END

public class L2W101EagerUncoveredInitError {
    static boolean fail = true;

    static class BadA {
        static int VALUE;

        static {
            if (fail) {
                throw new IllegalStateException("A");
            }
            VALUE = 1;
        }
    }

    static class BadB {
        static {
            if (fail) {
                throw new IllegalStateException("B");
            }
        }

        static int f() {
            return 2;
        }
    }

    static class BadC {
        static {
            if (fail) {
                throw new IllegalStateException("C");
            }
        }
    }

    static class BadD {
        static int VALUE;

        static {
            if (fail) {
                throw new IllegalStateException("D");
            }
        }
    }

    static int other(int x) {
        return x + 1;
    }

    // The try covers only the first call; the getstatic is outside it.
    static int getstaticOutside(int x) {
        int r = 0;
        try {
            r = other(x);
        } catch (ExceptionInInitializerError e) {
            return -1;
        }
        return r + BadA.VALUE;
    }

    static int invokestaticOutside(int x) {
        int r = 0;
        try {
            r = other(x);
        } catch (ExceptionInInitializerError e) {
            return -1;
        }
        return r + BadB.f();
    }

    static Object newOutside(int x) {
        Object o = null;
        try {
            o = Integer.valueOf(other(x));
        } catch (NoClassDefFoundError e) {
            return "caught";
        }
        return new BadC();
    }

    // Control: the throwing getstatic IS covered.
    static int covered(int x) {
        try {
            return other(x) + BadD.VALUE;
        } catch (ExceptionInInitializerError e) {
            return -7;
        }
    }

    public static void main(String[] args) {
        try {
            System.out.println("getstatic: returned " + getstaticOutside(1));
        } catch (Throwable t) {
            System.out.println("getstatic: escaped " + t.getClass().getName());
        }
        try {
            System.out.println("invokestatic: returned " + invokestaticOutside(1));
        } catch (Throwable t) {
            System.out.println("invokestatic: escaped " + t.getClass().getName());
        }
        // BadC's first initialization fails here, so the `new` below sees an
        // erroneous class: NoClassDefFoundError.
        try {
            new BadC();
        } catch (Throwable t) {
            // ExceptionInInitializerError, expected.
        }
        try {
            System.out.println("new: returned " + newOutside(1));
        } catch (Throwable t) {
            System.out.println("new: escaped " + t.getClass().getName());
        }
        try {
            System.out.println("covered: returned " + covered(1));
        } catch (Throwable t) {
            System.out.println("covered: escaped " + t.getClass().getName());
        }
    }
}
