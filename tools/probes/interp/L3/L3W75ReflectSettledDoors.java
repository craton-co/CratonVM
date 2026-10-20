// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L3 -- what the served `Method.invoke`
// (static targets) and `Constructor.newInstance` answer and raise, first call
// and settled. Two switches change how the natives reach the answer, never
// what they print:
//
//   CRATONVM_REFLECT_INVOKE_SETTLED=1    a planned static target outside
//                                        java/ and jdk/ dispatches through the
//                                        settling static door with the owner
//                                        its plan remembers
//   CRATONVM_REFLECT_CTOR_PLAN_PER_VM=1  constructor plans live in the per-VM
//                                        reflect-door caches, keep the class
//                                        flags, and skip the `exports` walk
//                                        when it cannot refuse
//
// Rows (each static row is called at least twice: the first call settles):
// a static method whose class initializes on the first call, a static whose
// class initializer fails (the error unwrapped, then NoClassDefFoundError), a
// callee's throwable wrapped in InvocationTargetException (with the frame it
// was thrown in), a static taking a lambda named like its functional method,
// an interface static, an inherited static, a static taking an object, a
// reentrant static, a static called from a second thread; then constructors:
// public, class-initializing, copies, abstract, enum, private (refused, then
// overridden), a public constructor of a non-exported JDK class, a public JDK
// constructor, a throwing constructor, a wrong argument count; then a warm
// loop.
//
// Run --compatible and --jdk-only, default and --nojit, with both switches
// unset and =1.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// static-first 41 42 log=lazy-init;
// static-eiie-1 java.lang.ExceptionInInitializerError cause=java.lang.RuntimeException: clinit
// static-eiie-2 java.lang.NoClassDefFoundError: Could not initialize class L3W75ReflectSettledDoors$Broken
// static-fail-1 java.lang.reflect.InvocationTargetException cause=java.lang.IllegalStateException: bad at=L3W75ReflectSettledDoors$Target.fail
// static-fail-2 java.lang.reflect.InvocationTargetException cause=java.lang.IllegalStateException: bad at=L3W75ReflectSettledDoors$Target.fail
// static-lambda-arg 105 105
// static-interface 14 14
// static-inherited 3 3 declaring=L3W75ReflectSettledDoors$SBase
// static-object-arg 5 5
// static-reentrant 6 6
// static-thread 8
// ctor 9 9
// ctor-class-init 2 2 log=lazy-init;inited-init;
// ctor-copies 1 2
// ctor-abstract java.lang.InstantiationException: null
// ctor-enum java.lang.IllegalArgumentException: Cannot reflectively create enum objects
// ctor-private java.lang.IllegalAccessException: class L3W75ReflectSettledDoors cannot access a member of class L3W75ReflectSettledDoorsOther with modifiers "private"
// ctor-accessible 4 4
// ctor-module java.lang.IllegalAccessException exported=false
// ctor-jdk-public ab ab
// ctor-ite java.lang.reflect.InvocationTargetException cause=java.lang.IllegalStateException: ctor-bad
// ctor-arg-count java.lang.IllegalArgumentException: wrong number of arguments: 0 expected: 1
// warm sum=150395000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.function.IntSupplier;

public class L3W75ReflectSettledDoors {
    static final int WARM = 10_000;
    static final StringBuilder LOG = new StringBuilder();

    public static class Target {
        public static int one(int x) { return x + 1; }
        public static void fail() { throw new IllegalStateException("bad"); }
        public static int getAsInt(IntSupplier s) { return s.getAsInt() + 100; }
        public static int len(String s) { return s.length(); }
    }

    public static class Lazy {
        static int base;

        static {
            LOG.append("lazy-init;");
            base = 40;
        }

        public static int value(int x) { return base + x; }
    }

    static int boom() {
        throw new RuntimeException("clinit");
    }

    public static class Broken {
        static final int X = boom();

        public static int v() { return X; }
    }

    public interface IfaceS {
        static int twice(int x) { return 2 * x; }
    }

    public static class SBase {
        public static int base3() { return 3; }
    }

    public static class SSub extends SBase {
    }

    static Method rec;

    public static int rec(int n) throws Exception {
        return n == 0 ? 0 : n + (Integer) rec.invoke(null, n - 1);
    }

    public static class Box {
        public final int v;

        public Box(int v) {
            this.v = v;
        }
    }

    public static class Inited {
        static int count;

        static {
            LOG.append("inited-init;");
        }

        public final int n;

        public Inited() {
            n = ++count;
        }
    }

    public abstract static class Abs {
        public Abs() {
        }
    }

    enum Color { RED }

    public static class Thrower {
        public Thrower() {
            throw new IllegalStateException("ctor-bad");
        }
    }

    static String cause(Throwable e) {
        Throwable c = e.getCause();
        return c == null ? "none" : c.getClass().getName() + ": " + c.getMessage();
    }

    static void error(String row, Throwable t) {
        System.out.println(row + " " + t.getClass().getName() + ": " + t.getMessage());
    }

    public static void main(String[] args) throws Exception {
        Method value = Lazy.class.getMethod("value", int.class);
        int a = (Integer) value.invoke(null, 1);
        int b = (Integer) value.invoke(null, 2);
        System.out.println("static-first " + a + " " + b + " log=" + LOG);

        Method v = Broken.class.getMethod("v");
        for (int round = 1; round <= 2; round++) {
            try {
                v.invoke(null);
                System.out.println("static-eiie-" + round + " no throw");
            } catch (ExceptionInInitializerError e) {
                System.out.println("static-eiie-" + round + " " + e.getClass().getName() + " cause=" + cause(e));
            } catch (Throwable t) {
                error("static-eiie-" + round, t);
            }
        }

        Method fail = Target.class.getMethod("fail");
        for (int round = 1; round <= 2; round++) {
            try {
                fail.invoke(null);
                System.out.println("static-fail-" + round + " no throw");
            } catch (InvocationTargetException e) {
                StackTraceElement[] st = e.getCause().getStackTrace();
                String at = st.length > 0 ? st[0].getClassName() + "." + st[0].getMethodName() : "-";
                System.out.println("static-fail-" + round + " " + e.getClass().getName() + " cause=" + cause(e)
                        + " at=" + at);
            }
        }

        Method getAsInt = Target.class.getMethod("getAsInt", IntSupplier.class);
        IntSupplier five = () -> 5;
        System.out.println("static-lambda-arg " + getAsInt.invoke(null, five) + " " + getAsInt.invoke(null, five));

        Method twice = IfaceS.class.getMethod("twice", int.class);
        System.out.println("static-interface " + twice.invoke(null, 7) + " " + twice.invoke(null, 7));

        Method base3 = SSub.class.getMethod("base3");
        System.out.println("static-inherited " + base3.invoke(null) + " " + base3.invoke(null)
                + " declaring=" + base3.getDeclaringClass().getName());

        Method len = Target.class.getMethod("len", String.class);
        System.out.println("static-object-arg " + len.invoke(null, "abcde") + " " + len.invoke(null, "fghij"));

        rec = L3W75ReflectSettledDoors.class.getMethod("rec", int.class);
        System.out.println("static-reentrant " + rec.invoke(null, 3) + " " + rec.invoke(null, 3));

        Method one = Target.class.getMethod("one", int.class);
        one.invoke(null, 0);
        int[] fromThread = new int[1];
        Thread t = new Thread(() -> {
            try {
                fromThread[0] = (Integer) one.invoke(null, 7);
            } catch (Exception e) {
                fromThread[0] = -1;
            }
        });
        t.start();
        t.join();
        System.out.println("static-thread " + fromThread[0]);

        Constructor<Box> boxCtor = Box.class.getConstructor(int.class);
        System.out.println("ctor " + boxCtor.newInstance(9).v + " " + boxCtor.newInstance(9).v);

        Constructor<Inited> inited = Inited.class.getConstructor();
        int n1 = inited.newInstance().n;
        int n2 = inited.newInstance().n;
        System.out.println("ctor-class-init " + (n1 + 1) + " " + n2 + " log=" + LOG);

        Constructor<Inited> initedAgain = Inited.class.getConstructor();
        System.out.println("ctor-copies " + (initedAgain.newInstance().n - 2) + " " + (inited.newInstance().n - 2));

        try {
            Abs.class.getConstructor().newInstance();
            System.out.println("ctor-abstract no throw");
        } catch (InstantiationException e) {
            error("ctor-abstract", e);
        }

        Constructor<Color> colorCtor = Color.class.getDeclaredConstructor(String.class, int.class);
        colorCtor.setAccessible(true);
        try {
            colorCtor.newInstance("BLUE", 9);
            System.out.println("ctor-enum no throw");
        } catch (IllegalArgumentException e) {
            error("ctor-enum", e);
        }

        Constructor<L3W75ReflectSettledDoorsOther> hidden =
                L3W75ReflectSettledDoorsOther.class.getDeclaredConstructor(int.class);
        try {
            hidden.newInstance(4);
            System.out.println("ctor-private no throw");
        } catch (IllegalAccessException e) {
            error("ctor-private", e);
        }
        hidden.setAccessible(true);
        System.out.println("ctor-accessible " + hidden.newInstance(4).x + " " + hidden.newInstance(4).x);

        Constructor<?> utf8 = Class.forName("sun.nio.cs.UTF_8").getConstructor();
        try {
            utf8.newInstance();
            System.out.println("ctor-module no throw");
        } catch (IllegalAccessException e) {
            String m = String.valueOf(e.getMessage());
            System.out.println("ctor-module " + e.getClass().getName() + " exported="
                    + !m.contains("does not export sun.nio.cs to unnamed module"));
        }

        Constructor<StringBuilder> sb = StringBuilder.class.getConstructor(String.class);
        System.out.println("ctor-jdk-public " + sb.newInstance("ab") + " " + sb.newInstance("ab"));

        try {
            Thrower.class.getConstructor().newInstance();
            System.out.println("ctor-ite no throw");
        } catch (InvocationTargetException e) {
            System.out.println("ctor-ite " + e.getClass().getName() + " cause=" + cause(e));
        }

        try {
            boxCtor.newInstance();
            System.out.println("ctor-arg-count no throw");
        } catch (IllegalArgumentException e) {
            error("ctor-arg-count", e);
        }

        // Warm: a settled static door and a planned constructor.
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += (Integer) one.invoke(null, i) + boxCtor.newInstance(i).v + (Integer) value.invoke(null, i);
        }
        System.out.println("warm sum=" + sum);
    }
}

class L3W75ReflectSettledDoorsOther {
    final int x;

    private L3W75ReflectSettledDoorsOther(int x) {
        this.x = x;
    }
}
