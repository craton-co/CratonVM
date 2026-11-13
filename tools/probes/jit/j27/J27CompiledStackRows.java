// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 5 (lane STACKHUNT): what Java sees of compiled frames. Pages
// docs/feature-designs/jit-ir-optimizations.md
// (the dup-* facts) and the lane's report (the rest, parity checks with no
// defect found by reading).
//
// Each check runs once cold (first iteration of its loop) and once warm (the
// last iteration, after WARM calls compiled, inlined and OSR'd the frames);
// both answers are printed as separate facts, so a compiled-frame defect
// shows as a warm fact that differs from HotSpot while the cold one matches.
// No fact depends on timing.
//
// Run: cratonvm --real-jdk -cp out J27CompiledStackRows   (and --compatible,
// and once with CRATONVM_TIER_C2_THRESHOLD=1000000000 CRATONVM_JIT_OSR_OPTIMIZING=0
// for the single-pass tier). Expected (HotSpot 25, every run):
//   fact throw-rows-cold leaf:51,mid:54,outer:55,throwOnce:59
//   fact throw-rows-warm leaf:51,mid:54,outer:55,throwOnce:59
//   fact caller-class-cold J27CompiledStackRows$Asker
//   fact caller-class-warm J27CompiledStackRows$Asker
//   fact walk-rows-cold walkTop:76,walkVia:80,walkProbe:83
//   fact walk-rows-warm walkTop:76,walkVia:80,walkProbe:83
//   fact walk-bci-matches-cold true
//   fact thread-top-cold java.lang.Thread.getStackTrace,threadTop:92
//   fact thread-top-warm java.lang.Thread.getStackTrace,threadTop:92
//   fact deep-trace-len 1024
//   fact deep-trace-all-deep true
//   fact deep-walk-count 3001
//   fact osr-rows osrLoop:120,osrProbe:124
//   fact dup-exact-cold dupExact:201
//   fact dup-exact-warm dupExact:201
//   fact dup-below-cold dupBelow:302
//   fact dup-below-warm dupBelow:302
//   fact dup-file J27DupLines.java
// Before the STACKHUNT fix CratonVM printed dupExact:202 (the last of the two
// LineNumberTable rows at the trapping pc 4; HotSpot answers the first).
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.stream.Collectors;

public class J27CompiledStackRows {
    static final int WARM = 40_000;
    static final StackWalker RETAIN =
            StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    // ---- 1. a throw from a leaf the caller inlines
    static int leaf(int x) {
        if (x < 0) throw new IllegalStateException("neg");
        return x + 1;
    }
    static int mid(int x) { return leaf(x) * 2; }
    static int outer(int x) { return mid(x) + 3; }

    static Throwable throwOnce(int x) {
        try {
            outer(x);
            return null;
        } catch (IllegalStateException e) {
            return e;
        }
    }

    // ---- 2. StackWalker.getCallerClass through an inlined caller
    static final class Who {
        static Class<?> who() { return RETAIN.getCallerClass(); }
    }
    static final class Asker {
        static Class<?> ask() { return Who.who(); }
    }

    // ---- 3. StackWalker frames of compiled callers
    static String walkTop() {
        return RETAIN.walk(s -> s.limit(3)
                .map(f -> f.getMethodName() + ":" + f.getLineNumber() + "@" + f.getByteCodeIndex())
                .collect(Collectors.joining(",")));
    }
    static String walkVia() { return walkTop(); }

    static String walkProbe() {
        return walkVia();
    }

    static String stripBci(String rows) {
        return rows.replaceAll("@-?[0-9]+", "");
    }

    // ---- 4. Thread.getStackTrace from a compiled frame
    static String threadTop() {
        StackTraceElement[] st = Thread.currentThread().getStackTrace();
        return st[0].getClassName() + "." + st[0].getMethodName() + ","
                + st[1].getMethodName() + ":" + st[1].getLineNumber();
    }

    // ---- 5. MaxJavaStackTraceDepth (default 1024) and an uncapped walk
    static Throwable deep(int n) {
        return n == 0 ? new Throwable() : deep(n - 1);
    }

    static long deepWalk(int n) {
        return n == 0
                ? StackWalker.getInstance().walk(s ->
                        s.takeWhile(f -> !f.getMethodName().equals("deepWalkProbe")).count())
                : deepWalk(n - 1);
    }

    static long deepWalkProbe(int n) {
        return deepWalk(n);
    }

    // ---- 6. a throw after an OSR'd loop
    static Throwable osrLoop(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += i ^ (acc >>> 3);
        }
        String tag = (acc & 1) == 0 ? "e" : "o";
        return new RuntimeException("osr-" + tag);
    }

    static Throwable osrProbe() {
        return osrLoop(20_000_000);
    }

    // ---- 7. a LineNumberTable with two rows at one pc (no javac writes one)
    static final MethodHandle DUP_EXACT;
    static final MethodHandle DUP_BELOW;
    static {
        try {
            byte[] bytes = dupLinesClass();
            Class<?> c = new ClassLoader(J27CompiledStackRows.class.getClassLoader()) {
                Class<?> define() {
                    return defineClass("J27DupLines", bytes, 0, bytes.length);
                }
            }.define();
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            DUP_EXACT = lookup.findStatic(c, "dupExact", MethodType.methodType(void.class));
            DUP_BELOW = lookup.findStatic(c, "dupBelow", MethodType.methodType(void.class));
        } catch (ReflectiveOperationException | IOException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static StackTraceElement dupTop(MethodHandle mh) {
        try {
            mh.invokeExact();
            return null;
        } catch (Throwable t) {
            return t.getStackTrace()[0];
        }
    }

    static String row(StackTraceElement e) {
        return e == null ? "none" : e.getMethodName() + ":" + e.getLineNumber();
    }

    /**
     * public class J27DupLines { public static void dupExact(); dupBelow(); }
     * Both bodies are `new RuntimeException; dup; invokespecial <init>; athrow`
     * (the trace's top frame stands at the invokespecial, pc 4). dupExact's
     * table is (0,100) (4,201) (4,202); dupBelow's is (0,301) (0,302).
     */
    static byte[] dupLinesClass() throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream o = new DataOutputStream(bos);
        o.writeInt(0xCAFEBABE);
        o.writeShort(0);
        o.writeShort(52);
        o.writeShort(17);
        o.writeByte(1); o.writeUTF("J27DupLines");                 // #1
        o.writeByte(7); o.writeShort(1);                           // #2
        o.writeByte(1); o.writeUTF("java/lang/Object");            // #3
        o.writeByte(7); o.writeShort(3);                           // #4
        o.writeByte(1); o.writeUTF("java/lang/RuntimeException");  // #5
        o.writeByte(7); o.writeShort(5);                           // #6
        o.writeByte(1); o.writeUTF("<init>");                      // #7
        o.writeByte(1); o.writeUTF("()V");                         // #8
        o.writeByte(12); o.writeShort(7); o.writeShort(8);         // #9
        o.writeByte(10); o.writeShort(6); o.writeShort(9);         // #10
        o.writeByte(1); o.writeUTF("dupExact");                    // #11
        o.writeByte(1); o.writeUTF("dupBelow");                    // #12
        o.writeByte(1); o.writeUTF("Code");                        // #13
        o.writeByte(1); o.writeUTF("LineNumberTable");             // #14
        o.writeByte(1); o.writeUTF("SourceFile");                  // #15
        o.writeByte(1); o.writeUTF("J27DupLines.java");            // #16
        o.writeShort(0x0021);
        o.writeShort(2);
        o.writeShort(4);
        o.writeShort(0);
        o.writeShort(0);
        o.writeShort(2);
        dupMethod(o, 11, new int[][] {{0, 100}, {4, 201}, {4, 202}});
        dupMethod(o, 12, new int[][] {{0, 301}, {0, 302}});
        o.writeShort(1);
        o.writeShort(15);
        o.writeInt(2);
        o.writeShort(16);
        o.flush();
        return bos.toByteArray();
    }

    static void dupMethod(DataOutputStream o, int name, int[][] rows) throws IOException {
        byte[] code = {(byte) 0xbb, 0, 6, 0x59, (byte) 0xb7, 0, 10, (byte) 0xbf};
        int lntLen = 2 + 4 * rows.length;
        o.writeShort(0x0009);
        o.writeShort(name);
        o.writeShort(8);
        o.writeShort(1);
        o.writeShort(13);
        o.writeInt(2 + 2 + 4 + code.length + 2 + 2 + 2 + 4 + lntLen);
        o.writeShort(2);
        o.writeShort(0);
        o.writeInt(code.length);
        o.write(code);
        o.writeShort(0);
        o.writeShort(1);
        o.writeShort(14);
        o.writeInt(lntLen);
        o.writeShort(rows.length);
        for (int[] r : rows) {
            o.writeShort(r[0]);
            o.writeShort(r[1]);
        }
    }

    static String rows(Throwable t, int n) {
        if (t == null) {
            return "none";
        }
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < Math.min(n, st.length); i++) {
            if (i > 0) {
                sb.append(',');
            }
            sb.append(st[i].getMethodName()).append(':').append(st[i].getLineNumber());
        }
        return sb.toString();
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) {
        String cold = null, warm = null;
        for (int i = 0; i < WARM; i++) {
            Throwable t = throwOnce(i == 0 || i == WARM - 1 ? -1 : i);
            if (i == 0) cold = rows(t, 4);
            if (i == WARM - 1) warm = rows(t, 4);
        }
        fact("throw-rows-cold", cold);
        fact("throw-rows-warm", warm);

        Class<?> c0 = null, c1 = null;
        for (int i = 0; i < WARM; i++) {
            Class<?> c = Asker.ask();
            if (i == 0) c0 = c;
            c1 = c;
        }
        fact("caller-class-cold", c0.getName());
        fact("caller-class-warm", c1.getName());

        String w0 = null, w1 = null;
        for (int i = 0; i < WARM; i++) {
            String w = walkProbe();
            if (i == 0) w0 = w;
            w1 = w;
        }
        fact("walk-rows-cold", stripBci(w0));
        fact("walk-rows-warm", stripBci(w1));
        fact("walk-bci-matches-cold", w0.equals(w1));

        String t0 = null, t1 = null;
        for (int i = 0; i < WARM; i++) {
            String t = threadTop();
            if (i == 0) t0 = t;
            t1 = t;
        }
        fact("thread-top-cold", t0);
        fact("thread-top-warm", t1);

        for (int i = 0; i < WARM / 4; i++) {
            deep(40);
            deepWalk(10);
        }
        StackTraceElement[] st = deep(3000).getStackTrace();
        boolean allDeep = true;
        for (StackTraceElement e : st) {
            allDeep &= e.getMethodName().equals("deep");
        }
        fact("deep-trace-len", st.length);
        fact("deep-trace-all-deep", allDeep);
        fact("deep-walk-count", deepWalkProbe(3000));

        fact("osr-rows", rows(osrProbe(), 2));

        StackTraceElement e0 = null, e1 = null, b0 = null, b1 = null;
        for (int i = 0; i < WARM / 2; i++) {
            StackTraceElement e = dupTop(DUP_EXACT);
            StackTraceElement b = dupTop(DUP_BELOW);
            if (i == 0) {
                e0 = e;
                b0 = b;
            }
            e1 = e;
            b1 = b;
        }
        fact("dup-exact-cold", row(e0));
        fact("dup-exact-warm", row(e1));
        fact("dup-below-cold", row(b0));
        fact("dup-below-warm", row(b1));
        fact("dup-file", e1 == null ? "none" : e1.getFileName());
    }
}
