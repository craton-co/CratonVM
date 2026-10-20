// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L4: the `--jdk-only` finder mint, split
// into the steps Java can drive separately, plus calibration rows for the
// interpreter's per-invocation and per-native-crossing cost, and the A/B of
// one default-off switch:
//   CRATONVM_MH_METHOD_TYPE_INTERN_MEMO=1 -- a reviewed `Intrinsic` over
//     `MethodType.makeImpl` answers a type this thread interned before (same
//     collection epoch) from a per-thread memo (the `mt-*` rows directly; the
//     `find-*` / `unreflect-*` rows through their one or two intern probes).
//
// Wave 73 medians (`L4W73JdkOnlyFieldBench --jdk-only --nojit`): `find-only`
// ~37 900 ns (2 800 under `--compatible`), `find-static-only` ~31 800,
// `find-setter-only` ~40 600, `unreflect-getter-only` ~31 000.
//
// How to read the split (proposal
// `interpreter-i74-L4-proposal-the-jdk-only-finder-mint-cost-model-DONE-20261014`): a field
// `findGetter` is about 170 interpreted invocations and 15 native crossings
// (two `MethodType` intern probes of ~30 invocations each, the
// `VerifyAccess` checks, `MemberName` predicates, `DirectMethodHandle.make`).
//   * `java-call-x10` / 10 = this VM's cost of one small interpreted call;
//   * `native-is-array`, `native-hash-class`, `object-clone`, `chm-get` = the
//     cost of the native crossings the finder makes (an `isArray` /
//     `isInterface` / identity hash / `Object.clone` of the `MemberName` /
//     the intern table's `ConcurrentHashMap.get`);
//   * `mt-intern-*` / `mt-insert` = one intern probe (`find-only` pays two,
//     `find-static-only` one);
//   * `find-ref-getter` - `find-only` = `VerifyAccess.ensureTypeVisible`'s
//     `Class.forName` for a reference field type of another loader;
//   * `find-virtual-*` / `unreflect-method-only` = the method finders
//     (`MethodHandleNatives.resolve`'s method branch is heavier than the
//     field one: proposal
//     `i73-L4-proposal-method-member-resolve-memo-and-mirror-judged-load-20261106`).
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"         every loop, min/median over reps
//   "checksum <name> <value>"                     deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). Each row-set finishes well under 60 s.
//
// Switch sets (interleave sets, three runs each, take the medians), with
// `--jdk-only --nojit` (the rows the switch moves), `--jdk-only` with the JIT,
// and `--compatible --nojit` (control: the `mt-*` rows may move there too,
// the `find-*` rows are this model's carriers and must not):
//   A: (none)
//   B: CRATONVM_MH_METHOD_TYPE_INTERN_MEMO=1
// Expected, `--jdk-only --nojit`: B moves `mt-intern-*` / `mt-insert` by most
// of an intern probe (the body's ~30 calls against one native crossing),
// `find-only` / `find-setter-only` by two of those, `find-static-only` /
// `unreflect-getter-only` by one; `mt-intern-varied` (1024 types cycling
// through a 128-line memo: nearly all misses) shows the miss penalty and may
// get slower under B.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum java-call-x10 400000
// checksum native-is-array 0
// checksum native-hash-class 40000
// checksum object-clone 120000
// checksum chm-get 40000
// checksum mt-intern-0 40000
// checksum mt-intern-1 40000
// checksum mt-intern-3 40000
// checksum mt-insert 40000
// checksum mt-intern-varied 20000
// checksum lookup-in 40000
// checksum find-only 4000
// checksum find-static-only 4000
// checksum find-setter-only 4000
// checksum unreflect-getter-only 4000
// checksum find-ref-getter 4000
// checksum find-virtual-only 4000
// checksum find-virtual-ref 4000
// checksum unreflect-method-only 4000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.Locale;
import java.util.concurrent.ConcurrentHashMap;

public class L4W74FinderMintSplitBench {
    public static class Box {
        public static int S = 2;
        public int v = 3;
        public String s = "s";

        public int m() {
            return v;
        }

        public String echo(String x) {
            return x;
        }
    }

    static final class Cl implements Cloneable {
        int a = 1;
        int b = 2;

        Cl copy() throws CloneNotSupportedException {
            return (Cl) clone();
        }
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static int c0(int x) { return c1(x) ; }
    static int c1(int x) { return c2(x); }
    static int c2(int x) { return c3(x); }
    static int c3(int x) { return c4(x); }
    static int c4(int x) { return c5(x); }
    static int c5(int x) { return c6(x); }
    static int c6(int x) { return c7(x); }
    static int c7(int x) { return c8(x); }
    static int c8(int x) { return c9(x); }
    static int c9(int x) { return x + 10; }

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

    static void single(String name, Body b, int reps, int n) throws Throwable {
        b.run(Math.max(2, n / 10));
        double[] xs = new double[reps];
        long[] sum = new long[1];
        long first = 0;
        for (int rep = 0; rep < reps; rep++) {
            xs[rep] = time(b, n, sum);
            if (rep == 0) first = sum[0];
        }
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
        System.out.println("checksum " + name + " " + first);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Field vField = Box.class.getField("v");
        Method mMethod = Box.class.getMethod("m");
        Cl cl = new Cl();
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        Object key = MethodType.methodType(int.class, Box.class);
        chm.put(key, key);
        MethodType mtInt = MethodType.methodType(int.class);
        MethodType mtBox = MethodType.methodType(int.class, Box.class);
        MethodType mt3 = MethodType.methodType(int.class, Box.class, long.class, String.class);
        MethodType mtM = MethodType.methodType(int.class);
        MethodType mtEcho = MethodType.methodType(String.class, String.class);

        // 32 classes (int and long arrays of 1..16 dimensions) -> 1024 types.
        Class<?>[] pool = new Class<?>[32];
        for (int d = 1; d <= 16; d++) {
            pool[d - 1] = Array.newInstance(int.class, new int[d]).getClass();
            pool[d + 15] = Array.newInstance(long.class, new int[d]).getClass();
        }
        MethodType[] varied = new MethodType[1024];
        for (int k = 0; k < 1024; k++) varied[k] = MethodType.methodType(pool[k & 31], pool[(k >> 5) & 31]);

        int n = 40000;
        int m = 4000;
        single("java-call-x10", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += c0(0);
            return s;
        }, reps, n);
        single("native-is-array", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (Box.class.isArray()) s++;
            return s;
        }, reps, n);
        int boxHash = Box.class.hashCode();
        single("native-hash-class", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (Box.class.hashCode() == boxHash) s++;
            return s;
        }, reps, n);
        single("object-clone", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                Cl c = cl.copy();
                s += c.a + c.b;
            }
            return s;
        }, reps, n);
        single("chm-get", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (chm.get(key) == key) s++;
            return s;
        }, reps, n);
        single("mt-intern-0", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (MethodType.methodType(int.class) == mtInt) s++;
            return s;
        }, reps, n);
        single("mt-intern-1", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (MethodType.methodType(int.class, Box.class) == mtBox) s++;
            return s;
        }, reps, n);
        single("mt-intern-3", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                if (MethodType.methodType(int.class, Box.class, long.class, String.class) == mt3) s++;
            }
            return s;
        }, reps, n);
        single("mt-insert", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (mtInt.insertParameterTypes(0, Box.class) == mtBox) s++;
            return s;
        }, reps, n);
        single("mt-intern-varied", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                int j = i & 1023;
                if (MethodType.methodType(pool[j & 31], pool[(j >> 5) & 31]) == varied[j]) s++;
            }
            return s;
        }, reps, n / 2);
        single("lookup-in", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.in(Box.class) != null) s++;
            return s;
        }, reps, n);
        single("find-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-static-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findStaticGetter(Box.class, "S", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-setter-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findSetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
        single("unreflect-getter-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.unreflectGetter(vField) != null) s++;
            return s;
        }, reps, m);
        single("find-ref-getter", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "s", String.class) != null) s++;
            return s;
        }, reps, m);
        single("find-virtual-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findVirtual(Box.class, "m", mtM) != null) s++;
            return s;
        }, reps, m);
        single("find-virtual-ref", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findVirtual(Box.class, "echo", mtEcho) != null) s++;
            return s;
        }, reps, m);
        single("unreflect-method-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.unreflect(mMethod) != null) s++;
            return s;
        }, reps, m);
    }
}
