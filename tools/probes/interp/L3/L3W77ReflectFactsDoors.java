// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L3 -- what the served `Method.invoke`
// answers and raises on the paths CRATONVM_REFLECT_INVOKE_PLAN_FACTS
// rewrites. The switch changes which class-manager reads a planned call
// makes, never what it prints:
//
//   * the class half of the access check takes the plan's kept class flags
//     (rows `hidden-class`, `pkg-exact`);
//   * an exact-class receiver of a virtual call skips the selection-failure
//     question and the declared door's selection (rows `virtual-exact`,
//     `pkg-exact`, `warm`), a receiver of any other class keeps both
//     (`virtual-sub-inherit`, `virtual-override`, `pkg-override`);
//   * `crosses_package` compares the interned names (`pkg-exact`,
//     `pkg-override`);
//   * the boxing wrapper skips the return-shape check of a primitive the
//     body boxed itself (`returns`, `cached`, `class-return`).
//
// CRATONVM_REFLECT_DOOR_PHASE_STATS=1 must not change stdout either (its
// lines go to stderr, and only every 65536 calls).
//
// Run --compatible and --jdk-only, default and --nojit, with
// CRATONVM_REFLECT_INVOKE_PLAN_FACTS unset and =1, and with
// CRATONVM_REFLECT_DOOR_PHASE_STATS=1.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// virtual-exact 9
// virtual-sub-inherit 9
// virtual-override sub
// pkg-exact 5
// pkg-override sub-pkg
// hidden-class 3
// returns 8 Long true Boolean q Character -7 Byte 300 Short 1.5 Float 2.25 Double
// cached int=true bool=true char=true byte=true
// class-return java.lang.String
// void-return null
// wrong-receiver java.lang.IllegalArgumentException: object of type java.lang.String is not an instance of L3W77ReflectFactsDoors$Target
// null-receiver java.lang.NullPointerException
// arg-count java.lang.IllegalArgumentException: wrong number of arguments: 0 expected: 1
// warm sum=50005000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W77ReflectFactsDoors {
    static final int WARM = 10_000;

    public static class Target {
        public int two(int x) { return x + 2; }
        public String who() { return "base"; }
        int four(int x) { return x + 4; }
        String pkg() { return "base-pkg"; }

        public static long l(long x) { return x + 1; }
        public static boolean z() { return true; }
        public static char c() { return 'q'; }
        public static byte b() { return (byte) -7; }
        public static short s() { return (short) 300; }
        public static float f() { return 1.5f; }
        public static double d() { return 2.25; }
        public static int one(int x) { return x + 1; }
        public static Class<?> k() { return String.class; }
        public static void v() { }
        public int self(int x) { return x; }
    }

    public static class SubInherit extends Target {
    }

    public static class SubOverride extends Target {
        @Override
        public String who() { return "sub"; }

        @Override
        String pkg() { return "sub-pkg"; }
    }

    static class Hidden {
        public static int h() { return 3; }
    }

    static String thrown(Throwable t) {
        if (t instanceof InvocationTargetException ite) {
            t = ite.getCause();
        }
        return t.getMessage() == null ? t.getClass().getName() : t.toString();
    }

    public static void main(String[] args) throws Throwable {
        Method two = Target.class.getMethod("two", int.class);
        Method who = Target.class.getMethod("who");
        Method four = Target.class.getDeclaredMethod("four", int.class);
        Method pkg = Target.class.getDeclaredMethod("pkg");
        Target target = new Target();

        System.out.println("virtual-exact " + two.invoke(target, 7));
        System.out.println("virtual-sub-inherit " + two.invoke(new SubInherit(), 7));
        System.out.println("virtual-override " + who.invoke(new SubOverride()));
        System.out.println("pkg-exact " + four.invoke(target, 1));
        System.out.println("pkg-override " + pkg.invoke(new SubOverride()));
        System.out.println("hidden-class " + Hidden.class.getMethod("h").invoke(null));

        Object l = Target.class.getMethod("l", long.class).invoke(null, 7L);
        Object z = Target.class.getMethod("z").invoke(null);
        Object c = Target.class.getMethod("c").invoke(null);
        Object b = Target.class.getMethod("b").invoke(null);
        Object s = Target.class.getMethod("s").invoke(null);
        Object f = Target.class.getMethod("f").invoke(null);
        Object d = Target.class.getMethod("d").invoke(null);
        StringBuilder sb = new StringBuilder("returns");
        for (Object o : new Object[] {l, z, c, b, s, f, d}) {
            sb.append(' ').append(o).append(' ').append(o.getClass().getSimpleName());
        }
        System.out.println(sb);

        Method one = Target.class.getMethod("one", int.class);
        System.out.println("cached int=" + (one.invoke(null, 6) == Integer.valueOf(7))
                + " bool=" + (z == Boolean.TRUE)
                + " char=" + (c == Character.valueOf('q'))
                + " byte=" + (b == Byte.valueOf((byte) -7)));
        System.out.println("class-return " + ((Class<?>) Target.class.getMethod("k").invoke(null)).getName());
        System.out.println("void-return " + Target.class.getMethod("v").invoke(null));

        try {
            two.invoke("x", 1);
            System.out.println("wrong-receiver no throw");
        } catch (Throwable t) {
            System.out.println("wrong-receiver " + thrown(t));
        }
        try {
            two.invoke(null, 1);
            System.out.println("null-receiver no throw");
        } catch (Throwable t) {
            System.out.println("null-receiver " + t.getClass().getName());
        }
        try {
            two.invoke(target);
            System.out.println("arg-count no throw");
        } catch (Throwable t) {
            System.out.println("arg-count " + thrown(t));
        }

        Method self = Target.class.getMethod("self", int.class);
        long sum = 0;
        for (int i = 1; i <= WARM; i++) {
            sum += (Integer) self.invoke(target, i);
        }
        System.out.println("warm sum=" + sum);
    }
}
