// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L3 -- class initialization through the
// doors whose "is this class initialized" test CRATONVM_CLASS_INIT_BIT_FIRST
// changes (reflection, method handles, getstatic), and the JVMS 5.5 cases the
// lane's review checked against HotSpot:
//
//   reflect-invoke / field-get / handle / new-instance
//       the door initializes the declaring class once, on first use, not at
//       lookup (getDeclaredMethod / findStatic do not initialize);
//   iface-*  a class's initialization first initializes each superinterface
//       that declares a default method, and no other: direct `new`,
//       Constructor.newInstance, a findConstructor handle, a static
//       Method.invoke;
//   recursive  a <clinit> that reaches its own class again (same thread) reads
//       the default value of a static not yet assigned;
//   concurrent two threads racing to initialize one class: <clinit> runs once
//       and both see its result;
//   bad-*    a failed <clinit>: ExceptionInInitializerError first, then
//       NoClassDefFoundError "Could not initialize class ..." with the
//       recorded ExceptionInInitializerError as its cause, through a direct
//       call, Method.invoke (not wrapped in InvocationTargetException),
//       a method handle and Field.getInt.
//
// Run every mode and with CRATONVM_CLASS_INIT_BIT_FIRST unset and =1, default
// and --nojit; the output must not move.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// reflect-invoke before=[] after=[A;] r=1
// field-get before=[] after=[B;] r=2
// handle before=[] found=[] after=[C;] r=3
// new-instance before=[] after=[D;] v=4
// iface-new [WithDefault;Impl1;]
// iface-new-no-default [Impl2;] then [Impl2;NoDefault;]
// iface-reflect-ctor [WithDefault2;Impl3;]
// iface-handle-ctor [WithDefault3;Impl4;]
// iface-reflect-static [WithDefault4;Impl5;] r=5
// recursive b=0 c=7
// concurrent runs=1 seen=42,42
// bad-first java.lang.ExceptionInInitializerError: null cause=java.lang.IllegalStateException: bad
// bad-direct java.lang.NoClassDefFoundError: Could not initialize class L3W71ClassInitDoors$Bad cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: bad [in thread "main"]
// bad-reflect java.lang.NoClassDefFoundError: Could not initialize class L3W71ClassInitDoors$Bad cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: bad [in thread "main"]
// bad-handle java.lang.NoClassDefFoundError: Could not initialize class L3W71ClassInitDoors$Bad cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: bad [in thread "main"]
// bad-field java.lang.NoClassDefFoundError: Could not initialize class L3W71ClassInitDoors$Bad cause=java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException: bad [in thread "main"]
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

public class L3W71ClassInitDoors {
    static final StringBuffer LOG = new StringBuffer();

    static String take() {
        String s = "[" + LOG + "]";
        LOG.setLength(0);
        return s;
    }

    static String peek() {
        return "[" + LOG + "]";
    }

    static int q(String s) {
        LOG.append(s).append(';');
        return 1;
    }

    public static class A {
        static int x = 1;
        static { LOG.append("A;"); }
        public static int m() { return x; }
    }

    public static class B {
        public static int y = 2;
        static { LOG.append("B;"); }
    }

    public static class C {
        static { LOG.append("C;"); }
        public static int k() { return 3; }
    }

    public static class D {
        static int v;
        static { LOG.append("D;"); v = 4; }
        public D() {}
    }

    public interface WithDefault { int Q = q("WithDefault"); default int d() { return 1; } }
    public interface NoDefault { int R = q("NoDefault"); int f(); }
    public interface WithDefault2 { int Q = q("WithDefault2"); default int d() { return 1; } }
    public interface WithDefault3 { int Q = q("WithDefault3"); default int d() { return 1; } }
    public interface WithDefault4 { int Q = q("WithDefault4"); default int d() { return 1; } }

    public static class Impl1 implements WithDefault {
        static { LOG.append("Impl1;"); }
        public Impl1() {}
    }

    public static class Impl2 implements NoDefault {
        static { LOG.append("Impl2;"); }
        public Impl2() {}
        public int f() { return 0; }
    }

    public static class Impl3 implements WithDefault2 {
        static { LOG.append("Impl3;"); }
        public Impl3() {}
    }

    public static class Impl4 implements WithDefault3 {
        static { LOG.append("Impl4;"); }
        public Impl4() {}
    }

    public static class Impl5 implements WithDefault4 {
        static { LOG.append("Impl5;"); }
        public static int s() { return 5; }
    }

    public static class Rec {
        static int a = 5;
        static int b = RecPeer.peek();
        static int c = 7;
    }

    public static class RecPeer {
        static int peek() { return Rec.c; }
    }

    public static class Slow {
        static int runs;
        static int value;
        static {
            runs++;
            try {
                Thread.sleep(100);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            value = 42;
        }
    }

    public static class Bad {
        public static int z = boom();
        static int boom() { throw new IllegalStateException("bad"); }
        public static int get() { return z; }
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage() + " cause=" + t.getCause();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();

        String before = take();
        Method m = A.class.getDeclaredMethod("m");
        Object r = m.invoke(null);
        System.out.println("reflect-invoke before=" + before + " after=" + take() + " r=" + r);

        before = take();
        int y = B.class.getDeclaredField("y").getInt(null);
        System.out.println("field-get before=" + before + " after=" + take() + " r=" + y);

        before = take();
        MethodHandle k = lookup.findStatic(C.class, "k", MethodType.methodType(int.class));
        String found = peek();
        int kr = (int) k.invokeExact();
        System.out.println("handle before=" + before + " found=" + found + " after=" + take() + " r=" + kr);

        before = take();
        D.class.getDeclaredConstructor().newInstance();
        System.out.println("new-instance before=" + before + " after=" + take() + " v=" + D.v);

        new Impl1();
        System.out.println("iface-new " + take());

        new Impl2();
        String first = peek();
        int rr = NoDefault.R;
        System.out.println("iface-new-no-default " + first + " then " + take());

        Impl3.class.getDeclaredConstructor().newInstance();
        System.out.println("iface-reflect-ctor " + take());

        Object o = lookup.findConstructor(Impl4.class, MethodType.methodType(void.class)).invoke();
        System.out.println("iface-handle-ctor " + take());

        Object s = Impl5.class.getDeclaredMethod("s").invoke(null);
        System.out.println("iface-reflect-static " + take() + " r=" + s);

        System.out.println("recursive b=" + Rec.b + " c=" + Rec.c);

        int[] seen = new int[2];
        Thread[] ts = new Thread[2];
        for (int i = 0; i < 2; i++) {
            final int idx = i;
            ts[i] = new Thread(() -> seen[idx] = Slow.value);
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        System.out.println("concurrent runs=" + Slow.runs + " seen=" + seen[0] + "," + seen[1]);

        try {
            Bad.get();
            System.out.println("bad-first none");
        } catch (Throwable t) {
            System.out.println("bad-first " + describe(t));
        }
        try {
            Bad.get();
            System.out.println("bad-direct none");
        } catch (Throwable t) {
            System.out.println("bad-direct " + describe(t));
        }
        try {
            Bad.class.getDeclaredMethod("get").invoke(null);
            System.out.println("bad-reflect none");
        } catch (Throwable t) {
            System.out.println("bad-reflect " + describe(t));
        }
        try {
            MethodHandle g = lookup.findStatic(Bad.class, "get", MethodType.methodType(int.class));
            int gr = (int) g.invokeExact();
            System.out.println("bad-handle none " + gr);
        } catch (Throwable t) {
            System.out.println("bad-handle " + describe(t));
        }
        try {
            Bad.class.getDeclaredField("z").getInt(null);
            System.out.println("bad-field none");
        } catch (Throwable t) {
            System.out.println("bad-field " + describe(t));
        }
        if (o == null || rr != 1) {
            System.out.println("unexpected");
        }
    }
}
