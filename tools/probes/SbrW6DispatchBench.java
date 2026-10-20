// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import javax.crypto.Cipher;
import javax.crypto.Mac;

/**
 * SBR wave 6, lane {@code dispatch}: the micro for the four shared costs the
 * lane targets, one row each so a regression can be placed at a layer.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW6DispatchBench [rounds] [iters] [rows]
 * </pre>
 *
 * Rows: {@code mhStatic}/{@code mhInst}/{@code mhSet}/{@code mhInvoke} (MethodHandle exact and
 * generic invocation), {@code mInvoke} (public {@code Method.invoke}), {@code mInvokeAcc}
 * ({@code setAccessible(true)}), {@code newPub}/{@code newPriv} ({@code Constructor.newInstance},
 * public and package-private constructor), {@code tlGet}/{@code tlSet}, {@code cipher}
 * ({@code Cipher.getInstance}), {@code mac}, {@code fieldGet}, {@code swCaller}
 * ({@code StackWalker.getCallerClass}), {@code swWalk}, and the {@code ...D20}/{@code ...D60} rows:
 * the same call under a recursion 20/60 frames deep (what a bounded caller walk saves).
 * Prints ns/op per row per round.
 */
public class SbrW6DispatchBench {
    public static class H {
        public int i = 1;
        public Object o = "x";
        public H() {}
        H(int x) { i = x; }
        public int add(int a) { return a + i; }
        public static int twice(int a) { return a * 2; }
    }

    static long sink;
    static final ThreadLocal<Object> TL = new ThreadLocal<>();

    interface Row {
        void run(int n) throws Throwable;
    }

    /** Runs {@code r} at the bottom of a recursion {@code d} frames deep (a deep stack under the call). */
    static void deep(int d, Row r, int k) throws Throwable {
        if (d == 0) {
            r.run(k);
            return;
        }
        deep(d - 1, r, k);
        sink += d & 1;
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 100_000;
        String only = args.length > 2 ? "," + args[2] + "," : null;
        H h = new H();
        Field fi = H.class.getField("i");
        Method add = H.class.getMethod("add", int.class);
        Method addAcc = H.class.getMethod("add", int.class);
        addAcc.setAccessible(true);
        Constructor<H> ctorPub = H.class.getConstructor();
        Constructor<H> ctorPriv = H.class.getDeclaredConstructor(int.class);
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle mhTwice = lk.findStatic(H.class, "twice", MethodType.methodType(int.class, int.class));
        MethodHandle mhI = lk.unreflectGetter(fi);
        MethodHandle mhSetI = lk.unreflectSetter(fi);
        Object[] one = new Object[] {1};
        StackWalker sw = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
        TL.set("v");
        Object tlv = "w";
        String[] names = {
            "mhStatic", "mhInst", "mhSet", "mhInvoke", "mInvoke", "mInvokeAcc", "newPub", "newPriv",
            "tlGet", "tlSet", "cipher", "mac", "fieldGet", "swCaller", "swWalk",
            "fieldGetD20", "fieldGetD60", "newPrivD60", "mInvokeD60"
        };
        Row[] rows = {
            k -> { for (int j = 0; j < k; j++) sink += (int) mhTwice.invokeExact(j); },
            k -> { for (int j = 0; j < k; j++) sink += (int) mhI.invokeExact(h); },
            k -> { for (int j = 0; j < k; j++) mhSetI.invokeExact(h, j); },
            k -> { for (int j = 0; j < k; j++) sink += (Integer) mhTwice.invoke(j); },
            k -> { for (int j = 0; j < k; j++) sink += (Integer) add.invoke(h, one); },
            k -> { for (int j = 0; j < k; j++) sink += (Integer) addAcc.invoke(h, one); },
            k -> { for (int j = 0; j < k; j++) sink += ctorPub.newInstance().i; },
            k -> { for (int j = 0; j < k; j++) sink += ctorPriv.newInstance(j).i; },
            k -> { for (int j = 0; j < k; j++) sink += TL.get().hashCode(); },
            k -> { for (int j = 0; j < k; j++) TL.set(tlv); },
            k -> { for (int j = 0; j < k; j++) sink += Cipher.getInstance("AES/CBC/PKCS5Padding").hashCode() & 1; },
            k -> { for (int j = 0; j < k; j++) sink += Mac.getInstance("HmacSHA256").hashCode() & 1; },
            k -> { for (int j = 0; j < k; j++) sink += fi.getInt(h); },
            k -> { for (int j = 0; j < k; j++) sink += sw.getCallerClass().hashCode() & 1; },
            k -> { for (int j = 0; j < k; j++) sink += sw.walk(s -> s.limit(2).count()); },
            k -> deep(20, kk -> { for (int j = 0; j < kk; j++) sink += fi.getInt(h); }, k),
            k -> deep(60, kk -> { for (int j = 0; j < kk; j++) sink += fi.getInt(h); }, k),
            k -> deep(60, kk -> { for (int j = 0; j < kk; j++) sink += ctorPriv.newInstance(j).i; }, k),
            k -> deep(60, kk -> { for (int j = 0; j < kk; j++) sink += (Integer) add.invoke(h, one); }, k),
        };
        for (int r = 0; r < rounds; r++) {
            StringBuilder out = new StringBuilder("round " + r);
            for (int x = 0; x < rows.length; x++) {
                if (only != null && !only.contains("," + names[x] + ",")) continue;
                int k = (names[x].equals("cipher") || names[x].equals("mac")) ? n / 10 : n;
                long t = System.nanoTime();
                rows[x].run(k);
                out.append(' ').append(names[x]).append('=').append((System.nanoTime() - t) / k);
            }
            System.out.println(out);
        }
        System.out.println("sink " + (sink != 0));
    }
}
