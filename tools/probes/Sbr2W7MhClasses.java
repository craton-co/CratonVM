// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.util.function.IntUnaryOperator;

/**
 * SBR round 2 wave 7 lane MH: POSITIVE CONTROL for the per-row trial. Prints the
 * runtime CLASS of what each candidate row returns, so a run where the real JDK
 * bytecode executed (a {@code BoundMethodHandle$Species_*}, an
 * {@code IntrinsicMethodHandle}, a spun hidden lambda class) reads differently
 * from a run where the VM's native carrier answered (plain
 * {@code java.lang.invoke.MethodHandle}). Not diffed against HotSpot: the point
 * is which arm produced the object.
 */
public class Sbr2W7MhClasses {
    static int inc(int x) { return x + 1; }
    static int add(int a, int b) { return a + b; }

    interface X { Object get() throws Throwable; }
    static void row(String name, X c) {
        try {
            Object o = c.get();
            System.out.println("cls " + name + " = " + (o == null ? "null" : o.getClass().getName().replaceAll("/0x[0-9a-f]+|/[0-9a-f]+$", "/N")));
        } catch (Throwable t) {
            System.out.println("cls " + name + " = !" + t.getClass().getName());
        }
    }

    public static void main(String[] args) throws Throwable {
        Lookup L = MethodHandles.lookup();
        Class<?> C = Sbr2W7MhClasses.class;
        MethodType ii = MethodType.methodType(int.class, int.class);
        MethodHandle INC = L.findStatic(C, "inc", ii);
        MethodHandle ADD = L.findStatic(C, "add", MethodType.methodType(int.class, int.class, int.class));
        row("findStatic", () -> INC);
        row("foldArguments", () -> MethodHandles.foldArguments(ADD, INC));
        row("arrayElementGetter.int", () -> MethodHandles.arrayElementGetter(int[].class));
        row("arrayElementGetter.obj", () -> MethodHandles.arrayElementGetter(String[].class));
        row("arrayElementSetter.int", () -> MethodHandles.arrayElementSetter(int[].class));
        row("insertArguments", () -> MethodHandles.insertArguments(ADD, 0, 1));
        row("metafactory.callsite", () -> java.lang.invoke.LambdaMetafactory.metafactory(L, "applyAsInt",
                MethodType.methodType(IntUnaryOperator.class), ii, INC, ii));
        row("metafactory.lambda", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.metafactory(L, "applyAsInt",
                    MethodType.methodType(IntUnaryOperator.class), ii, INC, ii).getTarget();
            return mh.invoke();
        });
        row("alt.lambda", () -> {
            MethodHandle mh = java.lang.invoke.LambdaMetafactory.altMetafactory(L, "applyAsInt",
                    MethodType.methodType(IntUnaryOperator.class), ii, INC, ii, 0).getTarget();
            return mh.invoke();
        });
        row("indy.lambda", () -> (IntUnaryOperator) x -> x + 1);
        row("indy.methodref", () -> (IntUnaryOperator) Sbr2W7MhClasses::inc);
        row("VarHandle.field", () -> L.findVarHandle(C, "f", int.class));
        row("VarHandle.array", () -> MethodHandles.arrayElementVarHandle(int[].class));
        row("VarHandle.byteview", () -> MethodHandles.byteArrayViewVarHandle(int[].class, java.nio.ByteOrder.BIG_ENDIAN));
        row("defineHiddenClass", () -> L.defineHiddenClass(C.getResourceAsStream("Sbr2W7MhClasses$H.class").readAllBytes(), true).lookupClass());
        row("defineClass", () -> {
            byte[] b = C.getResourceAsStream("Sbr2W7MhClasses$H2.class").readAllBytes();
            return L.defineClass(b);
        });
        System.out.println("cls-done");
    }

    public int f;
    public static class H { }
    public static class H2 { }

    static byte[] read(Class<?> c) throws Exception {
        try (java.io.InputStream in = c.getResourceAsStream(c.getSimpleName() + ".class")) {
            return in.readAllBytes();
        }
    }
}
