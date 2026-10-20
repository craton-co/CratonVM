// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe
//
// Interpreter round i6 wave 116, lane L7 (exception-text review): the class-initialization
// failure shapes the earlier probes (L3W71ClassInitDoors, L3W75ReflectSettledDoors,
// HandleInitErrors) do not cover. JVMS 5.5 steps 7 and 11-12, and HotSpot 21+'s recorded
// initialization error (JDK-8048190: the later NoClassDefFoundError's cause is an
// ExceptionInInitializerError naming the original throwable and thread):
//
//   error-first     <clinit> throws an Error (AssertionError): rethrown as is, not wrapped
//   error-later     the later use: NCDFE, cause "Exception java.lang.AssertionError: ... [in thread ...]"
//   nomsg-later     an exception with no message: the cause text has no ": "
//   thread-later    the failure happened in another thread: the cause names that thread
//   super-first     `new Sub()` whose superclass's <clinit> fails: the superclass's EIIE
//   super-sub       `new Sub()` again: NCDFE for Sub, cause naming the EIIE
//   super-super     a static of the superclass: NCDFE for the superclass, its own cause
//   nested-first    A's <clinit> reads B, whose <clinit> fails: B's EIIE propagates out of A (an Error)
//   nested-a        A again: NCDFE for A, cause naming the EIIE
//   nested-b        B again: NCDFE for B, cause naming B's own failure
//   iface-default   an interface with a default method whose <clinit> fails, initialized by
//                   `new Impl()`: EIIE, then NCDFE for the interface on a static call
//   no-init         instanceof / checkcast / array creation / .class of a failed class: no error
//   forname         Class.forName(name, true, loader) of a failed class: NCDFE; with false: ok
//   trace           the recorded cause's stack trace starts in the failing <clinit>
//
// Run in both modes, with the JIT and --nojit:
//   cratonvm --java-home <jdk25> --jdk-only -cp <dir> L7W116InitErrorShapes
//   cratonvm --java-home <jdk25> --compatible --nojit -cp <dir> L7W116InitErrorShapes
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// error-first java.lang.AssertionError: asserted in clinit
// error-later java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$ErrorInit cause=java.lang.ExceptionInInitializerError: Exception java.lang.AssertionError: asserted in clinit [in thread "main"]
// nomsg-first java.lang.ExceptionInInitializerError cause=java.lang.UnsupportedOperationException
// nomsg-later java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$NoMessageInit cause=java.lang.ExceptionInInitializerError: Exception java.lang.UnsupportedOperationException [in thread "main"]
// thread-later java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$ThreadInit cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: from the worker [in thread "init-worker"]
// super-first java.lang.ExceptionInInitializerError cause=java.lang.IllegalArgumentException: super failed
// super-sub java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$Sub cause=java.lang.ExceptionInInitializerError: Exception java.lang.ExceptionInInitializerError [in thread "main"]
// super-sub-static java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$Sub cause=java.lang.ExceptionInInitializerError: Exception java.lang.ExceptionInInitializerError [in thread "main"]
// super-super java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$FailingSuper cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalArgumentException: super failed [in thread "main"]
// nested-first java.lang.ExceptionInInitializerError cause=java.lang.ArithmeticException: b failed
// nested-a java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$A cause=java.lang.ExceptionInInitializerError: Exception java.lang.ExceptionInInitializerError [in thread "main"]
// nested-b java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$B cause=java.lang.ExceptionInInitializerError: Exception java.lang.ArithmeticException: b failed [in thread "main"]
// iface-default java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: iface failed
// iface-static java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$WithDefault cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: iface failed [in thread "main"]
// iface-impl-again java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$Impl cause=java.lang.ExceptionInInitializerError: Exception java.lang.ExceptionInInitializerError [in thread "main"]
// plain-first java.lang.ExceptionInInitializerError cause=java.lang.RuntimeException: plain failed
// instanceof=false array=2 class=Plain cast=null
// no-init ok
// Plain
// forname-noinit ok
// forname-init java.lang.NoClassDefFoundError: Could not initialize class L7W116InitErrorShapes$Plain cause=java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: plain failed [in thread "main"]
// trace L7W116InitErrorShapes$Plain.<clinit> line>0=true
// HOTSPOT_EXPECTED_END

public class L7W116InitErrorShapes {
    static final class ErrorInit {
        static int v;

        static {
            if (Boolean.parseBoolean("true")) {
                throw new AssertionError("asserted in clinit");
            }
        }
    }

    static final class NoMessageInit {
        static int v;

        static {
            if (Boolean.parseBoolean("true")) {
                throw new UnsupportedOperationException();
            }
        }
    }

    static final class ThreadInit {
        static int v;

        static {
            if (Boolean.parseBoolean("true")) {
                throw new IllegalStateException("from the worker");
            }
        }
    }

    static class FailingSuper {
        static int s = 1;

        static {
            if (Boolean.parseBoolean("true")) {
                throw new IllegalArgumentException("super failed");
            }
        }
    }

    static final class Sub extends FailingSuper {
        static int t = 2;
    }

    static final class B {
        static int b;

        static {
            if (Boolean.parseBoolean("true")) {
                throw new ArithmeticException("b failed");
            }
        }
    }

    static final class A {
        static int a = B.b + 1;
    }

    interface WithDefault {
        int K = compute();

        static int compute() {
            if (Boolean.parseBoolean("true")) {
                throw new IllegalStateException("iface failed");
            }
            return 1;
        }

        default int d() {
            return 7;
        }

        static int st() {
            return 3;
        }
    }

    static final class Impl implements WithDefault {
    }

    static final class Plain {
        static int p;

        static {
            if (Boolean.parseBoolean("true")) {
                throw new RuntimeException("plain failed");
            }
        }
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder(String.valueOf(t));
        Throwable c = t.getCause();
        if (c != null) {
            sb.append(" cause=").append(c);
        }
        return sb.toString();
    }

    interface Action {
        void run() throws Throwable;
    }

    static void row(String label, Action a) {
        try {
            a.run();
            System.out.println(label + " ok");
        } catch (Throwable t) {
            System.out.println(label + " " + describe(t));
        }
    }

    public static void main(String[] args) throws Exception {
        row("error-first", () -> System.out.println(ErrorInit.v));
        row("error-later", () -> System.out.println(ErrorInit.v));
        row("nomsg-first", () -> System.out.println(NoMessageInit.v));
        row("nomsg-later", () -> System.out.println(NoMessageInit.v));

        Thread worker = new Thread(() -> {
            try {
                System.out.println(ThreadInit.v);
            } catch (Throwable t) {
                // recorded below from main
            }
        }, "init-worker");
        worker.start();
        worker.join();
        row("thread-later", () -> System.out.println(ThreadInit.v));

        row("super-first", () -> System.out.println(new Sub()));
        row("super-sub", () -> System.out.println(new Sub()));
        row("super-sub-static", () -> System.out.println(Sub.t));
        row("super-super", () -> System.out.println(FailingSuper.s));

        row("nested-first", () -> System.out.println(A.a));
        row("nested-a", () -> System.out.println(A.a));
        row("nested-b", () -> System.out.println(B.b));

        row("iface-default", () -> System.out.println(new Impl().d()));
        row("iface-static", () -> System.out.println(WithDefault.st()));
        row("iface-impl-again", () -> System.out.println(new Impl().d()));

        row("plain-first", () -> System.out.println(Plain.p));
        Object o = "not plain";
        row("no-init", () -> {
            boolean is = o instanceof Plain;
            Plain[] arr = new Plain[2];
            Class<?> c = Plain.class;
            Object n = null;
            Plain cast = (Plain) n;
            System.out.println("instanceof=" + is + " array=" + arr.length + " class="
                    + c.getSimpleName() + " cast=" + cast);
        });
        ClassLoader loader = L7W116InitErrorShapes.class.getClassLoader();
        String name = L7W116InitErrorShapes.class.getName() + "$Plain";
        row("forname-noinit", () -> System.out.println(Class.forName(name, false, loader).getSimpleName()));
        row("forname-init", () -> System.out.println(Class.forName(name, true, loader).getSimpleName()));

        try {
            System.out.println(Plain.p);
        } catch (NoClassDefFoundError e) {
            Throwable cause = e.getCause();
            StackTraceElement top = cause == null || cause.getStackTrace().length == 0
                    ? null
                    : cause.getStackTrace()[0];
            System.out.println("trace " + (top == null ? "none"
                    : top.getClassName() + "." + top.getMethodName() + " line>0=" + (top.getLineNumber() > 0)));
        }
    }
}
