// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 84, lane L4: the measurement the default-off
// switch CRATONVM_MH_REAL_AS_TYPE_MEMO (wave 82; `asTypeUncached` mints a
// converting adapter once two answers for one (receiver class, type, form,
// new type) agreed) still lacks before triage can flip it: WITH the JIT.
// Wave 82 measured only `--jdk-only --nojit` (`L4W72JdkOnlyMintBench`
// `call bind-getter-object` 27.9 -> 8.7 us). That bench warms each loop with
// one tenth of a pass, so under the JIT most of its timed reps run before the
// loop has tiered up; this one warms every loop three times first, and splits
// the converting `asType` of a FRESH receiver by receiver shape, so a row that
// moves says which arm of the memo moved it.
//
// Rows (each timed against its control, interleaved per rep; "call" is the
// median of row - control, i.e. the asType (+ invoke) cost alone):
//   bind-astype        getter.bindTo(box).asType(()Object)      bound species (Species_LL)
//   bind-invoke-object (Object) getter.bindTo(box).invoke()     the wave-82 row, via the linker
//   insert-astype      insertArguments(add,0,k).asType((int)Object).invokeExact(100)
//                                                              bound primitive (Species_LI)
//   find-astype        findVirtual(Box,"a",()int).asType((Box)Object).invokeExact(box)
//                                                              direct receiver (reinvoker)
//   echo-cast-astype   echo.bindTo(box).asType((Object)Object)  REFERENCE cast: a fresh
//                                                              MH_cast.bindTo(String) per answer,
//                                                              so the key is REFUSED; must not move
//                                                              (it prices the memo's bookkeeping)
//   alt-astype         ONE bound handle, asType alternating ()Object / ()long: the
//                                                              JDK's one-entry asTypeCache misses
//                                                              every call; wave 82's memo never
//                                                              promoted a key on the SAME receiver,
//                                                              wave 84's mints it for that receiver
//                                                              (`PromotedSelf`), so this row moves now
//   churn-astype       bind-astype plus a 16 KiB allocation per call: every collection
//                                                              ends the memo's epoch, so this row
//                                                              prices the re-learning (two body
//                                                              runs per key per epoch)
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"   every loop, min/median over reps
//   "call <name> med <ns> ns/op"           median of (row - its control)
//   "checksum <name> <value>"              deterministic (rep 0), diffed against HotSpot
// Lines containing "ns/" are timings. Args: [reps] (default 5).
//
// Run (interleave A and B, 3-5 runs each, take medians; see the
// microbench-noise note: in-JVM timings swing ~3x between reps here):
//   javac -d . L4W84AsTypeMemoJitBench.java
//   A: CRATONVM_NONE=1                  cratonvm --jdk-only        -cp . L4W84AsTypeMemoJitBench
//   B: CRATONVM_MH_REAL_AS_TYPE_MEMO=1  cratonvm --jdk-only        -cp . L4W84AsTypeMemoJitBench
//   A: CRATONVM_NONE=1                  cratonvm --jdk-only --nojit -cp . L4W84AsTypeMemoJitBench
//   B: CRATONVM_MH_REAL_AS_TYPE_MEMO=1  cratonvm --jdk-only --nojit -cp . L4W84AsTypeMemoJitBench
//   control (the memo never applies): B with --compatible
//   tier-up check (JIT runs): add CRATONVM_DBG=jit-compiled,tierup-decline once
//   and confirm the lambda bodies `lambda$main$*` compile before the timed reps.
//
// Want under B, `--jdk-only` with the JIT: `call bind-astype`,
// `call bind-invoke-object`, `call insert-astype`, `call find-astype` and
// `call alt-astype` drop; `call echo-cast-astype` stays within noise (a rise
// is the memo's bookkeeping cost on a refused key);
// `call churn-astype` drops by less than `call bind-astype` (the gap is the
// per-epoch re-learning). Every checksum identical to HotSpot in every set.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum bind-only 20000
// checksum bind-astype 20000
// checksum bind-invoke-object 60000
// checksum insert-only 20000
// checksum insert-astype 2070000
// checksum find-only 10000
// checksum find-astype 40000
// checksum echo-bind-only 20000
// checksum echo-cast-astype 40000
// checksum alt-control 20000
// checksum alt-astype 30000
// checksum churn-bind-only 60000
// checksum churn-astype 60000
// HOTSPOT_EXPECTED_END
//
// Derivation: Box.v = 3. bind-* rows add 1 per call (non-null handle);
// bind-invoke-object adds v = 3. insert-astype adds 100 + (i & 7): 20000 calls
// are 2500 cycles of 0..7 (sum 28), 2,000,000 + 70,000. find-astype adds
// a() = v + 1 = 4. echo-cast-astype adds "x3".length() = 2. alt-astype adds 1
// for the ()Object half and 2 for the ()long half. churn-* add
// (16384 >> 13) + 1 = 3.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.Locale;

public class L4W84AsTypeMemoJitBench {
    public static class Box {
        public int v;

        public Box(int v) {
            this.v = v;
        }

        public int a() {
            return v + 1;
        }

        public String echo(String x) {
            return x + v;
        }

        public static int add(int x, int y) {
            return x + y;
        }
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    /** Keeps the churn rows' allocation from being elided. */
    static volatile Object SINK;

    static double median(double[] xs) {
        double[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2;
    }

    static double min(double[] xs) {
        double best = Double.MAX_VALUE;
        for (double x : xs) best = Math.min(best, x);
        return best;
    }

    static double time(Body b, int n, long[] sum) throws Throwable {
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        sum[0] = s;
        return (t1 - t0) / (double) n;
    }

    static final java.util.Set<String> REPORTED = new java.util.HashSet<>();

    static void report(String name, double[] xs, long checksum) {
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
        if (REPORTED.add(name)) {
            System.out.println("checksum " + name + " " + checksum);
        }
    }

    /**
     * A control loop and a row loop: three full-length warm passes of each
     * (so a JIT VM has tiered both lambdas up before anything is timed), then
     * the timed reps, interleaved.
     */
    static void pair(String control, Body c, String name, Body b, int reps, int n) throws Throwable {
        for (int w = 0; w < 3; w++) {
            c.run(n);
            b.run(n);
        }
        double[] cs = new double[reps];
        double[] bs = new double[reps];
        double[] ds = new double[reps];
        long[] sum = new long[1];
        long cSum = 0;
        long bSum = 0;
        for (int rep = 0; rep < reps; rep++) {
            cs[rep] = time(c, n, sum);
            if (rep == 0) cSum = sum[0];
            bs[rep] = time(b, n, sum);
            if (rep == 0) bSum = sum[0];
            ds[rep] = bs[rep] - cs[rep];
        }
        report(control, cs, cSum);
        report(name, bs, bSum);
        System.out.printf(Locale.ROOT, "call %s med %.1f ns/op%n", name, median(ds));
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Box box = new Box(3);
        MethodHandle get = lk.findGetter(Box.class, "v", int.class);
        MethodHandle add = lk.findStatic(Box.class, "add",
                MethodType.methodType(int.class, int.class, int.class));
        MethodHandle echo = lk.findVirtual(Box.class, "echo",
                MethodType.methodType(String.class, String.class));
        MethodType mtI = MethodType.methodType(int.class);
        MethodType toObject = MethodType.methodType(Object.class);
        MethodType toLong = MethodType.methodType(long.class);
        MethodType intToObject = MethodType.methodType(Object.class, int.class);
        MethodType boxToObject = MethodType.methodType(Object.class, Box.class);
        MethodType objToObj = MethodType.methodType(Object.class, Object.class);

        int nb = 20000;
        Body bindOnly = k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (get.bindTo(box) != null) s++;
            return s;
        };
        pair("bind-only", bindOnly, "bind-astype", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (get.bindTo(box).asType(toObject) != null) s++;
            return s;
        }, reps, nb);
        pair("bind-only", bindOnly, "bind-invoke-object", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) get.bindTo(box).invoke();
            return s;
        }, reps, nb);

        int ni = 20000;
        pair("insert-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (MethodHandles.insertArguments(add, 0, i & 7) != null) s++;
            return s;
        }, "insert-astype", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandle h = MethodHandles.insertArguments(add, 0, i & 7).asType(intToObject);
                Object r = (Object) h.invokeExact(100);
                s += (Integer) r;
            }
            return s;
        }, reps, ni);

        int nf = 10000;
        pair("find-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findVirtual(Box.class, "a", mtI) != null) s++;
            return s;
        }, "find-astype", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandle h = lk.findVirtual(Box.class, "a", mtI).asType(boxToObject);
                Object r = (Object) h.invokeExact(box);
                s += (Integer) r;
            }
            return s;
        }, reps, nf);

        int ne = 20000;
        pair("echo-bind-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (echo.bindTo(box) != null) s++;
            return s;
        }, "echo-cast-astype", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandle h = echo.bindTo(box).asType(objToObj);
                Object r = (Object) h.invokeExact((Object) "x");
                s += ((String) r).length();
            }
            return s;
        }, reps, ne);

        int na = 20000;
        MethodHandle bound = get.bindTo(box);
        pair("alt-control", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += bound.type().parameterCount() + 1;
            return s;
        }, "alt-astype", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandle h = bound.asType((i & 1) == 0 ? toObject : toLong);
                s += h.type().returnType() == long.class ? 2 : 1;
            }
            return s;
        }, reps, na);

        int nc = 20000;
        pair("churn-bind-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                byte[] junk = new byte[16384];
                SINK = junk;
                s += junk.length >> 13;
                if (get.bindTo(box) != null) s++;
            }
            return s;
        }, "churn-astype", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                byte[] junk = new byte[16384];
                SINK = junk;
                s += junk.length >> 13;
                if (get.bindTo(box).asType(toObject) != null) s++;
            }
            return s;
        }, reps, nc);
    }
}
