// JIT round 21 wave 4, lane codecache: compiled-code lifecycle under invalidation.
//
// Three rows, every line deterministic (no timing rows):
//
//  1. cha-threads: four threads run a hot `measure(Shape, int)` whose one loaded implementor
//     (`Square`) a compile devirtualizes and inlines. Between phases the main thread defines a new
//     `Shape` subclass from bytes (java.lang.classfile, a fresh loader each), which withdraws every
//     body that inlined `Square.area`, and from then on the threads mix the new shape in. Each
//     thread's sum is checked against plain arithmetic; the bodies they were inside when the
//     withdrawal landed keep running until they return (obsolete code stays mapped).
//  2. storm-cha / storm-trap: a dependency-invalidation storm (450 rounds, one new `Shape2`
//     subclass per round, past HotSpot's PerMethodRecompilationCutoff of 400 and the lane's
//     CRATONVM_JIT_WITHDRAWAL_RECOMPILE_CUTOFF) and a class-check trap storm (1000 rounds of an
//     Integer-only site meeting one String). Both must answer right and terminate.
//  3. loader-drop: a class from a dropped loader is hot (inlined through an interface call), the
//     loader is released and collected (System.gc() up to 50 times), and the next round's
//     same-named class from a new loader takes the site. Answers checked every round; the
//     `collected=` facts are HotSpot's (G1/Serial/Z unload a dead loader at System.gc()).
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21CodeCacheRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx256m -cp $P R21CodeCacheRows
//   cratonvm --compatible -Xmx256m -cp $P R21CodeCacheRows
//   cratonvm -XX:+UseG1GC -Xmx256m -cp $P R21CodeCacheRows
//   cratonvm -XX:+UseGenerationalGC -Xmx256m -cp $P R21CodeCacheRows
//   cratonvm --nojit -Xmx256m -cp $P R21CodeCacheRows                 # same lines
//   java -Xmx256m -cp $P R21CodeCacheRows                             # HotSpot 25
// Switch arms (lines must not change):
//   CRATONVM_JIT_RESCUE_KEEPS_SIDE_TABLES=0, CRATONVM_JIT_WITHDRAWAL_RECOMPILE_CUTOFF=0
// Engagement (not for diffing): CRATONVM_DBG_JIT_METHOD_STATS=1 prints the code-cache report at exit
// (`recompilations=`, `max_versions_for_one_method=`, `retired=` by reason incl. assumption-invalidated
// and class-unloaded); with the cutoff on, R21CodeCacheRows.measure2's C2 versions stop near 400.
// CRATONVM_DBG_JIT_CODE_FREE=1 prints `[jit-rescued-side-tables]` for every rescued body (expected rare).
//
// HotSpot expectation: every line identical; each row ends `ok=true`; `collected=true` on the
// loader-drop rounds.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.ref.WeakReference;
import java.util.concurrent.CyclicBarrier;

public class R21CodeCacheRows {

    // ---- shared plumbing ---------------------------------------------------------------------

    public static abstract class Shape {
        public Shape() {}
        public abstract int area(int x);
    }

    public static final class Square extends Shape {
        public int area(int x) { return x * 3 + 1; }
    }

    public static abstract class Shape2 {
        public Shape2() {}
        public abstract int area(int x);
    }

    public static final class Base2 extends Shape2 {
        public int area(int x) { return x + 5; }
    }

    public interface IntOp {
        int apply(int x);
    }

    static final class ByteLoader extends ClassLoader {
        ByteLoader() { super(R21CodeCacheRows.class.getClassLoader()); }
        Class<?> define(String name, byte[] bytes) { return defineClass(name, bytes, 0, bytes.length); }
    }

    /** A public class `name` extending `superName` (or Object implementing `iface`) whose
     *  `method(I)I` answers `x * k + c`. */
    static byte[] gen(String name, String superName, String iface, String method, int k, int c) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc sup = ClassDesc.of(superName);
        MethodTypeDesc init = MethodTypeDesc.of(ConstantDescs.CD_void);
        MethodTypeDesc ii = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withSuperclass(sup);
            if (iface != null) {
                cb.withInterfaceSymbols(ClassDesc.of(iface));
            }
            cb.withMethodBody("<init>", init, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(sup, "<init>", init).return_());
            cb.withMethodBody(method, ii, ClassFile.ACC_PUBLIC,
                    code -> code.iload(1).ldc(Integer.valueOf(k)).imul()
                            .ldc(Integer.valueOf(c)).iadd().ireturn());
        });
    }

    static Object newInstance(Class<?> c) throws Exception {
        return c.getConstructor().newInstance();
    }

    // ---- row 1: CHA invalidation of a hot inlined method across four threads ---------------

    static int measure(Shape s, int x) { return s.area(x); }

    static final int THREADS = 4;
    static final int PHASES = 6;
    static final int PER_PHASE = 60_000;
    static volatile Shape[] extra = new Shape[0];

    static long expectedThread(int t) {
        long sum = 0;
        for (int p = 0; p < PHASES; p++) {
            for (int i = 0; i < PER_PHASE; i++) {
                int x = (i + t) & 1023;
                sum += x * 3 + 1;
                // Phase p sees the p shapes defined before it: shape j answers x * (11 + j) + 7.
                if (p > 0 && (i & 15) == 0) {
                    int j = i % p;
                    sum += x * (11 + j) + 7;
                }
            }
        }
        return sum;
    }

    static void rowChaThreads() throws Exception {
        Square sq = new Square();
        long[] sums = new long[THREADS];
        CyclicBarrier start = new CyclicBarrier(THREADS + 1);
        CyclicBarrier end = new CyclicBarrier(THREADS + 1);
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int tid = t;
            ts[t] = new Thread(() -> {
                try {
                    long sum = 0;
                    for (int p = 0; p < PHASES; p++) {
                        start.await();
                        Shape[] shapes = extra;
                        for (int i = 0; i < PER_PHASE; i++) {
                            int x = (i + tid) & 1023;
                            sum += measure(sq, x);
                            if (p > 0 && (i & 15) == 0) {
                                sum += measure(shapes[i % p], x);
                            }
                        }
                        end.await();
                    }
                    sums[tid] = sum;
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            }, "cc-" + t);
            ts[t].start();
        }
        for (int p = 0; p < PHASES; p++) {
            start.await();
            end.await();
            if (p + 1 < PHASES) {
                // The define lands while no thread is inside this phase; the next phase's first
                // calls find the withdrawn body and recompile against two implementors.
                String name = "R21CcShape" + p;
                Class<?> c = new ByteLoader().define(name,
                        gen(name, "R21CodeCacheRows$Shape", null, "area", 11 + p, 7));
                Shape[] next = new Shape[p + 1];
                System.arraycopy(extra, 0, next, 0, p);
                next[p] = (Shape) newInstance(c);
                extra = next;
            }
        }
        for (Thread t : ts) {
            t.join();
        }
        boolean ok = true;
        long total = 0;
        for (int t = 0; t < THREADS; t++) {
            ok &= sums[t] == expectedThread(t);
            total += sums[t];
        }
        System.out.println("row cha-threads threads=" + THREADS + " phases=" + PHASES
                + " total=" + total + " ok=" + ok);
    }

    // ---- row 2: deopt storms ------------------------------------------------------------------

    static int measure2(Shape2 s, int x) { return s.area(x); }

    static void rowStormCha() throws Exception {
        final int rounds = 450;
        Base2 base = new Base2();
        long sum = 0;
        long want = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 4000; i++) {
                sum += measure2(base, i);
                want += i + 5;
            }
            String name = "R21CcStorm" + r;
            Shape2 fresh = (Shape2) newInstance(new ByteLoader().define(name,
                    gen(name, "R21CodeCacheRows$Shape2", null, "area", 2, r)));
            sum += measure2(fresh, r);
            want += 2L * r + r;
        }
        System.out.println("row storm-cha rounds=" + rounds + " sum=" + sum + " ok=" + (sum == want));
    }

    static int typeTrap(Object o) {
        if (o instanceof Integer) {
            return (Integer) o + 1;
        }
        return ((String) o).length() * 100;
    }

    static void rowStormTrap() {
        final int rounds = 1000;
        Object[] ints = new Object[64];
        for (int i = 0; i < ints.length; i++) {
            ints[i] = Integer.valueOf(i);
        }
        long sum = 0;
        long want = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 3000; i++) {
                sum += typeTrap(ints[i & 63]);
                want += (i & 63) + 1;
            }
            String s = "s" + r;
            sum += typeTrap(s);
            want += s.length() * 100L;
        }
        System.out.println("row storm-trap rounds=" + rounds + " sum=" + sum + " ok=" + (sum == want));
    }

    // ---- row 3: a dropped loader whose code was hot -------------------------------------------

    static int drive(IntOp op, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.apply(i & 255);
        }
        return acc;
    }

    static int lastAnswer;

    /** Define `R21CcGen` (x * k + 3) in a fresh loader, run it hot, and hand back only a weak
     *  reference to the loader. */
    static WeakReference<ClassLoader> loaderRound(int k) throws Exception {
        ByteLoader loader = new ByteLoader();
        Class<?> c = loader.define("R21CcGen", gen("R21CcGen", "java.lang.Object",
                "R21CodeCacheRows$IntOp", "apply", k, 3));
        IntOp op = (IntOp) newInstance(c);
        int acc = 0;
        for (int rep = 0; rep < 40; rep++) {
            acc += drive(op, 5000);
        }
        lastAnswer = acc;
        return new WeakReference<>(loader);
    }

    static int expectedLoaderRound(int k) {
        int acc = 0;
        for (int rep = 0; rep < 40; rep++) {
            int a = 0;
            for (int i = 0; i < 5000; i++) {
                a += (i & 255) * k + 3;
            }
            acc += a;
        }
        return acc;
    }

    static void rowLoaderDrop() throws Exception {
        boolean ok = true;
        StringBuilder collected = new StringBuilder();
        for (int round = 0; round < 3; round++) {
            int k = 5 + round * 4;
            WeakReference<ClassLoader> ref = loaderRound(k);
            ok &= lastAnswer == expectedLoaderRound(k);
            for (int i = 0; i < 50 && ref.get() != null; i++) {
                System.gc();
                Thread.sleep(10);
            }
            collected.append(round == 0 ? "" : ",").append(ref.get() == null);
        }
        System.out.println("row loader-drop rounds=3 collected=" + collected + " ok=" + ok);
    }

    public static void main(String[] args) throws Exception {
        rowChaThreads();
        rowStormCha();
        rowStormTrap();
        rowLoaderDrop();
        System.out.println("done");
    }
}
