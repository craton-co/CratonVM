// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L4 (review): a VarHandle access whose
// REFERENCE variable holds `null`, read into a PRIMITIVE call site, is
// `asType`'s run-time unboxing on HotSpot: a `NullPointerException` whose
// helpful message names the unboxing (`primitiveConversion` for an `Object` /
// `Number` variable, `Integer.intValue()` on "x" for an `Integer` variable at
// its own primitive). Wave 57's rule (`vm_exec::vh_unbox_reference_return`)
// raised the NPE with no message. The probe prints the full text.
//
// Run: javac -d out L4W58VarHandleReturnNullText.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W58VarHandleReturnNullText
//
// Expected HotSpot 25 output: see the block at the end of this file.
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W58VarHandleReturnNullText {
    Object o;
    Integer boxed;
    Number num;
    Object other = 7L;
    static Object so;

    interface Body { Object run() throws Throwable; }

    static void row(String name, Body b) {
        String out;
        try { out = String.valueOf(b.run()); } catch (Throwable t) { out = t.getClass().getName() + ": " + t.getMessage(); }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle o = lk.findVarHandle(L4W58VarHandleReturnNullText.class, "o", Object.class);
        VarHandle boxed = lk.findVarHandle(L4W58VarHandleReturnNullText.class, "boxed", Integer.class);
        VarHandle num = lk.findVarHandle(L4W58VarHandleReturnNullText.class, "num", Number.class);
        VarHandle other = lk.findVarHandle(L4W58VarHandleReturnNullText.class, "other", Object.class);
        VarHandle so = lk.findStaticVarHandle(L4W58VarHandleReturnNullText.class, "so", Object.class);
        VarHandle arr = MethodHandles.arrayElementVarHandle(Object[].class);
        L4W58VarHandleReturnNullText h = new L4W58VarHandleReturnNullText();
        Object[] elems = new Object[1];
        row("object-null-as-int-first", () -> (int) o.get(h));
        row("object-null-as-long", () -> (long) o.get(h));
        row("object-null-as-boolean", () -> (boolean) o.get(h));
        row("object-null-as-double", () -> (double) o.get(h));
        row("integer-null-as-int", () -> (int) boxed.get(h));
        row("integer-null-as-long", () -> (long) boxed.get(h));
        row("number-null-as-int", () -> (int) num.get(h));
        row("static-null-as-int", () -> (int) so.get());
        row("array-null-as-int", () -> (int) arr.get(elems, 0));
        row("volatile-null-as-int", () -> (int) o.getVolatile(h));
        row("getandset-null-as-int", () -> (int) o.getAndSet(h, (Object) 1));
        row("getandset-after", () -> { String s = String.valueOf(h.o); h.o = null; return s; });
        row("object-long-as-int", () -> (int) other.get(h));
        row("object-null-as-int-again", () -> (int) o.get(h));
    }
}

// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical):
//   object-null-as-int-first: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   object-null-as-long: java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   object-null-as-boolean: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   object-null-as-double: java.lang.NullPointerException: Cannot invoke "java.lang.Number.doubleValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   integer-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
//   integer-null-as-long: java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   number-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   static-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   array-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   volatile-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   getandset-null-as-int: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   getandset-after: 1
//   object-long-as-int: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   object-null-as-int-again: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
