// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Constructor;

/**
 * SBR round 4, lane {@code lang}: ns/op of the six rows SBR3 held, one row per
 * run, best of five after a warm-up. Control: {@code CRATONVM_UNRETIRE_NATIVE_SHADOW=<Class.method,...>}
 * on the same binary.
 *
 * <pre>cratonvm --jdk-only -cp . Sbr4LangBench &lt;row&gt; [iterations]</pre>
 */
public final class Sbr4LangBench {
    public static final class Pt {
        public Pt() {}
    }

    interface Op {
        void run(int n) throws Exception;
    }

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String row = args[0];
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        Constructor<Pt> ctor = Pt.class.getConstructor();
        Op op;
        switch (row) {
            case "typeNamePlain":
                op = k -> { for (int i = 0; i < k; i++) sink = String.class.getTypeName(); };
                break;
            case "typeNameArray":
                op = k -> { for (int i = 0; i < k; i++) sink = String[][].class.getTypeName(); };
                break;
            // The parts of the JDK's array `getTypeName` body, one per row.
            case "partIsArray":
                op = k -> { Class<?> c = String[][].class; for (int i = 0; i < k; i++) sink = c.isArray(); };
                break;
            case "partComponent":
                op = k -> { Class<?> c = String[][].class; for (int i = 0; i < k; i++) sink = c.getComponentType(); };
                break;
            case "partGetName":
                op = k -> { Class<?> c = String.class; for (int i = 0; i < k; i++) sink = c.getName(); };
                break;
            case "partRepeat":
                op = k -> { for (int i = 0; i < k; i++) sink = "[]".repeat(2); };
                break;
            case "partConcat":
                op = k -> { String a = "java.lang.String"; String b = "[][]"; for (int i = 0; i < k; i++) sink = a.concat(b); };
                break;
            case "walk5":
                op = k -> { for (int i = 0; i < k; i++) sink = StackWalker.getInstance().walk(s -> s.limit(5).count()); };
                break;
            case "ctorNew":
                op = k -> { for (int i = 0; i < k; i++) sink = ctor.newInstance(); };
                break;
            case "setProperty":
                op = k -> { for (int i = 0; i < k; i++) sink = System.setProperty("sbr4.lang.k", "v" + (i & 7)); };
                break;
            case "clearProperty":
                op = k -> { for (int i = 0; i < k; i++) { System.setProperty("sbr4.lang.c", "x"); sink = System.clearProperty("sbr4.lang.c"); } };
                break;
            case "getProperties":
                op = k -> { for (int i = 0; i < k; i++) sink = System.getProperties(); };
                break;
            case "getProperty":
                op = k -> { for (int i = 0; i < k; i++) sink = System.getProperty("user.dir"); };
                break;
            default:
                throw new IllegalArgumentException(row);
        }
        op.run(n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            op.run(n);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println(row + " " + (best / n) + " ns/op");
    }
}
