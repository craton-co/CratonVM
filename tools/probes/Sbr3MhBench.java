// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.function.IntUnaryOperator;
import java.util.function.Supplier;

/**
 * SBR round 2 wave 7 lane MH: microbenchmarks over the MethodHandle carrier
 * surface, us/op, one line per row ("bench <name> <us/op>"), for interleaved
 * A/B runs (medians of >= 5 reps). Args: [reps] [filter-substring].
 */
public class Sbr3MhBench {
    static int sink;
    public static class Hid { }
    static final byte[] HIDDEN_BYTES = load();
    static byte[] load() { try (java.io.InputStream in = Sbr3MhBench.class.getResourceAsStream("Sbr3MhBench$Hid.class")) { return in.readAllBytes(); } catch (Exception e) { throw new RuntimeException(e); } }
    public int f;
    static int sf;

    static int inc(int x) { return x + 1; }
    static int add(int a, int b) { return a + b; }
    static int neg(int x) { return -x; }
    static boolean pos(int x) { return x > 0; }

    interface Body { long run(int n) throws Throwable; }

    static void bench(String filter, String name, int n, Body b) throws Throwable {
        if (filter != null && !name.contains(filter)) return;
        b.run(Math.max(1, n / 10)); // warm
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        sink += (int) s;
        System.out.printf(java.util.Locale.ROOT, "bench %s %.4f%n", name, (t1 - t0) / 1000.0 / n);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        String filter = args.length > 1 ? args[1] : null;
        Lookup L = MethodHandles.lookup();
        Class<?> C = Sbr3MhBench.class;
        MethodType ii = MethodType.methodType(int.class, int.class);
        MethodHandle INC = L.findStatic(C, "inc", ii);
        MethodHandle ADD = L.findStatic(C, "add", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle NEG = L.findStatic(C, "neg", ii);
        MethodHandle CONCAT = L.findVirtual(String.class, "concat", MethodType.methodType(String.class, String.class));
        MethodHandle POS = L.findStatic(C, "pos", MethodType.methodType(boolean.class, int.class));
        VarHandle IF = L.findVarHandle(C, "f", int.class);
        VarHandle SF = L.findStaticVarHandle(C, "sf", int.class);
        VarHandle AV = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle BV = MethodHandles.byteArrayViewVarHandle(int[].class, java.nio.ByteOrder.BIG_ENDIAN);
        Sbr3MhBench o = new Sbr3MhBench();
        int[] arr = new int[16];
        byte[] bytes = new byte[16];
        MethodHandle guard = MethodHandles.guardWithTest(POS, INC, NEG);
        MethodHandle ins = MethodHandles.insertArguments(ADD, 0, 5);
        MethodHandle filt = MethodHandles.filterReturnValue(INC, NEG);
        for (int rep = 0; rep < reps; rep++) {
            // ---- invocation of an existing handle
            bench(filter, "invokeExact.static", 400000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) INC.invokeExact(i); return s; });
            bench(filter, "invoke.static", 400000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) INC.invoke(i); return s; });
            bench(filter, "invoke.guard", 200000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) guard.invoke(i - 5); return s; });
            bench(filter, "invoke.insert", 200000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) ins.invoke(i); return s; });
            bench(filter, "invoke.filterRet", 200000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) filt.invoke(i); return s; });
            bench(filter, "invokeWithArguments", 50000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) ADD.invokeWithArguments(i, 1); return s; });
            // ---- construction of handles
            bench(filter, "find.static", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += L.findStatic(C, "inc", ii).type().parameterCount(); return s; });
            bench(filter, "find.virtual", 20000, n -> { long s = 0; for (int i = 0; i < n; i++)
                s += L.findVirtual(String.class, "length", MethodType.methodType(int.class)).type().parameterCount(); return s; });
            bench(filter, "find.varhandle", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += L.findVarHandle(C, "f", int.class).varType() == int.class ? 1 : 0; return s; });
            bench(filter, "combinator.insert", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += MethodHandles.insertArguments(ADD, 0, i).type().parameterCount(); return s; });
            bench(filter, "combinator.guard", 10000, n -> { long s = 0; for (int i = 0; i < n; i++) s += MethodHandles.guardWithTest(POS, INC, NEG).type().parameterCount(); return s; });
            bench(filter, "combinator.asType", 20000, n -> { long s = 0; for (int i = 0; i < n; i++)
                s += INC.asType(MethodType.methodType(Object.class, Integer.class)).type().parameterCount(); return s; });
            bench(filter, "combinator.identity", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += MethodHandles.identity(int.class).type().parameterCount(); return s; });
            bench(filter, "combinator.constant", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += MethodHandles.constant(int.class, i).type().parameterCount(); return s; });
            bench(filter, "combinator.arrayGetter", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += MethodHandles.arrayElementGetter(int[].class).type().parameterCount(); return s; });
            bench(filter, "combinator.fold", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += MethodHandles.foldArguments(ADD, INC).type().parameterCount(); return s; });
            bench(filter, "define.hidden", 300, n -> { long s = 0; byte[] hb = HIDDEN_BYTES; for (int i = 0; i < n; i++) s += L.defineHiddenClass(hb, true).lookupClass().isHidden() ? 1 : 0; return s; });
            bench(filter, "mh.toString", 100000, n -> { long s = 0; for (int i = 0; i < n; i++) s += INC.toString().length(); return s; });
            bench(filter, "combinator.empty", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += MethodHandles.empty(ii).type().parameterCount(); return s; });
            bench(filter, "combinator.countedLoop", 5000, n -> { long s = 0; MethodHandle it = MethodHandles.constant(int.class, 3); MethodHandle in = MethodHandles.constant(int.class, 0); for (int i = 0; i < n; i++) s += MethodHandles.countedLoop(it, in, ADD).type().parameterCount(); return s; });
            bench(filter, "bindTo", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += CONCAT.bindTo("x").type().parameterCount(); return s; });
            bench(filter, "asSpreader", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += ADD.asSpreader(int[].class, 2).type().parameterCount(); return s; });
            // ---- VarHandle
            bench(filter, "vh.instance.get", 1000000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) IF.get(o); return s; });
            bench(filter, "vh.instance.set", 1000000, n -> { for (int i = 0; i < n; i++) IF.set(o, i); return o.f; });
            bench(filter, "vh.instance.cas", 1000000, n -> { long s = 0; for (int i = 0; i < n; i++) { int c = (int) IF.getVolatile(o); s += IF.compareAndSet(o, c, c + 1) ? 1 : 0; } return s; });
            bench(filter, "vh.instance.getAndAdd", 1000000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) IF.getAndAdd(o, 1); return s; });
            bench(filter, "vh.static.get", 1000000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) SF.get(); return s; });
            bench(filter, "vh.array.get", 1000000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) AV.get(arr, i & 15); return s; });
            bench(filter, "vh.array.set", 1000000, n -> { for (int i = 0; i < n; i++) AV.set(arr, i & 15, i); return arr[3]; });
            bench(filter, "vh.byteview.get", 1000000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) BV.get(bytes, i & 7); return s; });
            bench(filter, "vh.varType", 100000, n -> { long s = 0; for (int i = 0; i < n; i++) s += IF.varType() == int.class ? 1 : 0; return s; });
            bench(filter, "vh.coordinateTypes", 100000, n -> { long s = 0; for (int i = 0; i < n; i++) s += IF.coordinateTypes().size(); return s; });
            // ---- lambdas: creation (one site, captured) and first call
            bench(filter, "lambda.capture.create", 300000, n -> { long s = 0; for (int i = 0; i < n; i++) { final int k = i; IntUnaryOperator op = x -> x + k; s += op.applyAsInt(1); } return s; });
            bench(filter, "lambda.nocapture", 300000, n -> { long s = 0; for (int i = 0; i < n; i++) { IntUnaryOperator op = x -> x + 1; s += op.applyAsInt(i); } return s; });
            bench(filter, "lambda.methodref", 300000, n -> { long s = 0; for (int i = 0; i < n; i++) { IntUnaryOperator op = Sbr3MhBench::inc; s += op.applyAsInt(i); } return s; });
            bench(filter, "lambda.supplier", 300000, n -> { long s = 0; for (int i = 0; i < n; i++) { Supplier<String> sp = "abc"::toString; s += sp.get().length(); } return s; });
            bench(filter, "lambda.serializable", 100000, n -> { long s = 0; for (int i = 0; i < n; i++) { Runnable r = (Runnable & java.io.Serializable) () -> sink++; r.run(); s++; } return s; });
            bench(filter, "lambda.metafactory.direct", 2000, n -> { long s = 0; for (int i = 0; i < n; i++) {
                MethodHandle mh = java.lang.invoke.LambdaMetafactory.metafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class),
                        ii, INC, ii).getTarget();
                s += ((IntUnaryOperator) mh.invoke()).applyAsInt(i); } return s; });
            bench(filter, "lambda.alt.direct", 2000, n -> { long s = 0; for (int i = 0; i < n; i++) {
                MethodHandle mh = java.lang.invoke.LambdaMetafactory.altMetafactory(L, "applyAsInt", MethodType.methodType(IntUnaryOperator.class),
                        ii, INC, ii, java.lang.invoke.LambdaMetafactory.FLAG_SERIALIZABLE).getTarget();
                s += ((IntUnaryOperator) mh.invoke()).applyAsInt(i); } return s; });
            bench(filter, "stringconcat", 300000, n -> { long s = 0; for (int i = 0; i < n; i++) s += ("a" + i + "b").length(); return s; });
        }
        System.out.println("sink " + sink);
    }
}
