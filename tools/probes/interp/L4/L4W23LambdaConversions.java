// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L4: the argument and return conversions
// a `LambdaMetafactory` proxy performs between the functional interface's
// (erased or instantiated) signature and the implementation method — boxing,
// unboxing, primitive widening, varargs method references, and the
// SerializedLambda a serializable lambda writes. CratonVM links javac's
// `metafactory` / `altMetafactory` call sites natively
// (`invokedynamic.rs::bootstrap_lambda`) and converts arguments in its own
// lambda dispatch (`interpreter/lambda.rs`), where HotSpot spins a class that
// does the conversions in bytecode.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W23LambdaConversions
//
// HotSpot 25 (25.0.3) prints:
//   unbox+widen return: 41 (Long)
//   unbox+widen arg, box return: 42 (Long)
//   int return widened to long: 4 (Long)
//   short->int->double: -3.0 (Double)
//   char widened to int param: 66 (Integer)
//   short widened to long param: 14 (Long)
//   Byte unboxed to byte return: 120 (Byte)
//   byte param boxed: -2 (Byte)
//   int boxed to Object: 5 (Integer)
//   int literal lambda as double: 3.0 (Double)
//   char boxed to Character: q (Character)
//   varargs method ref: a-b (String)
//   varargs array passthrough: x/y (String)
//   null unboxed in body: java.lang.NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because "<parameter1>" is null
//   null unboxed by method ref arg: java.lang.NullPointerException: null
//   null unboxed by method ref return: java.lang.NullPointerException: null
//   null unboxed+widened return: java.lang.NullPointerException: null
//   boxed return unboxed: 9 (Integer)
//   erased param cast in body: java.lang.ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')
//   erased bridge cast: java.lang.ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')
//   serialized: L4W23LambdaConversions L4W23LambdaConversions$Ser.apply(Ljava/lang/Object;)Ljava/lang/Object; kind=6 L4W23LambdaConversions.stat(Ljava/lang/Integer;)Ljava/lang/String; inst=(Ljava/lang/Integer;)Ljava/lang/String; captured=0
//   round trip: stat7 sameClass=false
//   captured arg: 1 cap kind=6 true
//
// Before wave 23 (read from the code, not run): the three `null unboxed by
// method ref` rows passed the null on as the primitive (argument) or returned
// it as the primitive (return) instead of throwing; HotSpot's spun class
// throws a NullPointerException whose getMessage() is null.

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.invoke.SerializedLambda;
import java.lang.reflect.Method;
import java.util.function.BiFunction;
import java.util.function.DoubleSupplier;
import java.util.function.Function;
import java.util.function.IntFunction;
import java.util.function.IntUnaryOperator;
import java.util.function.LongSupplier;
import java.util.function.Supplier;
import java.util.function.ToDoubleFunction;
import java.util.function.ToLongFunction;

public class L4W23LambdaConversions {
    interface Ser extends Function<Integer, String>, Serializable {}
    interface CharToInt { int apply(char c); }
    interface ShortToLong { long apply(short s); }
    interface ObjToByte { byte apply(Object o); }
    interface VarJoin { String join(String sep, String a, String b); }
    interface ByteBox { Object apply(byte b); }

    static String stat(Integer x) { return "stat" + x; }
    static long twice(long x) { return 2 * x; }
    static int ci(int c) { return c + 1; }
    static Byte asByte(Object o) { return (byte) o.hashCode(); }
    static Integer nullInteger(Object o) { return null; }

    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            Object r = c.run();
            out = r == null ? "null" : r + " (" + r.getClass().getSimpleName() + ")";
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        ToLongFunction<Integer> intToLong = Integer::intValue;
        row("unbox+widen return", () -> intToLong.applyAsLong(41));
        Function<Integer, Long> widenArg = L4W23LambdaConversions::twice;
        row("unbox+widen arg, box return", () -> widenArg.apply(21));
        LongSupplier len = "abcd"::length;
        row("int return widened to long", () -> len.getAsLong());
        ToDoubleFunction<Short> sd = Short::intValue;
        row("short->int->double", () -> sd.applyAsDouble((short) -3));
        CharToInt c2i = L4W23LambdaConversions::ci;
        row("char widened to int param", () -> c2i.apply('A'));
        ShortToLong s2l = L4W23LambdaConversions::twice;
        row("short widened to long param", () -> s2l.apply((short) 7));
        ObjToByte o2b = L4W23LambdaConversions::asByte;
        row("Byte unboxed to byte return", () -> o2b.apply("x"));
        ByteBox bb = Byte::valueOf;
        row("byte param boxed", () -> bb.apply((byte) -2));
        IntFunction<Object> boxInt = Integer::valueOf;
        row("int boxed to Object", () -> boxInt.apply(5));
        DoubleSupplier ds = () -> 3;
        row("int literal lambda as double", () -> ds.getAsDouble());
        Supplier<Object> so = () -> 'q';
        row("char boxed to Character", () -> so.get());
        VarJoin vj = String::join;
        row("varargs method ref", () -> vj.join("-", "a", "b"));
        BiFunction<String, Object[], String> fmt = String::format;
        row("varargs array passthrough", () -> fmt.apply("%s/%s", new Object[] {"x", "y"}));
        Function<Integer, Integer> unboxNull = i -> i + 1;
        row("null unboxed in body", () -> unboxNull.apply(null));
        Function<Integer, Integer> absRef = Math::abs;
        row("null unboxed by method ref arg", () -> absRef.apply(null));
        java.util.function.ToIntFunction<String> nullRet = L4W23LambdaConversions::nullInteger;
        row("null unboxed by method ref return", () -> nullRet.applyAsInt("z"));
        java.util.function.ToLongFunction<Object> nullWiden = L4W23LambdaConversions::nullInteger;
        row("null unboxed+widened return", () -> nullWiden.applyAsLong("z"));
        IntUnaryOperator viaBoxed = Integer::valueOf;
        row("boxed return unboxed", () -> viaBoxed.applyAsInt(9));
        Function<Object, String> cce = x -> ((String) x).trim();
        row("erased param cast in body", () -> cce.apply(5));
        @SuppressWarnings("unchecked")
        Function<Object, Object> raw = (Function<Object, Object>) (Function<?, ?>) (Function<String, Integer>) String::length;
        row("erased bridge cast", () -> raw.apply(12));

        Ser ser = L4W23LambdaConversions::stat;
        Method wr = ser.getClass().getDeclaredMethod("writeReplace");
        wr.setAccessible(true);
        SerializedLambda sl = (SerializedLambda) wr.invoke(ser);
        System.out.println("serialized: " + sl.getCapturingClass() + " " + sl.getFunctionalInterfaceClass()
                + "." + sl.getFunctionalInterfaceMethodName() + sl.getFunctionalInterfaceMethodSignature()
                + " kind=" + sl.getImplMethodKind() + " " + sl.getImplClass() + "." + sl.getImplMethodName()
                + sl.getImplMethodSignature() + " inst=" + sl.getInstantiatedMethodType()
                + " captured=" + sl.getCapturedArgCount());
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
            oos.writeObject(ser);
        }
        Object back;
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            back = ois.readObject();
        }
        @SuppressWarnings("unchecked")
        Function<Integer, String> f = (Function<Integer, String>) back;
        System.out.println("round trip: " + f.apply(7) + " sameClass=" + (back.getClass() == ser.getClass()));
        String captured = "cap";
        Ser capturing = i -> captured + i;
        Method wr2 = capturing.getClass().getDeclaredMethod("writeReplace");
        wr2.setAccessible(true);
        SerializedLambda s2 = (SerializedLambda) wr2.invoke(capturing);
        System.out.println("captured arg: " + s2.getCapturedArgCount() + " " + s2.getCapturedArg(0)
                + " kind=" + s2.getImplMethodKind() + " " + s2.getImplMethodName().startsWith("lambda$main$"));
    }
}
