// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 25, lane L4: `asType`'s reference -> primitive
// conversion on the two doors CratonVM answers with its own natives -- a
// primitive `MethodHandles.arrayElementSetter` (`native-builtins/src/lang_invoke.rs`,
// `MH_KIND_ARRAY_SET`) and a direct handle with a primitive parameter reached
// through `invoke` / `invokeWithArguments` (`invoke_reference_cast_refusal`,
// `adapt_single_arg`). Page:
// docs/internal/fixed-bugs/interpreter-L4-primitive-array-setter-handle-stores-an-unconvertible-value-FIXED-20260927.md
//
// HotSpot converts a reference argument to a primitive parameter with
// `ValueConversions.unbox<W>(Object, false)`: a wrapper whose primitive widens
// is converted, any other wrapper or a non-wrapper `Number` is
// `Cannot cast <W> to <target wrapper>`, anything else fails the `(Number)`
// cast, and a null is an NPE from `primitiveConversion` -- unless the call
// site typed the value as the target's own wrapper (`x.intValue()` on null).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W25PrimitiveArraySetter
//
// HotSpot 25 (25.0.3) prints the 88 matrix rows (8 arrays x 11 values) then
// the tail; the matrix rows follow one rule per value class:
//   <T>[] <- <W>:   the value (widened) when W's primitive widens to T,
//                   else java.lang.ClassCastException: Cannot cast java.lang.<W> to java.lang.<T's wrapper>
//   <T>[] <- String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   <T>[] <- Foo:   java.lang.ClassCastException: class L4W25PrimitiveArraySetter$Foo cannot be cast to class java.lang.Number (L4W25PrimitiveArraySetter$Foo is in unnamed module of loader 'app'; java.lang.Number is in module java.base of loader 'bootstrap')
//   <T>[] <- null:  java.lang.NullPointerException: Cannot invoke "java.lang.Number.<g>()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//                   with <g> = intValue for boolean/char/int, byteValue, shortValue, longValue, floatValue, doubleValue
// e.g. (verbatim):
//   boolean[] <- Boolean: true
//   boolean[] <- Byte: java.lang.ClassCastException: Cannot cast java.lang.Byte to java.lang.Boolean
//   char[] <- Character: A
//   char[] <- Integer: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Character
//   short[] <- Byte: 1
//   int[] <- Character: 65
//   int[] <- Long: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   long[] <- Integer: 4
//   float[] <- Long: 5.0
//   float[] <- Double: java.lang.ClassCastException: Cannot cast java.lang.Double to java.lang.Float
//   double[] <- Float: 6.5
// and the tail, verbatim:
//   int[] withArgs String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   int[] withArgs Short: 9
//   int[] bound Long: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   int[] Integer-typed null: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
//   int[] bound Integer-typed null: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
//   long[] Integer-typed null: java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   int[] BigInteger: java.lang.ClassCastException: Cannot cast java.math.BigInteger to java.lang.Integer
//   int[] int[]: java.lang.ClassCastException: class [I cannot be cast to class java.lang.Number ([I and java.lang.Number are in module java.base of loader 'bootstrap')
//   long[] raw int: 4
//   double[] raw char: 99.0
//   getter String index: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   getter null index: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   getter Long index: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   getter Short index: 30
//   getter Integer-typed null index: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
//   setter String value, index out of range: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   findStatic(int) String: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   findStatic(int) Long: java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
//   findStatic(int) Character: 66
//   findStatic(int) null: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   findStatic(int) Integer-typed null: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "x" is null
//   findStatic(long) Integer: 7
//   findStatic(double) Float: 1.5
//   findStatic(long) withArgs Integer: 8
//   findStatic(int) withArgs Boolean: java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Integer
//
// Before wave 25 (read from the code, not run): every refusal row of the
// matrix stored something (a truncated wrapper, a reinterpreted reference, a
// zero for a null) and printed it; a widening wrapper into long/float/double
// stored 0 (`write_prim_element` takes only its own `Value` variant); the
// findStatic refusal rows ran `takeInt` on the reference; `findStatic(long)
// Integer` handed the callee an int in a long slot; a String / Long / null
// index read element 0 (or the truncated long), and `setter String value,
// index out of range` was the index's ArrayIndexOutOfBoundsException.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.math.BigInteger;

public class L4W25PrimitiveArraySetter {
    static class Foo {}
    static int got;
    static long gotLong;
    static double gotDouble;
    static void takeInt(int v) { got = v; }
    static void takeLong(long v) { gotLong = v; }
    static void takeDouble(double v) { gotDouble = v; }

    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            out = String.valueOf(c.run());
        } catch (Throwable t) {
            out = t.toString();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        Class<?>[] arrays = {boolean[].class, byte[].class, char[].class, short[].class,
            int[].class, long[].class, float[].class, double[].class};
        Object[] values = {Boolean.TRUE, Byte.valueOf((byte) 1), Character.valueOf('A'), Short.valueOf((short) 3),
            Integer.valueOf(4), Long.valueOf(5L), Float.valueOf(6.5f), Double.valueOf(7.5), "s", new Foo(), null};
        for (Class<?> at : arrays) {
            MethodHandle set = MethodHandles.arrayElementSetter(at);
            for (Object v : values) {
                String vn = v == null ? "null" : v.getClass().getSimpleName();
                row(at.getSimpleName() + " <- " + vn, () -> {
                    Object arr = Array.newInstance(at.getComponentType(), 1);
                    set.invoke(arr, 0, v);
                    return Array.get(arr, 0);
                });
            }
        }
        MethodHandle si = MethodHandles.arrayElementSetter(int[].class);
        MethodHandle sl = MethodHandles.arrayElementSetter(long[].class);
        MethodHandle sd = MethodHandles.arrayElementSetter(double[].class);
        row("int[] withArgs String", () -> {
            int[] a = new int[1];
            si.invokeWithArguments(a, 0, "s");
            return a[0];
        });
        row("int[] withArgs Short", () -> {
            int[] a = new int[1];
            si.invokeWithArguments(a, 0, (short) 9);
            return a[0];
        });
        row("int[] bound Long", () -> {
            int[] a = new int[1];
            si.bindTo(a).invoke(0, (Object) Long.valueOf(2));
            return a[0];
        });
        row("int[] Integer-typed null", () -> {
            si.invoke(new int[1], 0, (Integer) null);
            return "stored";
        });
        row("int[] bound Integer-typed null", () -> {
            si.bindTo(new int[1]).invoke(0, (Integer) null);
            return "stored";
        });
        row("long[] Integer-typed null", () -> {
            sl.invoke(new long[1], 0, (Integer) null);
            return "stored";
        });
        row("int[] BigInteger", () -> {
            si.invoke(new int[1], 0, (Object) BigInteger.ONE);
            return "stored";
        });
        row("int[] int[]", () -> {
            si.invoke(new int[1], 0, (Object) new int[0]);
            return "stored";
        });
        row("long[] raw int", () -> {
            long[] a = new long[1];
            sl.invoke(a, 0, 4);
            return a[0];
        });
        row("double[] raw char", () -> {
            double[] a = new double[1];
            sd.invoke(a, 0, 'c');
            return a[0];
        });
        MethodHandle gi = MethodHandles.arrayElementGetter(int[].class);
        int[] three = {10, 20, 30};
        row("getter String index", () -> gi.invoke(three, (Object) "x"));
        row("getter null index", () -> gi.invoke(three, (Object) null));
        row("getter Long index", () -> gi.invoke(three, (Object) Long.valueOf(1)));
        row("getter Short index", () -> gi.invoke(three, (Object) Short.valueOf((short) 2)));
        row("getter Integer-typed null index", () -> gi.invoke(three, (Integer) null));
        row("setter String value, index out of range", () -> {
            si.invoke(new int[1], 5, (Object) "s");
            return "stored";
        });
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W25PrimitiveArraySetter.class;
        MethodHandle ti = l.findStatic(me, "takeInt", MethodType.methodType(void.class, int.class));
        MethodHandle tl = l.findStatic(me, "takeLong", MethodType.methodType(void.class, long.class));
        MethodHandle td = l.findStatic(me, "takeDouble", MethodType.methodType(void.class, double.class));
        row("findStatic(int) String", () -> {
            ti.invoke((Object) "s");
            return got;
        });
        row("findStatic(int) Long", () -> {
            ti.invoke((Object) Long.valueOf(3));
            return got;
        });
        row("findStatic(int) Character", () -> {
            ti.invoke((Object) Character.valueOf('B'));
            return got;
        });
        row("findStatic(int) null", () -> {
            ti.invoke((Object) null);
            return got;
        });
        row("findStatic(int) Integer-typed null", () -> {
            ti.invoke((Integer) null);
            return got;
        });
        row("findStatic(long) Integer", () -> {
            tl.invoke((Object) Integer.valueOf(7));
            return gotLong;
        });
        row("findStatic(double) Float", () -> {
            td.invoke((Object) Float.valueOf(1.5f));
            return gotDouble;
        });
        row("findStatic(long) withArgs Integer", () -> {
            tl.invokeWithArguments(8);
            return gotLong;
        });
        row("findStatic(int) withArgs Boolean", () -> {
            ti.invokeWithArguments(true);
            return got;
        });
    }
}
