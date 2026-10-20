// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * SBR wave 5, lane {@code reflacc}: measurement 7 micro for {@code Field}'s
 * value accessors and the layers beneath them. Besides the reflective rows
 * ({@code get}, {@code set}, {@code getInt}, {@code setInt}, {@code getLong},
 * {@code Method.invoke}, {@code Constructor.newInstance}) it times the
 * {@code MethodHandle} getter the JDK's field accessor wraps, exact and
 * erased to {@code (Object)int} the way {@code MethodHandleAccessorFactory}
 * erases it, so a regression can be placed at a layer. Prints ns/op per row
 * per round; run the arms INTERLEAVED, JIT and {@code --nojit}.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW5ReflAccBench [rounds] [iters] [rows]
 * </pre>
 *
 * {@code rows} is a comma list of row names to run (default: all).
 */
public class SbrW5ReflAccBench {
    public static class H {
        public int i = 1;
        public long l = 2;
        public Object o = "x";
        public static int si = 3;

        public H() {}

        public int add(int a) {
            return a + i;
        }
    }

    static long sink;

    interface Row {
        void run(int n) throws Throwable;
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        String only = args.length > 2 ? "," + args[2] + "," : null;
        H h = new H();
        Field fi = H.class.getField("i");
        Field fl = H.class.getField("l");
        Field fo = H.class.getField("o");
        Field fsi = H.class.getField("si");
        Field fia = H.class.getField("i");
        fia.setAccessible(true);
        Method add = H.class.getMethod("add", int.class);
        Constructor<H> ctor = H.class.getConstructor();
        MethodHandle mhI = MethodHandles.lookup().unreflectGetter(fi);
        MethodHandle mhIo = mhI.asType(MethodType.methodType(int.class, Object.class));
        MethodHandle mhSetI = MethodHandles.lookup().unreflectSetter(fi);
        Object[] one = new Object[] {1};
        Object ho = h;
        String[] names = {
            "getInt", "setInt", "getLong", "get", "set", "getIntAcc", "getStatic",
            "mhExact", "mhErased", "mhSet", "invoke", "newInst"
        };
        Row[] rows = {
            k -> { for (int j = 0; j < k; j++) sink += fi.getInt(h); },
            k -> { for (int j = 0; j < k; j++) fi.setInt(h, j); },
            k -> { for (int j = 0; j < k; j++) sink += fl.getLong(h); },
            k -> { for (int j = 0; j < k; j++) sink += fo.get(h).hashCode(); },
            k -> { for (int j = 0; j < k; j++) fo.set(h, "y"); },
            k -> { for (int j = 0; j < k; j++) sink += fia.getInt(h); },
            k -> { for (int j = 0; j < k; j++) sink += fsi.getInt(null); },
            k -> { for (int j = 0; j < k; j++) sink += (int) mhI.invokeExact(h); },
            k -> { for (int j = 0; j < k; j++) sink += (int) mhIo.invokeExact(ho); },
            k -> { for (int j = 0; j < k; j++) mhSetI.invokeExact(h, j); },
            k -> { for (int j = 0; j < k; j++) sink += (Integer) add.invoke(h, one); },
            k -> { for (int j = 0; j < k / 4; j++) sink += ctor.newInstance().i; },
        };
        for (int r = 0; r < rounds; r++) {
            StringBuilder out = new StringBuilder("round " + r);
            for (int x = 0; x < rows.length; x++) {
                if (only != null && !only.contains("," + names[x] + ",")) continue;
                int k = names[x].equals("newInst") ? n / 4 : n;
                long t = System.nanoTime();
                rows[x].run(n);
                out.append(' ').append(names[x]).append('=').append((System.nanoTime() - t) / k);
            }
            System.out.println(out);
        }
        System.out.println("sink " + (sink != 0));
    }
}
