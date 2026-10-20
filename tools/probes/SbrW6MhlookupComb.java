// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

/**
 * SBR wave 6 lane mhlookup: the {@code MethodHandle} carrier rows that stay
 * held -- combinators, {@code asType}/{@code asSpreader}/{@code asCollector}/
 * {@code bindTo}/{@code invokeWithArguments}, {@code find*}/{@code unreflect*},
 * {@code byteArrayViewVarHandle} -- as ONE probe, so the dial
 * ({@code CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/invoke/MethodHandles} etc.)
 * shows how each fails when the real bytecode runs instead of the carrier.
 * Same row/drift protocol as {@code SbrW6Mhlookup}.
 */
public class SbrW6MhlookupComb {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    interface X {
        Object get() throws Throwable;
    }

    static void rowx(String label, X x) {
        LABELS.add(label);
        ROWS.add(() -> {
            try {
                return String.valueOf(x.get());
            } catch (Throwable t) {
                return "!" + t.getClass().getName();
            }
        });
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName();
        }
    }

    static int inc(int x) { return x + 1; }
    static int add(int a, int b) { return a + b; }
    static String cat(String a, String b) { return a + b; }
    static boolean pos(int x) { return x > 0; }
    static int neg(int x) { return -x; }
    static int thrower(int x) { throw new IllegalStateException("t"); }
    static int handler(IllegalStateException e, int x) { return -1; }
    public String hello(String s) { return "hi " + s; }
    public int f = 5;

    public static void main(String[] args) throws Exception {
        final Lookup L = MethodHandles.lookup();
        final MethodType ii = MethodType.methodType(int.class, int.class);
        final MethodHandle INC = L.findStatic(SbrW6MhlookupComb.class, "inc", ii);
        final MethodHandle ADD = L.findStatic(SbrW6MhlookupComb.class, "add", MethodType.methodType(int.class, int.class, int.class));
        final MethodHandle CAT = L.findStatic(SbrW6MhlookupComb.class, "cat", MethodType.methodType(String.class, String.class, String.class));
        final MethodHandle POS = L.findStatic(SbrW6MhlookupComb.class, "pos", MethodType.methodType(boolean.class, int.class));
        final MethodHandle NEG = L.findStatic(SbrW6MhlookupComb.class, "neg", ii);
        rowx("identity", () -> MethodHandles.identity(int.class).invoke(7));
        rowx("constant", () -> MethodHandles.constant(String.class, "k").invoke());
        rowx("insertArguments", () -> MethodHandles.insertArguments(ADD, 0, 5).invoke(3));
        rowx("dropArguments", () -> MethodHandles.dropArguments(INC, 0, String.class).invoke("x", 4));
        rowx("filterArguments", () -> MethodHandles.filterArguments(ADD, 0, NEG).invoke(3, 10));
        rowx("filterReturnValue", () -> MethodHandles.filterReturnValue(INC, NEG).invoke(3));
        rowx("foldArguments", () -> MethodHandles.foldArguments(ADD, INC).invoke(3));
        rowx("collectArguments", () -> MethodHandles.collectArguments(ADD, 0, NEG).invoke(3, 4));
        rowx("permuteArguments", () -> MethodHandles.permuteArguments(CAT,
                MethodType.methodType(String.class, String.class, String.class), 1, 0).invoke("a", "b"));
        rowx("guardWithTest", () -> MethodHandles.guardWithTest(POS, INC, NEG).invoke(5) + " "
                + MethodHandles.guardWithTest(POS, INC, NEG).invoke(-5));
        rowx("catchException", () -> MethodHandles.catchException(
                L.findStatic(SbrW6MhlookupComb.class, "thrower", ii), IllegalStateException.class,
                L.findStatic(SbrW6MhlookupComb.class, "handler",
                        MethodType.methodType(int.class, IllegalStateException.class, int.class))).invoke(1));
        rowx("arrayElementGetter", () -> MethodHandles.arrayElementGetter(int[].class).invoke(new int[] {4, 5}, 1));
        rowx("byteArrayViewVarHandle", () -> {
            VarHandle vh = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
            byte[] b = new byte[8];
            vh.set(b, 0, 0x01020304);
            return b[0] + " " + b[3] + " " + (int) vh.get(b, 0);
        });
        rowx("asType", () -> INC.asType(MethodType.methodType(Object.class, Integer.class)).invoke((Object) 4));
        rowx("asSpreader", () -> ADD.asSpreader(int[].class, 2).invoke(new int[] {3, 9}));
        rowx("asCollector", () -> L.findStatic(java.util.Arrays.class, "toString",
                MethodType.methodType(String.class, Object[].class)).asCollector(Object[].class, 2).invoke("a", "b"));
        rowx("asVarargsCollector", () -> L.findStatic(java.util.Arrays.class, "toString",
                MethodType.methodType(String.class, Object[].class)).asVarargsCollector(Object[].class).invoke("a", "b", "c"));
        rowx("bindTo", () -> L.findVirtual(SbrW6MhlookupComb.class, "hello", MethodType.methodType(String.class, String.class))
                .bindTo(new SbrW6MhlookupComb()).invoke("z"));
        rowx("invokeWithArguments", () -> ADD.invokeWithArguments(3, 4));
        rowx("invokeExact", () -> (int) INC.invokeExact(3));
        rowx("findConstructor", () -> ((SbrW6MhlookupComb) L.findConstructor(SbrW6MhlookupComb.class,
                MethodType.methodType(void.class)).invoke()).f);
        rowx("findGetter", () -> L.findGetter(SbrW6MhlookupComb.class, "f", int.class).invoke(new SbrW6MhlookupComb()));
        rowx("findVarHandle", () -> L.findVarHandle(SbrW6MhlookupComb.class, "f", int.class).get(new SbrW6MhlookupComb()));
        rowx("unreflect", () -> L.unreflect(SbrW6MhlookupComb.class.getMethod("hello", String.class))
                .invoke(new SbrW6MhlookupComb(), "u"));
        rowx("lambda", () -> ((java.util.function.IntUnaryOperator) x -> x + 1).applyAsInt(1));
        rowx("VarHandle.varType", () -> L.findVarHandle(SbrW6MhlookupComb.class, "f", int.class).varType());
        rowx("VarHandle.coordinateTypes", () -> L.findVarHandle(SbrW6MhlookupComb.class, "f", int.class).coordinateTypes());

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
