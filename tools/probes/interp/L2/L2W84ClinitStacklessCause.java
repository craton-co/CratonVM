// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L2 -- the cause a "Could not initialize
// class" NoClassDefFoundError carries when the <clinit> threw a throwable that
// captured NO stack trace.
//
// HotSpot (`java_lang_Throwable::create_initialization_error`) records an
// ExceptionInInitializerError whose message names the original exception and
// thread, and whose stack trace is the original's `getStackTrace()`. For a
// stackless throwable -- `writableStackTrace == false` (B), or the common
// "fillInStackTrace() returns this" idiom (A) -- that is an EMPTY array. The
// VM recorded "no trace" and so left the recorded error with the trace of the
// NoClassDefFoundError's construction site (`exceptions::
// record_class_init_error`). C is the ordinary case, unchanged.
//
// Rows per class X: first = the ExceptionInInitializerError of the first use
// (its own trace's top method), second = the NoClassDefFoundError of the
// second use, second-cause* = its recorded cause.
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W84ClinitStacklessCause.java
//   $B --java-home $J/.. -cp out L2W84ClinitStacklessCause
//   $B --compatible --java-home $J/.. -cp out L2W84ClinitStacklessCause
//   (each also with --nojit)
//   $J/java -cp out L2W84ClinitStacklessCause
//
// Derived from the JDK 25 sources (InstanceKlass::initialize_impl steps 7-11,
// create_initialization_error, Throwable.getOurStackTrace); refresh from
// $J/java before reading a DIFF.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// A first java.lang.ExceptionInInitializerError cause=L2W84ClinitStacklessCause$Quiet: quiet top=touch
// A second java.lang.NoClassDefFoundError: Could not initialize class L2W84ClinitStacklessCause$A
// A second-cause java.lang.ExceptionInInitializerError: Exception L2W84ClinitStacklessCause$Quiet: quiet [in thread "main"]
// A second-cause-trace 0
// A second-cause-cause null
// B first java.lang.ExceptionInInitializerError cause=L2W84ClinitStacklessCause$Unwritable: unwritable top=touch
// B second java.lang.NoClassDefFoundError: Could not initialize class L2W84ClinitStacklessCause$B
// B second-cause java.lang.ExceptionInInitializerError: Exception L2W84ClinitStacklessCause$Unwritable: unwritable [in thread "main"]
// B second-cause-trace 0
// B second-cause-cause null
// C first java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: loud top=touch
// C second java.lang.NoClassDefFoundError: Could not initialize class L2W84ClinitStacklessCause$C
// C second-cause java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: loud [in thread "main"]
// C second-cause-trace 3 L2W84ClinitStacklessCause$C.<clinit> L2W84ClinitStacklessCause.touch L2W84ClinitStacklessCause.main
// C second-cause-cause null
// HOTSPOT_EXPECTED_END
public class L2W84ClinitStacklessCause {
    static final class Quiet extends RuntimeException {
        Quiet(String m) {
            super(m);
        }

        @Override
        public synchronized Throwable fillInStackTrace() {
            return this;
        }
    }

    static final class Unwritable extends RuntimeException {
        Unwritable(String m) {
            super(m, null, false, false);
        }
    }

    static final class A {
        static {
            if (true) {
                throw new Quiet("quiet");
            }
        }

        static void use() {
        }
    }

    static final class B {
        static {
            if (true) {
                throw new Unwritable("unwritable");
            }
        }

        static void use() {
        }
    }

    static final class C {
        static {
            if (true) {
                throw new IllegalStateException("loud");
            }
        }

        static void use() {
        }
    }

    static void touch(char which) {
        switch (which) {
            case 'A' -> A.use();
            case 'B' -> B.use();
            default -> C.use();
        }
    }

    static String frames(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder().append(st.length);
        for (StackTraceElement e : st) {
            sb.append(' ').append(e.getClassName()).append('.').append(e.getMethodName());
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        for (char which : new char[] {'A', 'B', 'C'}) {
            try {
                touch(which);
                System.out.println(which + " first initialized");
            } catch (Throwable t) {
                StackTraceElement[] st = t.getStackTrace();
                System.out.println(which + " first " + t.getClass().getName() + " cause=" + t.getCause()
                        + " top=" + (st.length == 0 ? "none" : st[0].getMethodName()));
            }
            try {
                touch(which);
                System.out.println(which + " second initialized");
            } catch (Throwable t) {
                System.out.println(which + " second " + t);
                Throwable c = t.getCause();
                System.out.println(which + " second-cause " + c);
                if (c != null) {
                    System.out.println(which + " second-cause-trace " + frames(c.getStackTrace()));
                    System.out.println(which + " second-cause-cause " + c.getCause());
                }
            }
        }
    }
}
