// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L3 -- what a `Field` value door answers,
// raises and initializes. CRATONVM_FIELD_DOORS_INTRINSIC=1 makes `--jdk-only`
// serve `Field.get*` / `set*` with the natives `--compatible` runs (reviewed
// `Intrinsic`s) instead of the JDK's `Field` bytecode and its accessor's
// method handle; nothing below may move.
//
// Rows: plain reads and writes (static, instance, widening, the Object getter,
// a final instance field through the override, a private field through the
// override); the declaring class initialized by the first value access, not by
// `getDeclaredField`; a failing <clinit> (ExceptionInInitializerError, then
// NoClassDefFoundError); and every refusal's throwable, message, the frame it
// was built in (`top`) and the `Field` frame under the caller (`via`):
// a private member of a non-nestmate, a helpful NPE from the access check's
// `obj.getClass()`, the override's null receiver, a wrong receiver, a
// conversion refusal, a static final write, a wrong value type; then
// `Field.getInt` itself reached through `Method.invoke`, and a warm loop, so
// cached and compiled paths run too.
//
// Run --compatible and --jdk-only, default and --nojit, with
// CRATONVM_FIELD_DOORS_INTRINSIC unset and =1.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// static-get 7
// obj-get 7 n
// inst 3 set 11 long 11 double 11.0
// final-inst 5
// private-acc 13
// lazy: got field
// Lazy.<clinit>
// lazy 41
// bad-1 java.lang.ExceptionInInitializerError cause=java.lang.RuntimeException: boom top=jdk.internal.misc.Unsafe.ensureClassInitialized0 via=java.lang.reflect.Field.getInt
// bad-2 java.lang.NoClassDefFoundError: Could not initialize class L3W73JdkOnlyFieldDoors$Bad top=jdk.internal.misc.Unsafe.ensureClassInitialized0 via=java.lang.reflect.Field.getInt
// private java.lang.IllegalAccessException: class L3W73JdkOnlyFieldDoors cannot access a member of class L3W73FieldDoorsOther with modifiers "private static" top=jdk.internal.reflect.Reflection.newIllegalAccessException via=java.lang.reflect.Field.getInt
// null-receiver java.lang.NullPointerException: Cannot invoke "Object.getClass()" because "obj" is null top=java.lang.reflect.Field.checkAccess via=java.lang.reflect.Field.getInt
// null-receiver-acc java.lang.NullPointerException: null top=java.util.Objects.requireNonNull via=java.lang.reflect.Field.getInt
// wrong-receiver java.lang.IllegalArgumentException: Can not get int field L3W73JdkOnlyFieldDoors$Holder.count on java.lang.String top=jdk.internal.reflect.MethodHandleFieldAccessorImpl.newGetIllegalArgumentException via=java.lang.reflect.Field.getInt
// conversion java.lang.IllegalArgumentException: Attempt to get long field "L3W73JdkOnlyFieldDoors$Holder.big" with illegal data type conversion to int top=jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException via=java.lang.reflect.Field.getInt
// static-final java.lang.IllegalAccessException: Can not set static final int field L3W73JdkOnlyFieldDoors$Holder.FIXED to (int)9 top=jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException via=java.lang.reflect.Field.setInt
// wrong-value java.lang.IllegalArgumentException: Can not set int field L3W73JdkOnlyFieldDoors$Holder.count to java.lang.String top=jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException via=java.lang.reflect.Field.set
// reflective 13 7
// warm sum=1820000 last=19999
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class L3W73JdkOnlyFieldDoors {
    static final int WARM = 20_000;

    public static class Holder {
        public static int seven = 7;
        public static String name = "n";
        public static long big = 5L;
        public static final int FIXED = 4;
        public int count = 3;
        public final int fin;

        public Holder() {
            fin = 1;
        }
    }

    public static class Lazy {
        public static int v = init();

        static int init() {
            System.out.println("Lazy.<clinit>");
            return 41;
        }
    }

    public static class Bad {
        public static int v = boom();

        static int boom() {
            throw new RuntimeException("boom");
        }
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
            if (st[i].getClassName().equals("L3W73JdkOnlyFieldDoors")) {
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

    public static void main(String[] args) throws Exception {
        Field seven = Holder.class.getDeclaredField("seven");
        Field name = Holder.class.getDeclaredField("name");
        Field big = Holder.class.getDeclaredField("big");
        Field fixed = Holder.class.getDeclaredField("FIXED");
        Field count = Holder.class.getDeclaredField("count");
        Field fin = Holder.class.getDeclaredField("fin");
        Holder h = new Holder();

        System.out.println("static-get " + seven.getInt(null));
        System.out.println("obj-get " + seven.get(null) + " " + name.get(null));
        int before = count.getInt(h);
        count.setInt(h, 11);
        System.out.println("inst " + before + " set " + h.count + " long " + count.getLong(h)
                + " double " + count.getDouble(h));
        fin.setAccessible(true);
        fin.setInt(h, 5);
        System.out.println("final-inst " + fin.getInt(h));
        Field secretAcc = L3W73FieldDoorsOther.class.getDeclaredField("secret");
        secretAcc.setAccessible(true);
        System.out.println("private-acc " + secretAcc.getInt(null));

        Field lazy = Lazy.class.getDeclaredField("v");
        System.out.println("lazy: got field");
        System.out.println("lazy " + lazy.getInt(null));

        Field bad = Bad.class.getDeclaredField("v");
        try {
            bad.getInt(null);
            System.out.println("bad-1 no throw");
        } catch (ExceptionInInitializerError e) {
            Throwable c = e.getCause();
            System.out.println("bad-1 " + e.getClass().getName() + " cause=" + c.getClass().getName()
                    + ": " + c.getMessage() + " " + where(e));
        }
        try {
            bad.getInt(null);
            System.out.println("bad-2 no throw");
        } catch (NoClassDefFoundError e) {
            report("bad-2", e);
        }

        Field secret = L3W73FieldDoorsOther.class.getDeclaredField("secret");
        try {
            secret.getInt(null);
            System.out.println("private no throw");
        } catch (IllegalAccessException e) {
            report("private", e);
        }
        try {
            count.getInt(null);
            System.out.println("null-receiver no throw");
        } catch (NullPointerException e) {
            report("null-receiver", e);
        }
        Field countAcc = Holder.class.getDeclaredField("count");
        countAcc.setAccessible(true);
        try {
            countAcc.getInt(null);
            System.out.println("null-receiver-acc no throw");
        } catch (NullPointerException e) {
            report("null-receiver-acc", e);
        }
        try {
            count.getInt("str");
            System.out.println("wrong-receiver no throw");
        } catch (IllegalArgumentException e) {
            report("wrong-receiver", e);
        }
        try {
            big.getInt(null);
            System.out.println("conversion no throw");
        } catch (IllegalArgumentException e) {
            report("conversion", e);
        }
        try {
            fixed.setInt(null, 9);
            System.out.println("static-final no throw");
        } catch (IllegalAccessException e) {
            report("static-final", e);
        }
        try {
            count.set(h, "x");
            System.out.println("wrong-value no throw");
        } catch (IllegalArgumentException e) {
            report("wrong-value", e);
        }
        Method getInt = Field.class.getMethod("getInt", Object.class);
        System.out.println("reflective " + getInt.invoke(secretAcc, (Object) null) + " "
                + getInt.invoke(seven, (Object) null));

        // Warm: the static and instance doors, read and write, many times.
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            count.setInt(h, i);
            sum += seven.getInt(null) + count.getInt(h) - i + ((Integer) seven.get(null)) * 12;
        }
        System.out.println("warm sum=" + sum + " last=" + count.getInt(h));
    }
}

class L3W73FieldDoorsOther {
    private static int secret = 13;
}
