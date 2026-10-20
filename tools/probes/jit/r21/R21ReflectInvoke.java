// JIT round 21 wave 1, lane reflect: reflective invoke rows (Method.invoke static / virtual /
// interface / final-class / private, Constructor.newInstance, Field.getInt / setInt) and the
// answers a compiled reflective call must keep.
//
// Target (r20 handoff perf table, R20PAllocEntryCheck): Method.invoke 23858 / 24860 ns static /
// virtual on CratonVM (12.6 / 11.9 us after r20 w17-w19) against 13 / 14 ns on HotSpot 25.
// Under --jdk-only and --compatible alike (CRATONVM_JDK_REFLECTIVE_INVOKE, default on) the JDK's
// own Method.invoke body runs: DirectMethodHandleAccessor.invoke -> invokeImpl ->
// target.invokeExact(..), whose compiled site r21 links straight to the reflected method under
// CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1 (opt-in; vm jit/helpers.rs reflect_accessor_link_redirect).
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21ReflectInvoke.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21ReflectInvoke                                   # arm off
//   CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1 cratonvm -Xmx512m -cp $P R21ReflectInvoke   # arm on
//   (both arms again with --compatible; both arms with -XX:+UseG1GC and -XX:+UseGenerationalGC
//    for the GC side; once with --nojit for the answers)
//   java -Xmx512m -cp $P R21ReflectInvoke                                       # HotSpot 25
// Further opt-in arms (each alone, then together), interleaved with the default:
//   CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1   (the chain's spun LambdaForm hops without their door;
//                                            fact hidden-walk must equal the default arm's)
//   CRATONVM_JIT_REFLECT_CTOR_LINK=1         (time reflect-ctor; fact ctor* rows unchanged)
//   CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1     (fact hidden-walk is EXPECTED to differ: no hidden frames)
// The native-accessor rows (fact sigpoly-*, slots255-*) also under CRATONVM_JDK_REFLECTIVE_INVOKE=0
// (the VM's Method.invoke door), both modes; kill-switch controls CRATONVM_REFLECT_INVOKE0_VM_TEXTS=0
// and CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_TEXTS=0 bring the accessor's texts back on those rows.
// Kill-switch control for the correctness fix (rows fact sigpoly-reflective*):
//   CRATONVM_REFLECT_SIG_POLY_UOE=0 cratonvm -Xmx512m -cp $P R21ReflectInvoke
//   (expected to differ from HotSpot on those rows only: the handle is dispatched instead of
//    HotSpot's ITE(UnsupportedOperationException: MethodHandle.invokeExact cannot be invoked
//    reflectively)). The sigpoly-wrong-receiver / -wrong-arity / -null-receiver-accessible rows
//    decide docs/known-issues/jit/r21-native-accessor-invoke0-check-messages-20261018.md.
// Fewer calls per rep: -Dr21.n=20000 (default 50000; 9 reps, the median printed, a warm-up of
// 40000 calls first). Only the `time` rows may differ from HotSpot; every other line must be
// identical in every arm and mode (diff them).
//
// HotSpot expectation: time rows ~5-30 ns (C2 inlines the accessor chain), the constructor row a
// little more (allocation). Pass for this lane: the arm-on method rows well below the arm-off ones
// (the linker chain and its spun adapter form are gone per call), no `fact`/`ok` line changed.
// Engagement check (not for timing): CRATONVM_DBG=jit-compiled lists
// jdk/internal/reflect/DirectMethodHandleAccessor.invokeImpl compiled; the arm needs it compiled.

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Arrays;

public class R21ReflectInvoke {
    interface Shape {
        int area(int k);
    }

    static class Sq implements Shape {
        int side;

        Sq() {}

        Sq(int side) {
            this.side = side;
        }

        public int area(int k) {
            return side * side + k;
        }

        public int scale(int k) {
            return side * k;
        }

        public void bump() {
            side++;
        }

        public String name(String p) {
            return p + side;
        }

        public long wide(long a, double d) {
            return a + (long) d;
        }

        private int secret(int k) {
            return k - side;
        }
    }


    // 127 longs + the receiver = 255 slots (the JVM maximum): JDK 25's useNativeAccessor sends it to NativeAccessor.invoke0.
    public long wide127(long a0, long a1, long a2, long a3, long a4, long a5, long a6, long a7, long a8, long a9, long a10, long a11, long a12, long a13, long a14, long a15, long a16, long a17, long a18, long a19, long a20, long a21, long a22, long a23, long a24, long a25, long a26, long a27, long a28, long a29, long a30, long a31, long a32, long a33, long a34, long a35, long a36, long a37, long a38, long a39, long a40, long a41, long a42, long a43, long a44, long a45, long a46, long a47, long a48, long a49, long a50, long a51, long a52, long a53, long a54, long a55, long a56, long a57, long a58, long a59, long a60, long a61, long a62, long a63, long a64, long a65, long a66, long a67, long a68, long a69, long a70, long a71, long a72, long a73, long a74, long a75, long a76, long a77, long a78, long a79, long a80, long a81, long a82, long a83, long a84, long a85, long a86, long a87, long a88, long a89, long a90, long a91, long a92, long a93, long a94, long a95, long a96, long a97, long a98, long a99, long a100, long a101, long a102, long a103, long a104, long a105, long a106, long a107, long a108, long a109, long a110, long a111, long a112, long a113, long a114, long a115, long a116, long a117, long a118, long a119, long a120, long a121, long a122, long a123, long a124, long a125, long a126) {
        return a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8 + a9 + a10 + a11 + a12 + a13 + a14 + a15 + a16 + a17 + a18 + a19 + a20 + a21 + a22 + a23 + a24 + a25 + a26 + a27 + a28 + a29 + a30 + a31 + a32 + a33 + a34 + a35 + a36 + a37 + a38 + a39 + a40 + a41 + a42 + a43 + a44 + a45 + a46 + a47 + a48 + a49 + a50 + a51 + a52 + a53 + a54 + a55 + a56 + a57 + a58 + a59 + a60 + a61 + a62 + a63 + a64 + a65 + a66 + a67 + a68 + a69 + a70 + a71 + a72 + a73 + a74 + a75 + a76 + a77 + a78 + a79 + a80 + a81 + a82 + a83 + a84 + a85 + a86 + a87 + a88 + a89 + a90 + a91 + a92 + a93 + a94 + a95 + a96 + a97 + a98 + a99 + a100 + a101 + a102 + a103 + a104 + a105 + a106 + a107 + a108 + a109 + a110 + a111 + a112 + a113 + a114 + a115 + a116 + a117 + a118 + a119 + a120 + a121 + a122 + a123 + a124 + a125 + a126;
    }
    static final class Fin {
        int v = 3;

        public int get(int k) {
            return v + k;
        }
    }

    public static int add(int a, int b) {
        return a + b;
    }

    public static Integer boxed(int a) {
        return a;
    }

    public static int thrower(int a) {
        if (a >= 0) {
            throw new IllegalStateException("boom" + a);
        }
        return a;
    }

    public static Object npeInside(Object o) {
        return o.toString();
    }

    // The frames between a reflected method and its reflective caller, hidden ones included
    // (spun hidden class names lose their /0x... suffix so the line is deterministic).
    public static String hiddenWalk(int unused) {
        StackWalker w = StackWalker.getInstance(java.util.Set.of(
                StackWalker.Option.SHOW_HIDDEN_FRAMES, StackWalker.Option.SHOW_REFLECT_FRAMES));
        return w.walk(s -> {
            StringBuilder sb = new StringBuilder();
            s.skip(1).takeWhile(f -> !f.getClassName().equals("R21ReflectInvoke"))
                    .forEach(f -> {
                        String c = f.getClassName();
                        int slash = c.indexOf('/');
                        if (slash >= 0) c = c.substring(0, slash);
                        sb.append(c).append('.').append(f.getMethodName()).append(" < ");
                    });
            return sb.toString();
        });
    }

    static int sinkI;
    static Object sinkO;

    static final int N = Integer.getInteger("r21.n", 50_000);
    static final int WARM = 40_000;
    static final int REPS = 9;

    static Method mAdd, mScale, mArea, mFin, mSecret, mBump, mName, mWide, mBoxed, mThrower, mNpe;
    static Constructor<Sq> cSq;
    static Field fSide;

    public static void main(String[] args) throws Throwable {
        mAdd = R21ReflectInvoke.class.getMethod("add", int.class, int.class);
        mScale = Sq.class.getMethod("scale", int.class);
        mArea = Shape.class.getMethod("area", int.class);
        mFin = Fin.class.getMethod("get", int.class);
        mSecret = Sq.class.getDeclaredMethod("secret", int.class);
        mSecret.setAccessible(true);
        mBump = Sq.class.getMethod("bump");
        mName = Sq.class.getMethod("name", String.class);
        mWide = Sq.class.getMethod("wide", long.class, double.class);
        mBoxed = R21ReflectInvoke.class.getMethod("boxed", int.class);
        mThrower = R21ReflectInvoke.class.getMethod("thrower", int.class);
        mNpe = R21ReflectInvoke.class.getMethod("npeInside", Object.class);
        cSq = Sq.class.getDeclaredConstructor(int.class);
        fSide = Sq.class.getDeclaredField("side");

        // Warm every row first (compiles the accessor chain), then the answers, so they are the
        // answers of a hot compiled reflective call.
        for (int r = 0; r < 2; r++) {
            staticRow(WARM);
            virtualRow(WARM);
            interfaceRow(WARM);
            finalRow(WARM);
            privateRow(WARM);
            ctorRow(WARM / 4);
            fieldRow(WARM);
            nameRow(WARM, MIXED);
            nameRow(WARM, PRIMS);
            nameRow(WARM, ARRAYS);
        }
        facts();

        timeRow("reflect-static", 0);
        timeRow("reflect-virtual", 1);
        timeRow("reflect-interface", 2);
        timeRow("reflect-final-class", 3);
        timeRow("reflect-private", 4);
        timeRow("reflect-ctor", 5);
        timeRow("reflect-field-get-set", 6);
        // Class.getName() by mirror kind (orchestrator's R21TriageRows class-name, split):
        // CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1 should bring all three to the plain row's level or below.
        timeRow("class-name-plain", 7);
        timeRow("class-name-array", 8);
        timeRow("class-name-primitive", 9);
        timeRow("class-name-mixed8", 10);
        // The answers once more after the timed loops (fully hot).
        facts();
        nativeFacts();
    }

    static void facts() throws Throwable {
        Sq sq = new Sq(4);
        Fin fin = new Fin();
        fact("static-sum", mAdd.invoke(null, 40, 2));
        fact("virtual", mScale.invoke(sq, 5));
        fact("interface", mArea.invoke(sq, 1));
        fact("final-class", mFin.invoke(fin, 4));
        fact("private", mSecret.invoke(sq, 10));
        Object v = mBump.invoke(sq);
        fact("void-returns-null", String.valueOf(v) + " side=" + sq.side);
        fact("string-arg", mName.invoke(sq, "s"));
        fact("string-null-arg", mName.invoke(sq, (Object) null));
        fact("long-double", mWide.invoke(sq, 7L, 2.9d));
        // Widening: a Short / Byte / Character for an int parameter.
        fact("widen-short", mAdd.invoke(null, (short) 3, (byte) 4));
        fact("widen-char", mAdd.invoke(null, 'a', 1));
        // Integer.valueOf identity of the boxed int result (the JDK adapter boxes with valueOf).
        Object a1 = mAdd.invoke(null, 50, 50);
        Object a2 = mAdd.invoke(null, 60, 40);
        Object b1 = mAdd.invoke(null, 500, 500);
        Object b2 = mAdd.invoke(null, 600, 400);
        fact("box-identity-small", (a1 == a2) + " " + a1.getClass().getName());
        fact("box-identity-large", (b1 == b2) + " " + b1.equals(b2));
        fact("reference-return-kept", mBoxed.invoke(null, 1234) instanceof Integer);
        fact("hashcode-class", mAdd.invoke(null, 1, 1).getClass() == Integer.class);

        // Refusals: the JDK's own messages.
        refuse("wrong-arity", () -> mAdd.invoke(null, 1));
        refuse("wrong-arity-none", () -> mAdd.invoke(null));
        refuse("type-mismatch", () -> mAdd.invoke(null, "x", 1));
        refuse("long-for-int", () -> mAdd.invoke(null, 1L, 1));
        refuse("null-for-primitive", () -> mAdd.invoke(null, null, 1));
        refuse("string-param-gets-int", () -> mName.invoke(sq, 5));
        refuse("null-receiver", () -> mScale.invoke(null, 1));
        refuse("wrong-receiver", () -> mScale.invoke("str", 1));
        refuse("interface-wrong-receiver", () -> mArea.invoke(new Object(), 1));
        refuse("ctor-wrong-arity", () -> cSq.newInstance());
        refuse("ctor-type-mismatch", () -> cSq.newInstance("x"));
        refuse("field-wrong-receiver", () -> fSide.getInt("x"));
        refuse("field-set-type", () -> {
            fSide.set(sq, "x");
            return null;
        });

        // A throwing target: wrapped once, cause unchanged, the cause's frames as HotSpot lists them.
        try {
            mThrower.invoke(null, 7);
            fact("ite", "no throw");
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            fact("ite", c.getClass().getName() + " " + c.getMessage() + " wrapperMsg=" + e.getMessage());
            fact("ite-cause-frames", frames(c, 4));
            fact("ite-wrapper-top", frames(e, 3));
        }
        // An NPE inside the target is an ITE, not an IllegalArgumentException.
        try {
            mNpe.invoke(null, (Object) null);
            fact("npe-inside", "no throw");
        } catch (InvocationTargetException e) {
            fact("npe-inside", "ITE " + e.getCause().getClass().getName()
                    + " top=" + e.getCause().getStackTrace()[0].getMethodName());
        }
        // A call after the refusals still answers.
        fact("after-refusals", mAdd.invoke(null, 2, 3) + " " + mScale.invoke(sq, 2));
        fact("ctor", ((Sq) cSq.newInstance(9)).side);
        fSide.setInt(sq, 11);
        fact("field", fSide.getInt(sq) + " " + fSide.get(sq));
        // Class.getName() answers and identity for every mirror kind, after the hot loops.
        StringBuilder names = new StringBuilder();
        for (Class<?>[] cs : new Class<?>[][] {MIXED, ARRAYS, PRIMS}) {
            for (Class<?> c : cs) {
                names.append(c.getName()).append(c.getName() == c.getName() ? "=" : "!").append(' ');
            }
        }
        fact("class-names", names);
        // Hidden frames of a hot reflective call (CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN keeps them;
        // CRATONVM_JIT_REFLECT_ACCESSOR_LINK is expected to drop them -- the reason it stays opt-in).
        Method mWalk = R21ReflectInvoke.class.getMethod("hiddenWalk", int.class);
        Object last = null;
        for (int i = 0; i < 3000; i++) last = mWalk.invoke(null, i);
        fact("hidden-walk", last);
    }

    // Run last: an arm that mishandles one of these must not cost the rows above.
    static void nativeFacts() throws Throwable {
        // A `native` target (through the method-handle accessor, as any method in JDK 25).
        Method intern = String.class.getMethod("intern");
        Method internOpen = String.class.getMethod("intern");
        internOpen.setAccessible(true);
        Method copy = System.class.getMethod("arraycopy", Object.class, int.class, Object.class,
                int.class, int.class);
        fact("native-ok", intern.invoke("ab"));
        refuse("native-wrong-receiver", () -> intern.invoke(Integer.valueOf(1)));
        refuse("native-wrong-arity", () -> intern.invoke("ab", 1));
        refuse("native-null-receiver", () -> intern.invoke(null));
        refuse("native-null-receiver-accessible", () -> internOpen.invoke(null));
        refuse("native-throws", () -> copy.invoke(null, null, 0, new int[1], 0, 1));
        refuse("native-arg-mismatch", () -> copy.invoke(null, new int[1], "x", new int[1], 0, 1));
        refuse("native-null-for-primitive", () -> copy.invoke(null, new int[1], null, new int[1], 0, 1));

        // A signature-polymorphic target: the one shape JDK 25 still serves through
        // DirectMethodHandleAccessor$NativeAccessor.invoke0 (HotSpot's JVM_InvokeMethod, with the
        // VM's own receiver / arity checks and messages).
        java.lang.invoke.MethodHandle mh = java.lang.invoke.MethodHandles.lookup().findStatic(
                R21ReflectInvoke.class, "add",
                java.lang.invoke.MethodType.methodType(int.class, int.class, int.class));
        Method ie = java.lang.invoke.MethodHandle.class.getMethod("invokeExact", Object[].class);
        Method ieOpen = java.lang.invoke.MethodHandle.class.getMethod("invokeExact", Object[].class);
        ieOpen.setAccessible(true);
        refuse("sigpoly-reflective", () -> ie.invoke(mh, (Object) new Object[] {1, 2}));
        refuse("sigpoly-wrong-receiver", () -> ie.invoke("str", (Object) new Object[0]));
        refuse("sigpoly-wrong-arity", () -> ie.invoke(mh));
        refuse("sigpoly-arg-mismatch", () -> ie.invoke(mh, "notAnArray"));
        refuse("sigpoly-null-receiver", () -> ie.invoke(null, (Object) new Object[0]));
        refuse("sigpoly-null-receiver-accessible", () -> ieOpen.invoke(null, (Object) new Object[0]));

        // 255 slots: the other NativeAccessor shape (review item 3 of
        // docs/internal/fixed-bugs/r21-review-native-findings-FIXED-20261019.md; also run with
        // CRATONVM_JDK_REFLECTIVE_INVOKE=0 so the --compatible Method.invoke door answers).
        Class<?>[] longs = new Class<?>[127];
        java.util.Arrays.fill(longs, long.class);
        Method wide = R21ReflectInvoke.class.getMethod("wide127", longs);
        Method wideOpen = R21ReflectInvoke.class.getMethod("wide127", longs);
        wideOpen.setAccessible(true);
        Object[] ones = new Object[127];
        java.util.Arrays.fill(ones, 1L);
        R21ReflectInvoke self = new R21ReflectInvoke();
        fact("slots255-ok", wide.invoke(self, ones));
        refuse("slots255-wrong-receiver", () -> wide.invoke("str", ones));
        refuse("slots255-wrong-arity", () -> wide.invoke(self, 1L));
        refuse("slots255-null-receiver", () -> wide.invoke(null, ones));
        refuse("slots255-null-receiver-accessible", () -> wideOpen.invoke(null, ones));
        refuse("slots255-arg-mismatch", () -> {
            Object[] bad = ones.clone();
            bad[5] = "x";
            return wide.invoke(self, bad);
        });
    }

    interface Call {
        Object run() throws Throwable;
    }

    static void refuse(String row, Call c) {
        try {
            Object r = c.run();
            fact(row, "no throw: " + r);
        } catch (InvocationTargetException e) {
            fact(row, "ITE(" + e.getCause().getClass().getName() + ": " + e.getCause().getMessage() + ")");
        } catch (Throwable t) {
            String cause = t.getCause() == null ? "" : " cause=" + t.getCause().getClass().getName();
            fact(row, t.getClass().getName() + ": " + t.getMessage() + cause);
        }
    }

    static String frames(Throwable t, int n) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < Math.min(n, st.length); i++) {
            if (i > 0) sb.append(" < ");
            sb.append(st[i].getClassName()).append('.').append(st[i].getMethodName());
        }
        return sb.toString();
    }

    static void fact(String row, Object v) {
        System.out.println("fact " + row + " " + v);
    }

    static void timeRow(String row, int which) throws Throwable {
        long[] t = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            switch (which) {
                case 0 -> staticRow(N);
                case 1 -> virtualRow(N);
                case 2 -> interfaceRow(N);
                case 3 -> finalRow(N);
                case 4 -> privateRow(N);
                case 5 -> ctorRow(N);
                case 6 -> fieldRow(N);
                case 7 -> nameRow(N, PLAIN);
                case 8 -> nameRow(N, ARRAYS);
                case 9 -> nameRow(N, PRIMS);
                default -> nameRow(N, MIXED);
            }
            t[r] = System.nanoTime() - t0;
        }
        Arrays.sort(t);
        System.out.println("time " + row + " " + (t[REPS / 2] / N) + " ns/op");
    }

    static void staticRow(int n) throws Throwable {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) mAdd.invoke(null, i, 2);
        }
        sinkI = s;
    }

    static void virtualRow(int n) throws Throwable {
        Sq sq = new Sq(3);
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) mScale.invoke(sq, i);
        }
        sinkI = s;
    }

    static void interfaceRow(int n) throws Throwable {
        Shape sq = new Sq(2);
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) mArea.invoke(sq, i);
        }
        sinkI = s;
    }

    static void finalRow(int n) throws Throwable {
        Fin f = new Fin();
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) mFin.invoke(f, i);
        }
        sinkI = s;
    }

    static void privateRow(int n) throws Throwable {
        Sq sq = new Sq(1);
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) mSecret.invoke(sq, i);
        }
        sinkI = s;
    }

    static void ctorRow(int n) throws Throwable {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += cSq.newInstance(i).side;
        }
        sinkI = s;
    }

    static final Class<?>[] PLAIN = {String.class, Integer.class, java.util.ArrayList.class, Sq.class};
    static final Class<?>[] ARRAYS = {int[].class, Object[].class, String[][].class, long[].class};
    static final Class<?>[] PRIMS = {int.class, long.class, boolean.class, void.class};
    static final Class<?>[] MIXED = {String.class, Integer.class, java.util.ArrayList.class,
        java.util.HashMap.class, R21ReflectInvoke.class, int[].class, Object[].class, Runnable.class};

    static void nameRow(int n, Class<?>[] cs) {
        int s = 0;
        int mask = cs.length - 1;
        for (int i = 0; i < n; i++) {
            s += cs[i & mask].getName().length();
        }
        sinkI = s;
    }

    static void fieldRow(int n) throws Throwable {
        Sq sq = new Sq(0);
        int s = 0;
        for (int i = 0; i < n; i++) {
            fSide.setInt(sq, i);
            s += fSide.getInt(sq);
        }
        sinkI = s;
    }
}
