// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L3 -- what the served `Method.invoke`,
// `Constructor.newInstance` and `Field` doors answer and raise. Two switches
// change how the natives derive what they print, never what they print:
//
//   CRATONVM_REFLECT_INVOKE_PLAN=1     `Method.invoke` reads the member's
//                                      facts from a per-VM plan named by the
//                                      mint's `slot` (no by-name reads)
//   CRATONVM_FIELD_DOOR_IDENT_MEMO=1   the `Field` doors take a refusal's
//                                      `Decl.name` / type text from a per-VM
//                                      memo (each refusal row runs twice:
//                                      the first builds the row, the second
//                                      reads it)
//
// Rows: static, virtual (an override), default-method, `Object`-method,
// nestmate-private and caller-sensitive (`Class.forName`) calls, a canonical
// box, varargs, two copies of one `Method`, a constructor, a callee's
// throwable wrapped in `InvocationTargetException`; then every refusal's
// throwable and message, and for the access refusals the frame the throwable
// was built in (`top`) and the frame under this class's (`via`) -- HotSpot
// builds `Method.invoke`'s and `Constructor.newInstance`'s in
// `Reflection.newIllegalAccessException` under `AccessibleObject.checkAccess`
// (behaviour fix of this wave: the served natives raised at their own frame);
// then a warm loop.
//
// Run --compatible and --jdk-only, default and --nojit, with both switches
// unset and =1.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// static 7
// virtual sub
// default d-impl
// object-method 5
// nestmate-private 11
// box-identity true
// cs-static java.util.ArrayList
// varargs 3-x
// copies 7 7
// ctor 9
// ite java.lang.reflect.InvocationTargetException cause=java.lang.IllegalStateException: bad
// private-method java.lang.IllegalAccessException: class L3W74ReflectDoors cannot access a member of class L3W74ReflectDoorsOther with modifiers "private static" top=jdk.internal.reflect.Reflection.newIllegalAccessException via=java.lang.reflect.Method.invoke
// private-method-again java.lang.IllegalAccessException: class L3W74ReflectDoors cannot access a member of class L3W74ReflectDoorsOther with modifiers "private static" top=jdk.internal.reflect.Reflection.newIllegalAccessException via=java.lang.reflect.Method.invoke
// private-ctor java.lang.IllegalAccessException: class L3W74ReflectDoors cannot access a member of class L3W74ReflectDoorsOther with modifiers "private" top=jdk.internal.reflect.Reflection.newIllegalAccessException via=java.lang.reflect.Constructor.newInstance
// module-method java.lang.IllegalAccessException top=jdk.internal.reflect.Reflection.newIllegalAccessException via=java.lang.reflect.Method.invoke
// wrong-receiver java.lang.IllegalArgumentException: object of type java.lang.String is not an instance of L3W74ReflectDoors$Target
// arg-count java.lang.IllegalArgumentException: wrong number of arguments: 0 expected: 1
// arg-type java.lang.IllegalArgumentException: argument type mismatch
// null-receiver java.lang.NullPointerException: Cannot invoke "Object.getClass()" because "obj" is null
// field-private-1 java.lang.IllegalAccessException: class L3W74ReflectDoors cannot access a member of class L3W74ReflectDoorsOther with modifiers "private static" top=jdk.internal.reflect.Reflection.newIllegalAccessException via=java.lang.reflect.Field.getInt
// field-private-2 java.lang.IllegalAccessException: class L3W74ReflectDoors cannot access a member of class L3W74ReflectDoorsOther with modifiers "private static" top=jdk.internal.reflect.Reflection.newIllegalAccessException via=java.lang.reflect.Field.getInt
// field-wrong-receiver-1 java.lang.IllegalArgumentException: Can not get int field L3W74ReflectDoors$Holder.count on java.lang.String top=jdk.internal.reflect.MethodHandleFieldAccessorImpl.newGetIllegalArgumentException via=java.lang.reflect.Field.getInt
// field-wrong-receiver-2 java.lang.IllegalArgumentException: Can not get int field L3W74ReflectDoors$Holder.count on java.lang.String top=jdk.internal.reflect.MethodHandleFieldAccessorImpl.newGetIllegalArgumentException via=java.lang.reflect.Field.getInt
// field-conversion-1 java.lang.IllegalArgumentException: Attempt to get long field "L3W74ReflectDoors$Holder.big" with illegal data type conversion to int top=jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException via=java.lang.reflect.Field.getInt
// field-conversion-2 java.lang.IllegalArgumentException: Attempt to get long field "L3W74ReflectDoors$Holder.big" with illegal data type conversion to int top=jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException via=java.lang.reflect.Field.getInt
// field-static-final-1 java.lang.IllegalAccessException: Can not set static final int field L3W74ReflectDoors$Holder.FIXED to (int)9 top=jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException via=java.lang.reflect.Field.setInt
// field-static-final-2 java.lang.IllegalAccessException: Can not set static final int field L3W74ReflectDoors$Holder.FIXED to (int)9 top=jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException via=java.lang.reflect.Field.setInt
// warm sum=400100000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W74ReflectDoors {
    static final int WARM = 20_000;

    public static class Target {
        public static int one(int x) { return x + 1; }
        public int two(int x) { return x + 2; }
        public static void fail() { throw new IllegalStateException("bad"); }
    }

    public static class Base {
        public String m() { return "base"; }
    }

    public static class Sub extends Base {
        @Override
        public String m() { return "sub"; }
    }

    public interface Iface {
        default String d() { return "d-default"; }
    }

    public static class Impl implements Iface {
        @Override
        public String d() { return "d-impl"; }
    }

    static class Nest {
        private static int secret() { return 11; }
    }

    public static class Box {
        public final int v;

        public Box(int v) {
            this.v = v;
        }
    }

    public static class Holder {
        public static long big = 5L;
        public static final int FIXED = 4;
        public int count = 3;
    }

    static String frame(StackTraceElement e) {
        return e.getClassName() + "." + e.getMethodName();
    }

    /** The frame the throwable was built in, and the frame under this class's. */
    static String where(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        String top = st.length > 0 ? frame(st[0]) : "-";
        String via = "-";
        for (int i = 0; i < st.length; i++) {
            if (st[i].getClassName().equals("L3W74ReflectDoors")) {
                if (i > 0) {
                    via = frame(st[i - 1]);
                }
                break;
            }
        }
        return "top=" + top + " via=" + via;
    }

    static void report(String row, Throwable t) {
        System.out.println(row + " " + t.getClass().getName() + ": " + t.getMessage() + " " + where(t));
    }

    static void message(String row, Throwable t) {
        System.out.println(row + " " + t.getClass().getName() + ": " + t.getMessage());
    }

    public static void main(String[] args) throws Exception {
        Method one = Target.class.getMethod("one", int.class);
        Method two = Target.class.getMethod("two", int.class);
        Target t = new Target();

        System.out.println("static " + one.invoke(null, 6));
        System.out.println("virtual " + Base.class.getMethod("m").invoke(new Sub()));
        System.out.println("default " + Iface.class.getMethod("d").invoke(new Impl()));
        System.out.println("object-method " + Object.class.getMethod("hashCode").invoke(Integer.valueOf(5)));
        System.out.println("nestmate-private " + Nest.class.getDeclaredMethod("secret").invoke(null));
        System.out.println("box-identity " + (one.invoke(null, 6) == (Object) Integer.valueOf(7)));
        Object forName = Class.class.getMethod("forName", String.class).invoke(null, "java.util.ArrayList");
        System.out.println("cs-static " + ((Class<?>) forName).getName());
        Method format = String.class.getMethod("format", String.class, Object[].class);
        System.out.println("varargs " + format.invoke(null, "%d-%s", new Object[] {3, "x"}));
        Method oneAgain = Target.class.getMethod("one", int.class);
        System.out.println("copies " + one.invoke(null, 6) + " " + oneAgain.invoke(null, 6));
        Constructor<Box> boxCtor = Box.class.getConstructor(int.class);
        System.out.println("ctor " + boxCtor.newInstance(9).v);
        try {
            Target.class.getMethod("fail").invoke(null);
            System.out.println("ite no throw");
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            System.out.println("ite " + e.getClass().getName() + " cause=" + c.getClass().getName() + ": "
                    + c.getMessage());
        }

        Method hidden = L3W74ReflectDoorsOther.class.getDeclaredMethod("hidden");
        try {
            hidden.invoke(null);
            System.out.println("private-method no throw");
        } catch (IllegalAccessException e) {
            report("private-method", e);
        }
        try {
            hidden.invoke(null);
            System.out.println("private-method-again no throw");
        } catch (IllegalAccessException e) {
            report("private-method-again", e);
        }
        Constructor<L3W74ReflectDoorsOther> hiddenCtor =
                L3W74ReflectDoorsOther.class.getDeclaredConstructor(int.class);
        try {
            hiddenCtor.newInstance(1);
            System.out.println("private-ctor no throw");
        } catch (IllegalAccessException e) {
            report("private-ctor", e);
        }
        Method isBooted = Class.forName("jdk.internal.misc.VM").getMethod("isBooted");
        try {
            isBooted.invoke(null);
            System.out.println("module-method no throw");
        } catch (IllegalAccessException e) {
            System.out.println("module-method " + e.getClass().getName() + " " + where(e));
        }
        try {
            two.invoke("str", 1);
            System.out.println("wrong-receiver no throw");
        } catch (IllegalArgumentException e) {
            message("wrong-receiver", e);
        }
        try {
            one.invoke(null);
            System.out.println("arg-count no throw");
        } catch (IllegalArgumentException e) {
            message("arg-count", e);
        }
        try {
            one.invoke(null, "x");
            System.out.println("arg-type no throw");
        } catch (IllegalArgumentException e) {
            message("arg-type", e);
        }
        try {
            two.invoke(null, 1);
            System.out.println("null-receiver no throw");
        } catch (NullPointerException e) {
            message("null-receiver", e);
        }

        Field secret = L3W74ReflectDoorsOther.class.getDeclaredField("secret");
        Field count = Holder.class.getDeclaredField("count");
        Field big = Holder.class.getDeclaredField("big");
        Field fixed = Holder.class.getDeclaredField("FIXED");
        for (int round = 1; round <= 2; round++) {
            try {
                secret.getInt(null);
                System.out.println("field-private-" + round + " no throw");
            } catch (IllegalAccessException e) {
                report("field-private-" + round, e);
            }
            try {
                count.getInt("str");
                System.out.println("field-wrong-receiver-" + round + " no throw");
            } catch (IllegalArgumentException e) {
                report("field-wrong-receiver-" + round, e);
            }
            try {
                big.getInt(null);
                System.out.println("field-conversion-" + round + " no throw");
            } catch (IllegalArgumentException e) {
                report("field-conversion-" + round, e);
            }
            try {
                fixed.setInt(null, 9);
                System.out.println("field-static-final-" + round + " no throw");
            } catch (IllegalAccessException e) {
                report("field-static-final-" + round, e);
            }
        }

        // Warm: the static and virtual doors and an instance field read.
        Holder h = new Holder();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += (Integer) one.invoke(null, i) + (Integer) two.invoke(t, i) + count.getInt(h);
        }
        System.out.println("warm sum=" + sum);
    }
}

class L3W74ReflectDoorsOther {
    private static int secret = 13;

    private L3W74ReflectDoorsOther(int x) {
    }

    private static int hidden() {
        return 1;
    }
}
