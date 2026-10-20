// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.lang.reflect.RecordComponent;
import java.util.List;

/**
 * SBR2 wave 7, lane classrefl: ns/op of the reflection rows. Usage:
 * {@code cratonvm --jdk-only -cp . Sbr2W7ClassreflBench [rounds] [iters]}. Run interleaved
 * against the control binary and take medians.
 */
public class Sbr2W7ClassreflBench {
    @Retention(RetentionPolicy.RUNTIME)
    @interface Tag { String value(); int n() default 3; }

    @Tag("c")
    public static class T {
        public T() {}
        @Tag("m") public int m(int a) { return a; }
    }

    public interface Iface { default int dflt(int a) { return a + 1; } int plain(int a); }
    public record Rec(List<String> names, int i) {}

    static volatile Object sink;

    interface Op { Object run() throws Exception; }

    static long time(Op op, int iters) throws Exception {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) sink = op.run();
        return (System.nanoTime() - t0) / iters;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        final Class<?> t = T.class;
        final Constructor<?> ctor = t.getConstructor();
        final T tobj = new T();
        final Method mm = t.getMethod("m", int.class);
        final InvocationHandler h = (p, m, a) -> m.isDefault() ? InvocationHandler.invokeDefault(p, m, a) : (Object) 7;
        final ClassLoader cl = Sbr2W7ClassreflBench.class.getClassLoader();
        final Iface pi = (Iface) Proxy.newProxyInstance(cl, new Class<?>[] { Iface.class }, h);
        final Class<?> arr = String[][].class;
        final Class<?> rec = Rec.class;
        final String[] names = { "newInstance", "ctor.newInstance", "getMethod+invoke", "getConstructor", "newProxyInstance", "proxy plain call",
            "proxy default call", "isProxyClass", "getAnnotation+value", "annotation on method", "getTypeName array", "getTypeName plain",
            "recordcomp getGenericType", "getRecordComponents" };
        final Op[] ops = {
            () -> t.newInstance(),
            () -> ctor.newInstance(),
            () -> t.getMethod("m", int.class).invoke(tobj, 3),
            () -> t.getConstructor(),
            () -> Proxy.newProxyInstance(cl, new Class<?>[] { Iface.class }, h),
            () -> pi.plain(1),
            () -> pi.dflt(1),
            () -> Proxy.isProxyClass(pi.getClass()),
            () -> t.getAnnotation(Tag.class).value(),
            () -> mm.getAnnotation(Tag.class).n(),
            () -> arr.getTypeName(),
            () -> t.getTypeName(),
            () -> rec.getRecordComponents()[0].getGenericType(),
            () -> rec.getRecordComponents(),
        };
        for (int i = 0; i < ops.length; i++) time(ops[i], iters / 2);
        long[][] res = new long[ops.length][rounds];
        for (int r = 0; r < rounds; r++) for (int i = 0; i < ops.length; i++) res[i][r] = time(ops[i], iters);
        for (int i = 0; i < ops.length; i++) {
            StringBuilder sb = new StringBuilder(String.format("%-28s", names[i]));
            long[] s = res[i].clone();
            java.util.Arrays.sort(s);
            for (long v : res[i]) sb.append(String.format("%8d", v));
            sb.append("   median ").append(s[s.length / 2]);
            System.out.println(sb);
        }
    }
}
