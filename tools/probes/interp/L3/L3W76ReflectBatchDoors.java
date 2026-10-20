// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L3 -- what the served `Method.invoke`
// and `Constructor.newInstance` answer and raise on the paths the wave's
// switch rewrites. The switch changes how the natives read the `Method` /
// `Constructor` words and build the argument vector, never what they print:
//
//   CRATONVM_REFLECT_DOOR_WORD_BATCH=1  a planned call reads `slot`, `clazz`
//                                       and `override` (a constructor:
//                                       `constructorAccessor`, `slot`,
//                                       `override`) in one batch, reads and
//                                       coerces the arguments in one vector,
//                                       and the boxing wrapper takes the
//                                       return descriptor from the plan
//
// Rows: primitive returns of every shape (boxed canonically), void and
// reference returns, an exact int, a widened int -> long, a refused
// narrowing, null for a primitive (IAE with an NPE cause), an exact and a
// subtype reference argument, a refused reference argument, a virtual call,
// an override selected by the receiver, a private method made accessible, a
// private method refused, a wrong argument count, a wrong receiver, copies of
// one method, then constructors (exact, coerced, refused, accessible), then a
// warm loop.
//
// Run --compatible and --jdk-only, default and --nojit, with the switch
// unset and =1.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// static-int 6 java.lang.Integer cached=true
// static-long-widen 8 java.lang.Long
// static-narrow java.lang.IllegalArgumentException: argument type mismatch
// static-null-prim java.lang.IllegalArgumentException cause=java.lang.NullPointerException
// static-str 5 5
// static-charseq 3
// static-str-wrong java.lang.IllegalArgumentException: argument type mismatch
// returns null s true x 1.5 -7 java.lang.Byte
// virtual 9 9
// virtual-override sub base
// private-accessible 11 11
// private-refused java.lang.IllegalAccessException: class L3W76ReflectBatchDoors cannot access a member of class L3W76ReflectBatchDoorsOther with modifiers "private static"
// arg-count java.lang.IllegalArgumentException: wrong number of arguments: 0 expected: 1
// wrong-receiver java.lang.IllegalArgumentException: object of type java.lang.String is not an instance of L3W76ReflectBatchDoors$Target
// copies 2 2 same=false
// ctor-int 4 4
// ctor-str ab
// ctor-mismatch java.lang.IllegalArgumentException: argument type mismatch
// ctor-accessible 5 5
// warm sum=100050000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;

public class L3W76ReflectBatchDoors {
    static final int WARM = 10_000;

    public static class Target {
        public static int one(int x) { return x + 1; }
        public static long lone(long x) { return x + 1; }
        public static int len(String s) { return s.length(); }
        public static int cs(CharSequence s) { return s.length(); }
        public static void nothing() { }
        public static String str() { return "s"; }
        public static boolean yes() { return true; }
        public static char ex() { return 'x'; }
        public static double half() { return 1.5; }
        public static byte neg() { return (byte) -7; }
        private static int priv(int x) { return x + 10; }
        public int two(int x) { return x + 2; }
    }

    public static class Base {
        public String who() { return "base"; }
    }

    public static class Sub extends Base {
        @Override
        public String who() { return "sub"; }
    }

    public static class Box {
        public final String v;

        public Box(int v) {
            this.v = String.valueOf(v);
        }

        public Box(String v) {
            this.v = v;
        }

        private Box(long v) {
            this.v = String.valueOf(v + 1);
        }
    }

    static void error(String row, Throwable t) {
        System.out.println(row + " " + t.getClass().getName() + ": " + t.getMessage());
    }

    public static void main(String[] args) throws Exception {
        Method one = Target.class.getMethod("one", int.class);
        Object r = one.invoke(null, 5);
        System.out.println("static-int " + r + " " + r.getClass().getName() + " cached="
                + (r == Integer.valueOf(6)));

        Method lone = Target.class.getMethod("lone", long.class);
        Object lr = lone.invoke(null, Integer.valueOf(7));
        System.out.println("static-long-widen " + lr + " " + lr.getClass().getName());

        try {
            one.invoke(null, Long.valueOf(3));
            System.out.println("static-narrow no throw");
        } catch (Throwable t) {
            error("static-narrow", t);
        }

        try {
            one.invoke(null, (Object) null);
            System.out.println("static-null-prim no throw");
        } catch (IllegalArgumentException e) {
            Throwable c = e.getCause();
            System.out.println("static-null-prim " + e.getClass().getName() + " cause="
                    + (c == null ? "none" : c.getClass().getName()));
        }

        Method len = Target.class.getMethod("len", String.class);
        System.out.println("static-str " + len.invoke(null, "abcde") + " " + len.invoke(null, "fghij"));
        Method cs = Target.class.getMethod("cs", CharSequence.class);
        System.out.println("static-charseq " + cs.invoke(null, "abc"));
        try {
            len.invoke(null, Integer.valueOf(1));
            System.out.println("static-str-wrong no throw");
        } catch (Throwable t) {
            error("static-str-wrong", t);
        }

        Object neg = Target.class.getMethod("neg").invoke(null);
        System.out.println("returns " + Target.class.getMethod("nothing").invoke(null)
                + " " + Target.class.getMethod("str").invoke(null)
                + " " + Target.class.getMethod("yes").invoke(null)
                + " " + Target.class.getMethod("ex").invoke(null)
                + " " + Target.class.getMethod("half").invoke(null)
                + " " + neg + " " + neg.getClass().getName());

        Method two = Target.class.getMethod("two", int.class);
        Target target = new Target();
        System.out.println("virtual " + two.invoke(target, 7) + " " + two.invoke(target, 7));
        Method who = Base.class.getMethod("who");
        System.out.println("virtual-override " + who.invoke(new Sub()) + " " + who.invoke(new Base()));

        Method priv = Target.class.getDeclaredMethod("priv", int.class);
        priv.setAccessible(true);
        System.out.println("private-accessible " + priv.invoke(null, 1) + " " + priv.invoke(null, 1));

        Method hidden = L3W76ReflectBatchDoorsOther.class.getDeclaredMethod("hidden", int.class);
        try {
            hidden.invoke(null, 1);
            System.out.println("private-refused no throw");
        } catch (Throwable t) {
            error("private-refused", t);
        }

        try {
            one.invoke(null);
            System.out.println("arg-count no throw");
        } catch (Throwable t) {
            error("arg-count", t);
        }

        try {
            two.invoke("not a target", 1);
            System.out.println("wrong-receiver no throw");
        } catch (Throwable t) {
            error("wrong-receiver", t);
        }

        Method c1 = Target.class.getMethod("one", int.class);
        Method c2 = Target.class.getMethod("one", int.class);
        System.out.println("copies " + c1.invoke(null, 1) + " " + c2.invoke(null, 1) + " same=" + (c1 == c2));

        Constructor<Box> bi = Box.class.getConstructor(int.class);
        System.out.println("ctor-int " + bi.newInstance(4).v + " " + bi.newInstance(Short.valueOf((short) 4)).v);
        Constructor<Box> bs = Box.class.getConstructor(String.class);
        System.out.println("ctor-str " + bs.newInstance("ab").v);
        try {
            bi.newInstance("x");
            System.out.println("ctor-mismatch no throw");
        } catch (Throwable t) {
            error("ctor-mismatch", t);
        }
        Constructor<Box> bl = Box.class.getDeclaredConstructor(long.class);
        bl.setAccessible(true);
        System.out.println("ctor-accessible " + bl.newInstance(4L).v + " " + bl.newInstance(4).v);

        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += (Integer) one.invoke(null, i);
            sum += (Integer) two.invoke(target, i);
            sum += (Integer) len.invoke(null, "abc");
        }
        System.out.println("warm sum=" + sum);
    }
}

class L3W76ReflectBatchDoorsOther {
    private static int hidden(int x) { return x; }
}
