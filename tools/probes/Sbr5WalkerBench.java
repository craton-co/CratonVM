// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR round 5, lane {@code walker}: ns/op of the walker rows and the plain-call decomposition of
 * the array {@code Class.getTypeName}, with same-box controls. One row per run, best of five after
 * a warm-up.
 *
 * <pre>cratonvm --jdk-only -cp . Sbr5WalkerBench &lt;row&gt; [iterations]</pre>
 */
public final class Sbr5WalkerBench {
    interface Op {
        void run(int n) throws Exception;
    }

    static volatile Object sink;
    public static final class Pt { Pt(StackWalker w) {} }
    static int plain(int x) { return x + 1; }

    public static void main(String[] args) throws Exception {
        String row = args[0];
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        StackWalker w = StackWalker.getInstance();
        Op op;
        switch (row) {
            case "empty": op = k -> { for (int i = 0; i < k; i++) sink = null; }; break;
            case "plainStatic": op = k -> { int a = 0; for (int i = 0; i < k; i++) a = plain(a); sink = a; }; break;
            case "typeNamePlain": op = k -> { for (int i = 0; i < k; i++) sink = String.class.getTypeName(); }; break;
            case "typeNameArray": op = k -> { for (int i = 0; i < k; i++) sink = String[][].class.getTypeName(); }; break;
            case "typeNameArray1": op = k -> { for (int i = 0; i < k; i++) sink = int[].class.getTypeName(); }; break;
            case "isArray": op = k -> { Class<?> c = String[][].class; for (int i = 0; i < k; i++) sink = c.isArray(); }; break;
            case "isArrayNB": op = k -> { Class<?> c = String[][].class; int a = 0; for (int i = 0; i < k; i++) if (c.isArray()) a++; sink = a; }; break;
            case "idHash": op = k -> { Object o = new Object(); int a = 0; for (int i = 0; i < k; i++) a += System.identityHashCode(o); sink = a; }; break;
            case "objHash": op = k -> { Object o = new Object(); int a = 0; for (int i = 0; i < k; i++) a += o.hashCode(); sink = a; }; break;
            case "strLen": op = k -> { String o = "abc"; int a = 0; for (int i = 0; i < k; i++) a += o.length(); sink = a; }; break;
            case "walkNop": op = k -> { for (int i = 0; i < k; i++) sink = w.walk(s -> 0); }; break;
            case "ctorNew": { java.lang.reflect.Constructor<Pt> c = Pt.class.getDeclaredConstructor(StackWalker.class); op = k -> { for (int i = 0; i < k; i++) sink = c.newInstance(w); }; break; }
            case "getDeclCtor": op = k -> { for (int i = 0; i < k; i++) sink = Pt.class.getDeclaredConstructor(StackWalker.class); }; break;
            case "arrNew": op = k -> { for (int i = 0; i < k; i++) sink = java.lang.reflect.Array.newInstance(Pt.class, 8); }; break;
            case "streamOf": op = k -> { Object[] a = new Object[8]; for (int i = 0; i < k; i++) sink = java.util.Arrays.stream(a).limit(5).count(); }; break;
            case "fMethodName": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { for (int i = 0; i < k; i++) sink = f.getMethodName(); }; break; }
            case "fClassName": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { for (int i = 0; i < k; i++) sink = f.getClassName(); }; break; }
            case "fDeclaring": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { for (int i = 0; i < k; i++) sink = f.getDeclaringClass(); }; break; }
            case "fLine": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { int a = 0; for (int i = 0; i < k; i++) a += f.getLineNumber(); sink = a; }; break; }
            case "fFile": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { for (int i = 0; i < k; i++) sink = f.getFileName(); }; break; }
            case "fBci": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { int a = 0; for (int i = 0; i < k; i++) a += f.getByteCodeIndex(); sink = a; }; break; }
            case "fNative": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { int a = 0; for (int i = 0; i < k; i++) if (f.isNativeMethod()) a++; sink = a; }; break; }
            case "fMethodType": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { for (int i = 0; i < k; i++) sink = f.getMethodType(); }; break; }
            case "fDescriptor": { StackWalker.StackFrame f = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(x -> x.findFirst().get()); op = k -> { for (int i = 0; i < k; i++) sink = f.getDescriptor(); }; break; }
            case "boolValueOf": op = k -> { int a = 0; for (int i = 0; i < k; i++) if (Boolean.valueOf((i & 1) == 0)) a++; sink = a; }; break;
            case "boolBox": op = k -> { for (int i = 0; i < k; i++) sink = Boolean.valueOf((i & 1) == 0); }; break;
            case "boolUnbox": op = k -> { Boolean b = Boolean.TRUE; int a = 0; for (int i = 0; i < k; i++) if (b.booleanValue()) a++; sink = a; }; break;
            case "intBox": op = k -> { for (int i = 0; i < k; i++) sink = Integer.valueOf(i); }; break;
            case "component": op = k -> { Class<?> c = String[][].class; for (int i = 0; i < k; i++) sink = c.getComponentType(); }; break;
            case "getName": op = k -> { Class<?> c = String.class; for (int i = 0; i < k; i++) sink = c.getName(); }; break;
            case "repeat": op = k -> { for (int i = 0; i < k; i++) sink = "[]".repeat(2); }; break;
            case "concat": op = k -> { String a = "java.lang.String"; String b = "[][]"; for (int i = 0; i < k; i++) sink = a.concat(b); }; break;
            case "walk5": op = k -> { for (int i = 0; i < k; i++) sink = w.walk(s -> s.limit(5).count()); }; break;
            case "walkFirst": op = k -> { for (int i = 0; i < k; i++) sink = w.walk(s -> s.findFirst().get().getMethodName()); }; break;
            case "walkAll": op = k -> { for (int i = 0; i < k; i++) sink = w.walk(s -> s.count()); }; break;
            case "forEach": op = k -> { for (int i = 0; i < k; i++) w.forEach(f -> sink = f); }; break;
            case "caller": op = k -> { StackWalker r = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE); for (int i = 0; i < k; i++) sink = r.getCallerClass(); }; break;
            case "newThrowable": op = k -> { for (int i = 0; i < k; i++) sink = new Throwable().getStackTrace(); }; break;
            default: throw new IllegalArgumentException(row);
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
