// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L4 --
// docs/internal/fixed-bugs/interpreter-L4-a-varhandle-null-wrapper-variable-names-the-wrong-unboxing-FIXED-20261023.md:
// a VarHandle over a variable typed as a WRAPPER that holds `null`, read at
// that wrapper's own primitive, unboxes through `ValueConversions.unbox<W>(<W>
// x)` on HotSpot, whose NPE names `"x"`; a wrapper read at another primitive,
// or any other reference variable, goes through `primitiveConversion`
// (`Number.<p>Value()`). Every access mode that returns the variable, and
// the method-handle twin, which was already right. First row repeated last.
//
// Run: javac -d out L4W59VarHandleOwnWrapperNull.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W59VarHandleOwnWrapperNull
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical)
// int-int: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// short-short: java.lang.NullPointerException: Cannot invoke "java.lang.Short.shortValue()" because "x" is null
// short-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
// char-char: java.lang.NullPointerException: Cannot invoke "java.lang.Character.charValue()" because "x" is null
// char-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
// long-long: java.lang.NullPointerException: Cannot invoke "java.lang.Long.longValue()" because "x" is null
// bool-bool: java.lang.NullPointerException: Cannot invoke "java.lang.Boolean.booleanValue()" because "x" is null
// byte-byte: java.lang.NullPointerException: Cannot invoke "java.lang.Byte.byteValue()" because "x" is null
// double-double: java.lang.NullPointerException: Cannot invoke "java.lang.Double.doubleValue()" because "x" is null
// float-float: java.lang.NullPointerException: Cannot invoke "java.lang.Float.floatValue()" because "x" is null
// int-int-volatile: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// int-int-acquire: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// int-int-getAndSet: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// int-int-compareAndExchange: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// int-long: java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
// static-int-int: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// int-int-present: 7
// mh-int-int: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// int-int-again: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W59VarHandleOwnWrapperNull {
    Integer i;
    Short s;
    Character c;
    Long l;
    Boolean z;
    Byte b;
    Double d;
    Float f;
    Integer seven = 7;
    static Integer si;

    interface Body { Object run() throws Throwable; }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Class<?> me = L4W59VarHandleOwnWrapperNull.class;
        L4W59VarHandleOwnWrapperNull h = new L4W59VarHandleOwnWrapperNull();
        VarHandle i = lk.findVarHandle(me, "i", Integer.class);
        VarHandle s = lk.findVarHandle(me, "s", Short.class);
        VarHandle c = lk.findVarHandle(me, "c", Character.class);
        VarHandle l = lk.findVarHandle(me, "l", Long.class);
        VarHandle z = lk.findVarHandle(me, "z", Boolean.class);
        VarHandle b = lk.findVarHandle(me, "b", Byte.class);
        VarHandle d = lk.findVarHandle(me, "d", Double.class);
        VarHandle f = lk.findVarHandle(me, "f", Float.class);
        VarHandle seven = lk.findVarHandle(me, "seven", Integer.class);
        VarHandle si = lk.findStaticVarHandle(me, "si", Integer.class);
        row("int-int", () -> (int) i.get(h));
        row("short-short", () -> (short) s.get(h));
        row("short-int", () -> (int) s.get(h));
        row("char-char", () -> (char) c.get(h));
        row("char-int", () -> (int) c.get(h));
        row("long-long", () -> (long) l.get(h));
        row("bool-bool", () -> (boolean) z.get(h));
        row("byte-byte", () -> (byte) b.get(h));
        row("double-double", () -> (double) d.get(h));
        row("float-float", () -> (float) f.get(h));
        row("int-int-volatile", () -> (int) i.getVolatile(h));
        row("int-int-acquire", () -> (int) i.getAcquire(h));
        row("int-int-getAndSet", () -> (int) i.getAndSet(h, (Integer) null));
        row("int-int-compareAndExchange", () -> (int) i.compareAndExchange(h, (Integer) null, (Integer) null));
        row("int-long", () -> (long) i.get(h));
        row("static-int-int", () -> (int) si.get());
        row("int-int-present", () -> (int) seven.get(h));
        MethodHandle mh = lk.findGetter(me, "i", Integer.class).asType(MethodType.methodType(int.class, me));
        row("mh-int-int", () -> (int) mh.invokeExact(h));
        row("int-int-again", () -> (int) i.get(h));
    }
}
