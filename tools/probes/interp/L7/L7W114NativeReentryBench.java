// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 114, lane L7: the native-to-Java re-entry cost
// (pages i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call-20261212 and
// i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-20261127: ~5 us per
// re-entry through `interpreter::execute`). Every row calls the same one-line target,
// `inc(x) = x + 1`, through a different door; `plain-static` is the baseline with no
// native in between, so `row - plain-static` is the door's cost, and dividing by the
// row's re-entry count (`CRATONVM_DBG=mh-finder-census`, see the page) gives the cost of
// one re-entry.
//
// Rows:
//   plain-static      s += inc(i)                                   (baseline, no re-entry)
//   lambda-static     IntUnaryOperator op = ::inc; op.applyAsInt(i) (the VM's lambda-proxy door)
//   mh-exact-static   (int) MH_INC.invokeExact(i)                   (invokeExact door, linker form, linkToStatic)
//   mh-invoke-static  (int) MH_INC.invoke(i)                        (same, through `invoke`)
//   mh-iwa-static     (Integer) MH_INC.invokeWithArguments(i)       (the JDK body, ~5 re-entries per call)
//   mh-bound-getter   (int) BOUND_GETTER.invokeExact()              (i95's bound field getter)
//   reflect-static    M_INC.invoke(null, i)                         (Method.invoke, static)
//   reflect-virtual   M_INC_V.invoke(target, i)                     (Method.invoke, virtual)
//   map-user-key      HashMap<Key,Integer>.get(key)                 (a `--compatible` map native calling Key.hashCode/equals back; bytecode under --jdk-only)
//
// Expected A/B (CRATONVM_EXECUTE_LEAN_REENTRY=1 against unset, same binary, interleaved, medians):
//   plain-static flat; every other row flat to a few percent lower: the switch removes one
//   source-file copy (an `Arc<str>` allocation and free) per re-entry whose callee body is
//   compiled, plus the empty-exception-table allocation and two refcount pairs per re-entry
//   when CRATONVM_EXECUTE_FEWER_COPIES is unset. That is tens of ns of a ~5 us re-entry: the
//   switch is NOT expected to close the gap. The census split (`reentry-prologue.<scope>` /
//   `reentry-epilogue.<scope>` rows, see the page's wave-114 section) says how much of the
//   re-entry is `execute`'s own prologue and how much is the body and the native layers.
//
// Every row runs REPS timed repetitions after one untimed warm-up and reports the MINIMUM.
//
// Run:
//   cratonvm --java-home <jdk25> --jdk-only -cp <dir> L7W114NativeReentryBench [row]
//   CRATONVM_EXECUTE_LEAN_REENTRY=1 cratonvm --java-home <jdk25> --jdk-only -cp <dir> L7W114NativeReentryBench [row]
// Also both arms under --compatible and with --nojit: the checksums must match.
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op (min of N)` timing line (filter out `TIME `).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// plain-static checksum=500000500000
// lambda-static checksum=500000500000
// mh-exact-static checksum=200010000
// mh-invoke-static checksum=200010000
// mh-iwa-static checksum=200010000
// mh-bound-getter checksum=200010000
// reflect-static checksum=200010000
// reflect-virtual checksum=200010000
// map-user-key checksum=649488
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.function.IntUnaryOperator;

public class L7W114NativeReentryBench {
    interface Row {
        long run(int n) throws Throwable;
    }

    static final class Box {
        int v;
    }

    static final class Key {
        final int id;

        Key(int id) {
            this.id = id;
        }

        @Override
        public int hashCode() {
            return id * 31;
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof Key && ((Key) o).id == id;
        }
    }

    static final int REPS = 5;
    static final int N_FAST = 1_000_000;
    static final int N_SLOW = 20_000;

    static int inc(int x) {
        return x + 1;
    }

    int incV(int x) {
        return x + 1;
    }

    static final MethodHandle MH_INC;
    static final MethodHandle BOUND_GETTER;
    static final Box BOX = new Box();
    static final Method M_INC;
    static final Method M_INC_V;
    static final L7W114NativeReentryBench TARGET = new L7W114NativeReentryBench();
    static final IntUnaryOperator OP = L7W114NativeReentryBench::inc;
    static final HashMap<Key, Integer> MAP = new HashMap<>();
    static final Key[] KEYS = new Key[64];

    static {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MH_INC = lookup.findStatic(
                    L7W114NativeReentryBench.class, "inc", MethodType.methodType(int.class, int.class));
            BOUND_GETTER = lookup.findGetter(Box.class, "v", int.class).bindTo(BOX);
            M_INC = L7W114NativeReentryBench.class.getDeclaredMethod("inc", int.class);
            M_INC_V = L7W114NativeReentryBench.class.getDeclaredMethod("incV", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
        for (int k = 0; k < KEYS.length; k++) {
            KEYS[k] = new Key(k);
            MAP.put(new Key(k), k + 1);
        }
    }

    static long plainStatic(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += inc(i);
        }
        return s;
    }

    static long lambdaStatic(int n) {
        IntUnaryOperator op = OP;
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += op.applyAsInt(i);
        }
        return s;
    }

    static long mhExactStatic(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) MH_INC.invokeExact(i);
        }
        return s;
    }

    static long mhInvokeStatic(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) MH_INC.invoke(i);
        }
        return s;
    }

    static long mhIwaStatic(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) MH_INC.invokeWithArguments(i);
        }
        return s;
    }

    static long mhBoundGetter(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BOX.v = i + 1;
            s += (int) BOUND_GETTER.invokeExact();
        }
        return s;
    }

    static long reflectStatic(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) M_INC.invoke(null, i);
        }
        return s;
    }

    static long reflectVirtual(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) M_INC_V.invoke(TARGET, i);
        }
        return s;
    }

    static long mapUserKey(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += MAP.get(KEYS[i & 63]);
        }
        return s;
    }

    static void bench(String filter, String name, int n, Row row) throws Throwable {
        if (filter != null && !filter.equals(name)) {
            return;
        }
        long checksum = row.run(n);
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long c = row.run(n);
            long dt = System.nanoTime() - t0;
            if (c != checksum) {
                System.out.println(name + " checksum drift " + c + " != " + checksum);
            }
            best = Math.min(best, dt);
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.println("TIME " + name + " " + (best / n) + " ns/op (min of " + REPS + ")");
    }

    public static void main(String[] args) throws Throwable {
        String filter = args.length > 0 ? args[0] : null;
        bench(filter, "plain-static", N_FAST, L7W114NativeReentryBench::plainStatic);
        bench(filter, "lambda-static", N_FAST, L7W114NativeReentryBench::lambdaStatic);
        bench(filter, "mh-exact-static", N_SLOW, L7W114NativeReentryBench::mhExactStatic);
        bench(filter, "mh-invoke-static", N_SLOW, L7W114NativeReentryBench::mhInvokeStatic);
        bench(filter, "mh-iwa-static", N_SLOW, L7W114NativeReentryBench::mhIwaStatic);
        bench(filter, "mh-bound-getter", N_SLOW, L7W114NativeReentryBench::mhBoundGetter);
        bench(filter, "reflect-static", N_SLOW, L7W114NativeReentryBench::reflectStatic);
        bench(filter, "reflect-virtual", N_SLOW, L7W114NativeReentryBench::reflectVirtual);
        bench(filter, "map-user-key", N_SLOW, L7W114NativeReentryBench::mapUserKey);
    }
}
