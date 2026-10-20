// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.lang.reflect.RecordComponent;
import java.util.Map;
import java.util.WeakHashMap;

/**
 * SBR round 3, lane {@code ref}: ns/op of each retired row's call, one row per run.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr3RefBench &lt;row&gt; [iterations]
 *   # control: CRATONVM_UNRETIRE_NATIVE_SHADOW=&lt;Class.method,...&gt; on the same binary
 * </pre>
 */
public final class Sbr3RefBench {
    record R<T>(int a, java.util.List<String> names, Map<String, T> map) {}

    static final class Pt {
        final int x;
        final int y;

        Pt(int x, int y) {
            this.x = x;
            this.y = y;
        }
    }

    static volatile Object sink;
    static volatile long lsink;

    interface Op {
        void run(int n) throws Exception;
    }

    static final Object STRONG = new Object();
    static final WeakReference<Object> W = new WeakReference<>(STRONG);
    static final SoftReference<Object> S = new SoftReference<>(STRONG);
    static final ReferenceQueue<Object> Q = new ReferenceQueue<>();
    static final RecordComponent[] RC = R.class.getRecordComponents();
    static final ThreadLocal<Object> TL = new ThreadLocal<>();

    public static void main(String[] args) throws Exception {
        String row = args[0];
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        Op op;
        switch (row) {
            case "newWeak":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new WeakReference<>(STRONG);
                    }
                };
                break;
            case "newWeakQ":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new WeakReference<>(STRONG, Q);
                    }
                };
                break;
            case "newSoft":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new SoftReference<>(STRONG);
                    }
                };
                break;
            case "newPhantom":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new PhantomReference<>(STRONG, Q);
                    }
                };
                break;
            case "allocPlain":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new Pt(i, i + 1);
                    }
                };
                break;
            case "allocString":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new StringBuilder("a").append(i).toString();
                    }
                };
                break;
            case "weakGet":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = W.get();
                    }
                };
                break;
            case "softGet":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = S.get();
                    }
                };
                break;
            case "refersTo":
                op = k -> {
                    boolean b = false;
                    for (int i = 0; i < k; i++) {
                        b ^= W.refersTo(STRONG);
                    }
                    sink = b;
                };
                break;
            case "threadLocal":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        TL.set(STRONG);
                        sink = TL.get();
                        TL.remove();
                    }
                };
                break;
            case "weakHashMap":
                op = k -> {
                    WeakHashMap<Object, Object> m = new WeakHashMap<>();
                    Object[] keys = new Object[64];
                    for (int i = 0; i < 64; i++) {
                        keys[i] = new Object();
                    }
                    for (int i = 0; i < k; i++) {
                        Object key = keys[i & 63];
                        m.put(key, key);
                        sink = m.get(key);
                    }
                };
                break;
            case "walk5":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = StackWalker.getInstance().walk(s -> s.limit(5).count());
                    }
                };
                break;
            case "steToString":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new Throwable().getStackTrace()[0].toString();
                    }
                };
                break;
            case "stackTrace":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = new Throwable().getStackTrace();
                    }
                };
                break;
            case "setProperty":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        System.setProperty("sbr3.bench", "v");
                        System.clearProperty("sbr3.bench");
                    }
                };
                break;
            case "getProperties":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = System.getProperties();
                    }
                };
                break;
            case "getProperty":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = System.getProperty("java.version");
                    }
                };
                break;
            case "typeNamePlain":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = String.class.getTypeName();
                    }
                };
                break;
            case "typeNameArray":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = String[][].class.getTypeName();
                    }
                };
                break;
            case "getPackage":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = String.class.getPackage();
                    }
                };
                break;
            case "getPackageApp":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = Sbr3RefBench.class.getPackage();
                    }
                };
                break;
            case "getPackagePlatform":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = java.sql.Driver.class.getPackage();
                    }
                };
                break;
            case "loadClass":
                op = k -> {
                    ClassLoader cl = Sbr3RefBench.class.getClassLoader();
                    for (int i = 0; i < k; i++) {
                        sink = cl.loadClass("java.util.ArrayList");
                    }
                };
                break;
            case "loadClassUser":
                op = k -> {
                    ClassLoader cl = Sbr3RefBench.class.getClassLoader();
                    for (int i = 0; i < k; i++) {
                        sink = cl.loadClass("Sbr3RefBench");
                    }
                };
                break;
            case "recordGenericType":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = RC[1].getGenericType();
                    }
                };
                break;
            case "getField":
                op = k -> {
                    for (int i = 0; i < k; i++) {
                        sink = Sbr3RefBench.class.getField("PUB");
                    }
                };
                break;
            default:
                throw new IllegalArgumentException(row);
        }
        op.run(n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            op.run(n);
            long dt = System.nanoTime() - t0;
            best = Math.min(best, dt);
        }
        System.out.println(row + " " + (best / n) + " ns/op");
    }

    public static int PUB = 1;
}
