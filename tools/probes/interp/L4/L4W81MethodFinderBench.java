// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 81, lane L4: the METHOD finders under
// `--jdk-only`, and the A/B of one default-off switch (proposal
// `docs/internal/fixed-bugs/interpreter-i73-L4-proposal-method-member-resolve-memo-and-mirror-judged-load-DONE-20261115.md`):
//   CRATONVM_MHN_RESOLVE_METHOD_MEMO=1 -- `MethodHandleNatives.resolve` of a
//     METHOD `MemberName` answers a member this thread found before (same
//     collection epoch, same caller, lookup mode and speculative bit) from a
//     per-thread memo instead of re-deriving it (two descriptor renders,
//     `method_exists`, the verdict walk, up to three `declared_methods`
//     clones, the caller access check).
//
// Rows (each a loop of one finder call; the field row is the control: the
// switch does not touch it, and the wave-78 finder mint memo serves it):
//   find-virtual, find-virtual-ref, find-static, find-special,
//   find-constructor, unreflect-method, find-virtual-two-callers (two
//   lookups alternating: two memo keys, both must hit), find-getter.
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"         every loop, min/median over reps
//   "checksum <name> <value>"                     deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). The whole set finishes well under 60 s.
//
// Sets (interleave, 3-5 runs each, medians), `--jdk-only --nojit` first, then
// `--jdk-only` with the JIT, and `--compatible --nojit` as the control (the
// method natives there are the same code, so rows may move a little; nothing
// may get slower):
//   A: CRATONVM_NONE=1
//   B: CRATONVM_MHN_RESOLVE_METHOD_MEMO=1
//
// Wave 82 (lane L4, proposal
// `i81-L4-proposal-a-repeat-method-finder-is-minted-from-its-first-answer`):
// the method finder mint memo, default off. `--jdk-only --nojit`:
//   A: CRATONVM_NONE=1
//   C: CRATONVM_MH_METHOD_FINDER_MINT_MEMO=1
// `--jdk-only` with the JIT (the memo registers there only under its row):
//   A: CRATONVM_NONE=1
//   C: CRATONVM_MH_METHOD_FINDER_MINT_MEMO=1   (no row: must equal A)
//   D: CRATONVM_MH_METHOD_FINDER_MINT_MEMO=1
//      CRATONVM_INTRINSIC_BODY_COMPILES_ROWS=finder-mint,method-finder-mint
// Expected under C (no JIT) and D: find-virtual, find-virtual-ref,
// find-static, find-special, find-constructor, unreflect-method and
// find-virtual-two-callers fall toward find-getter (the field mint); `--compatible
// --nojit` with C is the control (nothing registers there; rows must not move).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum find-virtual 4000
// checksum find-virtual-ref 4000
// checksum find-static 4000
// checksum find-special 4000
// checksum find-constructor 4000
// checksum unreflect-method 4000
// checksum find-virtual-two-callers 4000
// checksum find-getter 4000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.Locale;

public class L4W81MethodFinderBench {
    public static class Box {
        public int v = 3;

        public Box() {
        }

        public int m() {
            return v;
        }

        public String echo(String x) {
            return x;
        }

        public static int st(int x) {
            return x + 1;
        }

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    /** A second caller class: its lookup is another memo key. */
    public static class Other {
        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    interface Body {
        long run(int n) throws Throwable;
    }

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
        MethodHandles.Lookup lk = Box.lookup();
        MethodHandles.Lookup lk2 = Other.lookup();
        Method mMethod = Box.class.getMethod("m");
        MethodType mtM = MethodType.methodType(int.class);
        MethodType mtEcho = MethodType.methodType(String.class, String.class);
        MethodType mtSt = MethodType.methodType(int.class, int.class);
        MethodType mtCtor = MethodType.methodType(void.class);

        int m = 4000;
        single("find-virtual", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findVirtual(Box.class, "m", mtM) != null) s++;
            return s;
        }, reps, m);
        single("find-virtual-ref", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findVirtual(Box.class, "echo", mtEcho) != null) s++;
            return s;
        }, reps, m);
        single("find-static", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findStatic(Box.class, "st", mtSt) != null) s++;
            return s;
        }, reps, m);
        single("find-special", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findSpecial(Box.class, "m", mtM, Box.class) != null) s++;
            return s;
        }, reps, m);
        single("find-constructor", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findConstructor(Box.class, mtCtor) != null) s++;
            return s;
        }, reps, m);
        single("unreflect-method", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.unreflect(mMethod) != null) s++;
            return s;
        }, reps, m);
        single("find-virtual-two-callers", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandles.Lookup l = (i & 1) == 0 ? lk : lk2;
                if (l.findVirtual(Box.class, "m", mtM) != null) s++;
            }
            return s;
        }, reps, m);
        single("find-getter", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
    }
}
