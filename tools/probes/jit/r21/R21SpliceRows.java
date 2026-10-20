// JIT round 21 wave 2, lane splice: the IR inliner ("splice") and tier policy -- receiver
// selection, invalidation and frame facts, plus per-op dispatch time rows.
//
// Correctness facts (each warmed so the calling method is compiled with the callee spliced, then
// driven into the case that would expose a wrong splice):
//   cha-*       a site warmed with ONE loaded implementor (abstract class / interface / concrete
//               base), then a second class is loaded by name and passed in; `cha-live` swaps the
//               receiver to a just-loaded subclass in the middle of a running (OSR) loop.
//   pp-*        JVMS 5.4.5 / 5.4.6 package-private overriding across runtime packages, built with
//               java.lang.classfile into two packages of one loader: p1.A.m() package-private,
//               p2.B extends A with a PUBLIC m() that does NOT override it, p1.C extends B with
//               m() that overrides both, p1.E extends B with no m(). The same five classes are
//               defined again in a SECOND loader with other constants: `pp-two-loaders` drives one
//               hot site with two classes of the same name (p1.Runner) from different loaders.
//   dia-*       default-method selection through one invokeinterface site: a more specific
//               default (PQ extends P), and a conflicting pair (P, Q) that must raise
//               IncompatibleClassChangeError once the site is hot.
//   priv-*      an interface private method called from its default method while the receiver
//               class has a PUBLIC method of the same name; a nestmate private method called
//               through invokevirtual on a subclass that declares a same-named method.
//   sync-*      a synchronized callee (instance and static) that throws on a hot path: the
//               monitor is released, the frames are the callee's, the state is exact.
//   lazy-* / boom-*   a callee whose class is not initialized when the caller compiles: its
//               <clinit> runs exactly once, at the first real call, in program order; a
//               failing <clinit> raises ExceptionInInitializerError, then NoClassDefFoundError.
// Every fact line also prints the value computed here by hand where one exists ("expected"), so
// a line is self-checking even without the HotSpot diff.
//
// Time rows (one op = one call at the measured site; 1024-element arrays, 2048 sweeps per rep,
// median of 7 reps after 5 warm-up reps): mono / bi / mega (8 classes) virtual, interface mono /
// bi, a default method, a static dispatcher (switch over six static leaves; first and last arm
// with a constant selector, and a per-element selector), and a deep static chain (depth 8 =
// ir::MAX_IR_SPLICE_DEPTH). The rows are reached through one shared static dispatcher
// (`runRow`), the R19Mega5Nested shape the r20 handoff carried item 5 asks about.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21SpliceRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21SpliceRows                        # jdk-only (launcher default)
//   cratonvm --compatible -Xmx512m -cp $P R21SpliceRows           # both modes, same lines
//   java -Xmx512m -cp $P R21SpliceRows                            # HotSpot 25
// Arms (existing switches this probe exercises; unset vs):
//   CRATONVM_JIT_GUARDED_SPLICE_PACKAGE_PRIVATE=0   (pp-* lines must not change)
//   CRATONVM_JIT_IR_NESTED_PROFILE_OVER_CACHE=0     (pp-*, mega rows)
//   CRATONVM_C2_ACCEPT=always                       (the acceptance-gate control)
//   CRATONVM_C2_ACCEPT_MEMO=0                       (SPLICE21-2: a refusal re-judged every compile)
//   CRATONVM_JIT_IR_INLINE_MAX_NEST=12, with and without
//     CRATONVM_JIT_IR_INLINE_MAX_NEST_FRAME_CLAMP=0 (r21 splice fix, default ON: with the clamp the
//     deep-chain row keeps its splice; =0 lets the 9-frame chain refuse the build -- see DBG_JITC)
//   --nojit                                         (every fact line is the interpreter's)
// Engagement (one run, not for timing): CRATONVM_DBG_JITC=1, grep `inline-resolve REFUSED`,
// `ir-profile-guard`, `nest-profiled`, `ir-splice-profile-order`, `[ir] acceptance`.
//
// HotSpot expectation: every non-`time` line identical (diff them; `fact dia-conflict-msg` is
// HotSpot's own ICCE text). Time rows under C2: ~1 ns for mono / bi / interface / default /
// dispatcher / deep rows (all inlined), 2-6 ns for the 8-class mega row (vtable call).

import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.IntSupplier;
import java.util.function.ToIntFunction;

public class R21SpliceRows {
    static long sink;

    // ------------------------------------------------------------------ cha
    abstract static class Animal {
        abstract int legs();
    }

    static class Dog extends Animal {
        int legs() {
            return 4;
        }
    }

    /** Loaded by name only (never named by other code of the probe). */
    static class Bird extends Animal {
        int legs() {
            return 2;
        }
    }

    interface Shape {
        int area();
    }

    static class Sq implements Shape {
        public int area() {
            return 9;
        }
    }

    /** Loaded by name only. */
    static class Tri implements Shape {
        public int area() {
            return 5;
        }
    }

    static class Base {
        int v() {
            return 1;
        }
    }

    /** Loaded by name only, from inside the running loop. */
    static class Late extends Base {
        int v() {
            return 7;
        }
    }

    static Base cur = new Base();

    static int totalLegs(Animal a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a.legs();
        }
        return s;
    }

    static int totalArea(Shape a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a.area();
        }
        return s;
    }

    static int baseSum(Base b, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += b.v();
        }
        return s;
    }

    static Object byName(String simple) {
        try {
            return Class.forName("R21SpliceRows$" + simple).getDeclaredConstructor().newInstance();
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    static long liveLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += cur.v();
            if (i == n / 2) {
                cur = (Base) byName("Late");
            }
        }
        return s;
    }

    static void chaFacts() {
        Animal dog = new Dog();
        long warm = 0;
        for (int k = 0; k < 4000; k++) {
            warm += totalLegs(dog, 100);
        }
        System.out.println("fact cha-abstract-warm " + warm + " expected " + (4000L * 100 * 4));
        Animal bird = (Animal) byName("Bird");
        System.out.println("fact cha-abstract-late " + totalLegs(bird, 1000) + " expected 2000");
        long mixed = 0;
        for (int k = 0; k < 2000; k++) {
            mixed += totalLegs((k & 1) == 0 ? dog : bird, 100);
        }
        System.out.println("fact cha-abstract-mixed " + mixed + " expected " + (1000L * 400 + 1000L * 200));

        Shape sq = new Sq();
        warm = 0;
        for (int k = 0; k < 4000; k++) {
            warm += totalArea(sq, 100);
        }
        System.out.println("fact cha-iface-warm " + warm + " expected " + (4000L * 100 * 9));
        Shape tri = (Shape) byName("Tri");
        System.out.println("fact cha-iface-late " + totalArea(tri, 1000) + " expected 5000");

        Base base = new Base();
        warm = 0;
        for (int k = 0; k < 4000; k++) {
            warm += baseSum(base, 100);
        }
        System.out.println("fact cha-base-warm " + warm + " expected " + (4000L * 100));

        int n = 4_000_000;
        long live = liveLoop(n);
        long expected = (long) (n / 2 + 1) + 7L * (n - n / 2 - 1);
        System.out.println("fact cha-live " + live + " expected " + expected);
        System.out.println("fact cha-live-class " + cur.getClass().getName());
        System.out.println("fact cha-base-after " + baseSum(base, 1000) + " expected 1000");
        System.out.println("fact cha-late-after " + baseSum(cur, 1000) + " expected 7000");
    }

    // ------------------------------------------------- pp: generated classes
    static final ClassDesc A = ClassDesc.of("p1.A");
    static final ClassDesc B = ClassDesc.of("p2.B");
    static final ClassDesc C = ClassDesc.of("p1.C");
    static final ClassDesc E = ClassDesc.of("p1.E");
    static final ClassDesc RUNNER = ClassDesc.of("p1.Runner");
    static final ClassDesc P = ClassDesc.of("p1.P");
    static final ClassDesc Q = ClassDesc.of("p1.Q");
    static final ClassDesc PQ = ClassDesc.of("p1.PQ");
    static final ClassDesc V = ClassDesc.of("p1.V");
    static final ClassDesc X = ClassDesc.of("p1.X");
    static final ClassDesc W = ClassDesc.of("p1.W");
    static final ClassDesc DIA = ClassDesc.of("p1.Dia");
    static final ClassDesc INT_SUPPLIER = ClassDesc.of("java.util.function.IntSupplier");
    static final ClassDesc TO_INT_FUNCTION = ClassDesc.of("java.util.function.ToIntFunction");
    static final MethodTypeDesc MTD_INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_CALL_A = MethodTypeDesc.of(ConstantDescs.CD_int, A);
    static final MethodTypeDesc MTD_CALL_B = MethodTypeDesc.of(ConstantDescs.CD_int, B);
    static final MethodTypeDesc MTD_APPLY =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object);

    static byte[] cls(ClassDesc self, ClassDesc sup, boolean iface, ClassDesc[] ifaces,
            Consumer<ClassBuilder> body) {
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(iface
                    ? ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT
                    : ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(sup);
            if (ifaces.length > 0) {
                cb.withInterfaceSymbols(ifaces);
            }
            if (!iface) {
                cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                        c -> c.aload(0)
                                .invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .return_());
            }
            body.accept(cb);
        });
    }

    static Consumer<ClassBuilder> intMethod(String name, int flags, int value) {
        final Integer boxed = Integer.valueOf(value);
        return cb -> cb.withMethodBody(name, MTD_INT, flags, c -> c.loadConstant(boxed).ireturn());
    }

    /** The class set of one loader; `k` offsets every constant so two loaders differ. */
    static Map<String, byte[]> generated(int k) {
        ClassDesc obj = ConstantDescs.CD_Object;
        ClassDesc[] none = new ClassDesc[0];
        Map<String, byte[]> m = new HashMap<>();
        m.put("p1.A", cls(A, obj, false, none, cb -> {
            // package-private: no ACC_PUBLIC / ACC_PROTECTED / ACC_PRIVATE
            intMethod("m", 0, k + 1).accept(cb);
            cb.withMethodBody("call", MTD_CALL_A, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.aload(0).invokevirtual(A, "m", MTD_INT).ireturn());
        }));
        m.put("p2.B", cls(B, A, false, none, cb -> {
            intMethod("m", ClassFile.ACC_PUBLIC, k + 2).accept(cb);
            cb.withMethodBody("callB", MTD_CALL_B, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.aload(0).invokevirtual(B, "m", MTD_INT).ireturn());
        }));
        m.put("p1.C", cls(C, B, false, none, intMethod("m", ClassFile.ACC_PUBLIC, k + 3)));
        m.put("p1.E", cls(E, B, false, none, cb -> { }));
        m.put("p1.Runner", cls(RUNNER, obj, false, new ClassDesc[] {INT_SUPPLIER}, cb ->
                cb.withMethodBody("getAsInt", MTD_INT, ClassFile.ACC_PUBLIC, c -> {
                    c.new_(A).dup().invokespecial(A, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokestatic(A, "call", MTD_CALL_A);
                    c.new_(B).dup().invokespecial(B, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokestatic(A, "call", MTD_CALL_A).bipush(3).imul().iadd();
                    c.new_(C).dup().invokespecial(C, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokestatic(A, "call", MTD_CALL_A).bipush(7).imul().iadd();
                    c.new_(E).dup().invokespecial(E, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokestatic(A, "call", MTD_CALL_A).bipush(13).imul().iadd();
                    c.new_(B).dup().invokespecial(B, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokestatic(B, "callB", MTD_CALL_B).bipush(17).imul().iadd();
                    c.new_(C).dup().invokespecial(C, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .invokestatic(B, "callB", MTD_CALL_B).bipush(19).imul().iadd();
                    c.ireturn();
                })));
        // Default-method selection.
        m.put("p1.P", cls(P, obj, true, none, intMethod("d", ClassFile.ACC_PUBLIC, k + 10)));
        m.put("p1.Q", cls(Q, obj, true, none, intMethod("d", ClassFile.ACC_PUBLIC, k + 20)));
        m.put("p1.PQ", cls(PQ, obj, true, new ClassDesc[] {P},
                intMethod("d", ClassFile.ACC_PUBLIC, k + 30)));
        m.put("p1.V", cls(V, obj, false, new ClassDesc[] {P}, cb -> { }));
        m.put("p1.X", cls(X, obj, false, new ClassDesc[] {PQ, P}, cb -> { }));
        m.put("p1.W", cls(W, obj, false, new ClassDesc[] {P, Q}, cb -> { }));
        m.put("p1.Dia", cls(DIA, obj, false, new ClassDesc[] {TO_INT_FUNCTION}, cb ->
                cb.withMethodBody("applyAsInt", MTD_APPLY, ClassFile.ACC_PUBLIC,
                        c -> c.aload(1).checkcast(P).invokeinterface(P, "d", MTD_INT).ireturn())));
        return m;
    }

    static final class GenLoader extends ClassLoader {
        private final Map<String, byte[]> defs;

        GenLoader(String name, Map<String, byte[]> defs) {
            super(name, R21SpliceRows.class.getClassLoader());
            this.defs = defs;
        }

        @Override
        protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] b = defs.get(n);
            if (b == null) {
                throw new ClassNotFoundException(n);
            }
            return defineClass(n, b, 0, b.length);
        }
    }

    static Object make(ClassLoader l, String name) {
        try {
            return Class.forName(name, true, l).getDeclaredConstructor().newInstance();
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    static long runnerLoop(IntSupplier a, IntSupplier b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ((i & 1) == 0 ? a : b).getAsInt();
        }
        return s;
    }

    @SuppressWarnings("unchecked")
    static int dia(ToIntFunction<Object> f, Object o) {
        return f.applyAsInt(o);
    }

    static long diaLoop(ToIntFunction<Object> f, Object a, Object b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += dia(f, (i & 1) == 0 ? a : b);
        }
        return s;
    }

    @SuppressWarnings("unchecked")
    static void ppFacts() {
        ClassLoader l1 = new GenLoader("r21-splice-one", generated(0));
        ClassLoader l2 = new GenLoader("r21-splice-two", generated(100));
        IntSupplier r1 = (IntSupplier) make(l1, "p1.Runner");
        IntSupplier r2 = (IntSupplier) make(l2, "p1.Runner");
        // (k+1) + 3(k+1) + 7(k+3) + 13(k+1) + 17(k+2) + 19(k+3) = 60k + 129
        System.out.println("fact pp-runner-cold " + r1.getAsInt() + " " + r2.getAsInt()
                + " expected 129 6129");
        System.out.println("fact pp-same-name " + r1.getClass().getName().equals(r2.getClass().getName())
                + " distinct " + (r1.getClass() != r2.getClass()));
        long warm = runnerLoop(r1, r1, 400_000);
        System.out.println("fact pp-one-loader " + warm + " expected " + 400_000L * 129);
        long both = runnerLoop(r1, r2, 400_000);
        System.out.println("fact pp-two-loaders " + both + " expected " + (200_000L * 129 + 200_000L * 6129));
        System.out.println("fact pp-runner-hot " + r1.getAsInt() + " " + r2.getAsInt()
                + " expected 129 6129");

        ToIntFunction<Object> d1 = (ToIntFunction<Object>) make(l1, "p1.Dia");
        Object v = make(l1, "p1.V");
        Object x = make(l1, "p1.X");
        Object w = make(l1, "p1.W");
        System.out.println("fact dia-cold " + dia(d1, v) + " " + dia(d1, x) + " expected 10 30");
        long dv = diaLoop(d1, v, x, 400_000);
        System.out.println("fact dia-warm " + dv + " expected " + (200_000L * 10 + 200_000L * 30));
        for (int k = 0; k < 2; k++) {
            try {
                System.out.println("fact dia-conflict NO-THROW " + dia(d1, w));
            } catch (IncompatibleClassChangeError e) {
                System.out.println("fact dia-conflict " + e.getClass().getName());
                System.out.println("fact dia-conflict-msg " + e.getMessage());
            }
        }
        long dv2 = diaLoop(d1, v, x, 400_000);
        System.out.println("fact dia-after " + dv2 + " expected " + (200_000L * 10 + 200_000L * 30));
    }

    // ------------------------------------------------------------- privates
    interface PrivI {
        private int p() {
            return 5;
        }

        default int q() {
            return p();
        }
    }

    static class PrivImpl implements PrivI {
        public int p() {
            return 7;
        }
    }

    static class NmBase {
        private int sec() {
            return 3;
        }
    }

    static class NmSub extends NmBase {
        int sec() {
            return 4;
        }
    }

    static final class NmCaller {
        static int call(NmBase b) {
            return b.sec();
        }
    }

    static long privLoop(PrivI o, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += o.q();
        }
        return s;
    }

    static long nestLoop(NmBase a, NmBase b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += NmCaller.call((i & 1) == 0 ? a : b);
        }
        return s;
    }

    static void privFacts() {
        PrivImpl o = new PrivImpl();
        System.out.println("fact priv-iface-cold " + o.q() + " expected 5");
        System.out.println("fact priv-iface-hot " + privLoop(o, 1_000_000) + " expected 5000000");
        System.out.println("fact priv-iface-public-namesake " + o.p() + " expected 7");
        NmBase base = new NmBase();
        NmBase sub = new NmSub();
        System.out.println("fact priv-nestmate-cold " + NmCaller.call(sub) + " expected 3");
        System.out.println("fact priv-nestmate-hot " + nestLoop(base, sub, 1_000_000) + " expected 3000000");
        System.out.println("fact priv-nestmate-sub-own " + ((NmSub) sub).sec() + " expected 4");
    }

    // ----------------------------------------------------------------- sync
    static final class Acct {
        int bal;
        static int sbal;

        synchronized int dep(int x) {
            if (x % 1000 == 999) {
                throw new IllegalStateException("bad " + x);
            }
            bal += x;
            return bal;
        }

        static synchronized int sdep(int x) {
            if (x % 1000 == 999) {
                throw new IllegalArgumentException("sbad " + x);
            }
            sbal += x;
            return sbal;
        }
    }

    static boolean otherThreadLocks(Object o) {
        boolean[] ok = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (o) {
                ok[0] = true;
            }
        });
        t.start();
        try {
            t.join(10_000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        return ok[0];
    }

    static void syncFacts() {
        Acct a = new Acct();
        int thrown = 0;
        int sthrown = 0;
        String last = null;
        String slast = null;
        String frames = null;
        String sframes = null;
        for (int i = 0; i < 400_000; i++) {
            try {
                a.dep(i % 1000);
            } catch (IllegalStateException e) {
                thrown++;
                last = e.getMessage();
                StackTraceElement[] st = e.getStackTrace();
                frames = st[0].getMethodName() + "<" + st[1].getMethodName();
            }
            try {
                Acct.sdep(i % 1000);
            } catch (IllegalArgumentException e) {
                sthrown++;
                slast = e.getMessage();
                StackTraceElement[] st = e.getStackTrace();
                sframes = st[0].getMethodName() + "<" + st[1].getMethodName();
            }
        }
        // 400 cycles of 0..998
        long per = 999L * 998 / 2;
        System.out.println("fact sync-instance bal=" + a.bal + " expected " + (int) (400 * per)
                + " thrown=" + thrown + " last=" + last + " frames=" + frames);
        System.out.println("fact sync-static bal=" + Acct.sbal + " expected " + (int) (400 * per)
                + " thrown=" + sthrown + " last=" + slast + " frames=" + sframes);
        System.out.println("fact sync-held " + Thread.holdsLock(a) + " " + Thread.holdsLock(Acct.class));
        System.out.println("fact sync-other-thread " + otherThreadLocks(a) + " " + otherThreadLocks(Acct.class));
    }

    // ----------------------------------------------------------------- lazy
    static final class Lazy {
        static int base;

        static {
            System.out.println("fact lazy-clinit ran");
            base = 5;
        }

        static int f(int x) {
            return x + base;
        }
    }

    static final class Boom {
        static int z;

        static {
            if (Boolean.parseBoolean("true")) {
                throw new RuntimeException("boom");
            }
            z = 1;
        }

        static int g(int x) {
            return x + z;
        }
    }

    static int lazyCaller(int i, int n) {
        if (i == n - 1) {
            return Lazy.f(i);
        }
        return i & 1;
    }

    static int boomCaller(int i, int n) {
        if (i >= n - 2) {
            return Boom.g(i);
        }
        return i & 3;
    }

    static void lazyFacts() {
        int n = 2_000_000;
        System.out.println("fact lazy-start");
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += lazyCaller(i, n);
        }
        System.out.println("fact lazy-sum " + s + " expected " + ((n - 1) / 2 + (long) (n - 1 + 5)));
        long t = 0;
        for (int i = 0; i < n; i++) {
            try {
                t += boomCaller(i, n);
            } catch (Throwable e) {
                Throwable cause = e.getCause();
                System.out.println("fact boom-" + (i - (n - 2)) + " " + e.getClass().getName()
                        + " msg=" + e.getMessage()
                        + " cause=" + (cause == null ? "none" : cause.getClass().getName()));
            }
        }
        // i & 3 over 0 .. n-3: (n-2)/4 full cycles of 0+1+2+3, then 0 .. r-1
        int r = (n - 2) % 4;
        System.out.println("fact boom-sum " + t + " expected "
                + ((long) (n - 2) / 4 * 6 + (long) r * (r - 1) / 2));
    }

    // ------------------------------------------------------------ time rows
    static final int N = 1024;
    static final int SWEEPS = 2048;
    static final int REPS = 7;

    abstract static class TB {
        abstract int f(int x);
    }

    interface TI {
        int g(int x);
    }

    interface TD {
        default int h(int x) {
            return x + 11;
        }
    }

    static final class T1 extends TB implements TI, TD {
        int f(int x) {
            return x + 1;
        }

        public int g(int x) {
            return x * 3 + 1;
        }
    }

    static final class T2 extends TB implements TI, TD {
        int f(int x) {
            return x + 2;
        }

        public int g(int x) {
            return x * 3 + 2;
        }
    }

    static final class T3 extends TB {
        int f(int x) {
            return x + 3;
        }
    }

    static final class T4 extends TB {
        int f(int x) {
            return x + 4;
        }
    }

    static final class T5 extends TB {
        int f(int x) {
            return x + 5;
        }
    }

    static final class T6 extends TB {
        int f(int x) {
            return x + 6;
        }
    }

    static final class T7 extends TB {
        int f(int x) {
            return x + 7;
        }
    }

    static final class T8 extends TB {
        int f(int x) {
            return x + 8;
        }
    }

    static TB t(int c) {
        switch (c) {
            case 0: return new T1();
            case 1: return new T2();
            case 2: return new T3();
            case 3: return new T4();
            case 4: return new T5();
            case 5: return new T6();
            case 6: return new T7();
            default: return new T8();
        }
    }

    static TB[] fill(int classes) {
        TB[] a = new TB[N];
        for (int i = 0; i < N; i++) {
            a[i] = t(i % classes);
        }
        return a;
    }

    static final TB[] MONO = fill(1);
    static final TB[] BI = fill(2);
    static final TB[] MEGA = fill(8);
    static final TI[] IMONO = new TI[N];
    static final TI[] IBI = new TI[N];
    static final TD[] DMONO = new TD[N];

    static {
        for (int i = 0; i < N; i++) {
            IMONO[i] = (TI) MONO[i];
            IBI[i] = (TI) BI[i];
            DMONO[i] = (TD) MONO[i];
        }
    }

    static long rowMono() {
        long s = 0;
        TB[] a = MONO;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += a[i].f(i);
            }
        }
        return s;
    }

    static long rowBi() {
        long s = 0;
        TB[] a = BI;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += a[i].f(i);
            }
        }
        return s;
    }

    static long rowMega() {
        long s = 0;
        TB[] a = MEGA;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += a[i].f(i);
            }
        }
        return s;
    }

    static long rowIfaceMono() {
        long s = 0;
        TI[] a = IMONO;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += a[i].g(i);
            }
        }
        return s;
    }

    static long rowIfaceBi() {
        long s = 0;
        TI[] a = IBI;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += a[i].g(i);
            }
        }
        return s;
    }

    static long rowDefault() {
        long s = 0;
        TD[] a = DMONO;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += a[i].h(i);
            }
        }
        return s;
    }

    static int s0(int x) {
        return x + 1;
    }

    static int s1(int x) {
        return x - 1;
    }

    static int s2(int x) {
        return x ^ 2;
    }

    static int s3(int x) {
        return x | 3;
    }

    static int s4(int x) {
        return x & 0xff;
    }

    static int s5(int x) {
        return x * 5;
    }

    static int disp(int w, int x) {
        switch (w) {
            case 0: return s0(x);
            case 1: return s1(x);
            case 2: return s2(x);
            case 3: return s3(x);
            case 4: return s4(x);
            default: return s5(x);
        }
    }

    static long rowDispFirst() {
        long s = 0;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += disp(0, i);
            }
        }
        return s;
    }

    static long rowDispLast() {
        long s = 0;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += disp(5, i);
            }
        }
        return s;
    }

    static long rowDispVar() {
        long s = 0;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += disp(i % 6, i);
            }
        }
        return s;
    }

    static int c0(int x) {
        return c1(x) + 1;
    }

    static int c1(int x) {
        return c2(x) ^ 1;
    }

    static int c2(int x) {
        return c3(x) + 2;
    }

    static int c3(int x) {
        return c4(x) ^ 2;
    }

    static int c4(int x) {
        return c5(x) + 3;
    }

    static int c5(int x) {
        return c6(x) ^ 3;
    }

    static int c6(int x) {
        return c7(x) + 4;
    }

    static int c7(int x) {
        return x ^ 4;
    }

    static long rowDeep() {
        long s = 0;
        for (int o = 0; o < SWEEPS; o++) {
            for (int i = 0; i < N; i++) {
                s += c0(i);
            }
        }
        return s;
    }

    static final String[] ROWS = {
        "mono-virtual", "bi-virtual", "mega-virtual", "iface-mono", "iface-bi", "default-mono",
        "static-dispatch-first", "static-dispatch-last", "static-dispatch-var", "deep-chain",
    };

    /** The shared static dispatcher every row is reached through (R19Mega5Nested's shape). */
    static long runRow(int which) {
        switch (which) {
            case 0: return rowMono();
            case 1: return rowBi();
            case 2: return rowMega();
            case 3: return rowIfaceMono();
            case 4: return rowIfaceBi();
            case 5: return rowDefault();
            case 6: return rowDispFirst();
            case 7: return rowDispLast();
            case 8: return rowDispVar();
            default: return rowDeep();
        }
    }

    static void timeRows() {
        long ops = (long) SWEEPS * N;
        for (int r = 0; r < ROWS.length; r++) {
            System.out.println("fact checksum " + ROWS[r] + " " + runRow(r));
        }
        for (int r = 0; r < ROWS.length; r++) {
            for (int w = 0; w < 5; w++) {
                sink += runRow(r);
            }
            double[] ns = new double[REPS];
            for (int k = 0; k < REPS; k++) {
                long t0 = System.nanoTime();
                sink += runRow(r);
                ns[k] = (System.nanoTime() - t0) / (double) ops;
            }
            Arrays.sort(ns);
            System.out.printf(Locale.ROOT, "time %s %.1f ns/op%n", ROWS[r], ns[REPS / 2]);
        }
    }

    /**
     * Two rows timed WITHOUT the shared dispatcher (their own call sites here), so a
     * dispatcher-planning effect on the rows above reads as a difference against these.
     */
    static void timeDirectRows() {
        long ops = (long) SWEEPS * N;
        for (int w = 0; w < 5; w++) {
            sink += rowMono();
        }
        double[] ns = new double[REPS];
        for (int k = 0; k < REPS; k++) {
            long t0 = System.nanoTime();
            sink += rowMono();
            ns[k] = (System.nanoTime() - t0) / (double) ops;
        }
        Arrays.sort(ns);
        System.out.printf(Locale.ROOT, "time %s %.1f ns/op%n", "mono-virtual-direct", ns[REPS / 2]);
        for (int w = 0; w < 5; w++) {
            sink += rowMega();
        }
        for (int k = 0; k < REPS; k++) {
            long t0 = System.nanoTime();
            sink += rowMega();
            ns[k] = (System.nanoTime() - t0) / (double) ops;
        }
        Arrays.sort(ns);
        System.out.printf(Locale.ROOT, "time %s %.1f ns/op%n", "mega-virtual-direct", ns[REPS / 2]);
    }

    public static void main(String[] args) {
        chaFacts();
        ppFacts();
        privFacts();
        syncFacts();
        lazyFacts();
        timeRows();
        timeDirectRows();
        System.out.println("fact done " + (sink != 42));
    }
}
