// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 70, lane L4: the cost of DERIVING a field-accessor
// link on a fresh handle's linking call, which two default-off switches cut:
//
//   CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE=1   (a pure refactor; proposal
//     `i69-L4-proposal-invoke-link-renders-the-declared-type-once-20261102`):
//     `invoke`'s link no longer runs the candidate screen and the declared-type
//     lookup a second time. Moves ONLY rows whose linking call is an `invoke`
//     at the declared type; the gap is widest with `CRATONVM_MH_RETURN_PLAN=0`
//     (the second lookup was a second `MethodType` render).
//   CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO=1  (proposal
//     `i69-L4-proposal-field-link-derivation-is-memoised-per-source-shape-20261102`):
//     a linking call of a shape already derived on this thread (the same
//     member slots, `type` object, owner and prior link word; the same
//     receiver / value class) writes the remembered words. Moves the `bind-*`
//     rows whose receiver class repeats.
//
// Timings go to STDERR: each row runs `reps` times (default 7) and one line
// per row gives the MINIMUM ("bench <name> <ns/op>"), after one line per rep
// ("rep <name> <i> <ns/op>"). Checksums go to STDOUT once per row
// ("checksum <name> <value>", from the first rep), so stdout is deterministic
// and can be diffed between runs and against HotSpot. Args: [reps].
// Run `--compatible --nojit` and `--jdk-only --nojit`; A/B each switch alone,
// then both, then both with `CRATONVM_MH_LINKING_CALL_SERVED=1` (the linking
// call then skips the door too, so the derivation is a larger share of it).
// Interleave the binaries' runs and take medians: the host is noisy. Leave
// `CRATONVM_MH_BINDTO_PRELINK_FIELD` unset (with it a bound copy is linked at
// mint, and its first call is no longer a linking call).
//
// Rows:                         DECLARED_ONCE      DERIVATION_MEMO
//   bind-getter-first-exact     control            moves
//   bind-getter-first-invoke    moves              moves
//   bind-setter-first-exact     control            moves
//   bind-setter-first-invoke    moves              moves
//   bind-ref-setter-first-exact control            moves (a checked value)
//   bind-getter-rotating3       control            control (three receiver
//                                                  classes in rotation: every
//                                                  call misses and refills;
//                                                  must not get slower beyond
//                                                  the probe's few ns)
//   find-getter-first-exact     control            diagnostic (each `findGetter`
//                                                  mints its own member strings
//                                                  and `type`, so the key is
//                                                  expected to differ per op;
//                                                  if it moves, the finder
//                                                  shares them)
//   find-getter-first-invoke    moves (a little)   diagnostic, as above
// Controls that must NOT move under either switch:
//   bind-only                   - `get.bindTo(box)`, no call: the mint alone
//                                 (`bind-getter-first-exact` minus this is the
//                                 first call's cost)
//   bind-getter-object          - `(Object) get.bindTo(box).invoke()`: an
//                                 `asType` site, never linked (the door alone)
//   find-only                   - `lk.findGetter(..)`, no call
//   getter-warm-exact           - one warm handle, `invokeExact`
//   getter-warm-invoke          - one warm handle, `invoke` at its type
//   static-first-exact          - `findStaticGetter` + `invokeExact`: a static
//                                 link is never memoised
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum bind-only 200000
// checksum bind-getter-object 600000
// checksum bind-getter-first-exact 600000
// checksum bind-getter-first-invoke 600000
// checksum bind-getter-rotating3 799999
// checksum bind-setter-first-exact 200000
// checksum bind-setter-first-invoke 200000
// checksum bind-ref-setter-first-exact 200000
// checksum find-only 20000
// checksum find-getter-first-exact 60000
// checksum find-getter-first-invoke 60000
// checksum static-first-exact 40000
// checksum getter-warm-exact 600000
// checksum getter-warm-invoke 600000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W70LinkDerivationBench {
    public static class Box {
        public static int S = 2;
        public int v = 3;
        public CharSequence cs = "init";
    }

    public static class SubBox extends Box {
    }

    public static class SubBox2 extends Box {
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static void bench(String name, int reps, int n, Body b) throws Throwable {
        b.run(Math.max(2, n / 10));
        double best = Double.MAX_VALUE;
        long checksum = 0;
        for (int rep = 0; rep < reps; rep++) {
            long t0 = System.nanoTime();
            long s = b.run(n);
            long t1 = System.nanoTime();
            double ns = (t1 - t0) / (double) n;
            System.err.printf(java.util.Locale.ROOT, "rep %s %d %.1f%n", name, rep, ns);
            best = Math.min(best, ns);
            if (rep == 0) {
                checksum = s;
            }
        }
        System.err.printf(java.util.Locale.ROOT, "bench %s %.1f%n", name, best);
        System.out.println("checksum " + name + " " + checksum);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 7;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Box box = new Box();
        SubBox sb = new SubBox();
        sb.v = 4;
        SubBox2 sb2 = new SubBox2();
        sb2.v = 5;
        MethodHandle get = lk.findGetter(Box.class, "v", int.class);
        MethodHandle set = lk.findSetter(Box.class, "v", int.class);
        MethodHandle csSet = lk.findSetter(Box.class, "cs", CharSequence.class);
        CharSequence csValue = "cs";
        int n = 200000;

        bench("bind-only", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += get.bindTo(box) != null ? 1 : 0;
            return s;
        });
        bench("bind-getter-object", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) get.bindTo(box).invoke();
            return s;
        });
        bench("bind-getter-first-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invokeExact();
            return s;
        });
        bench("bind-getter-first-invoke", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invoke();
            return s;
        });
        bench("bind-getter-rotating3", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                Box r = i % 3 == 0 ? box : (i % 3 == 1 ? sb : sb2);
                s += (int) get.bindTo(r).invokeExact();
            }
            return s;
        });
        // The setters store box's own value back (3), so the getters' sums hold.
        bench("bind-setter-first-exact", reps, n, k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invokeExact(3);
            return k;
        });
        bench("bind-setter-first-invoke", reps, n, k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invoke(3);
            return k;
        });
        bench("bind-ref-setter-first-exact", reps, n, k -> {
            for (int i = 0; i < k; i++) csSet.bindTo(box).invokeExact(csValue);
            return k;
        });

        int m = 20000;
        bench("find-only", reps, m, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += lk.findGetter(Box.class, "v", int.class) != null ? 1 : 0;
            return s;
        });
        bench("find-getter-first-exact", reps, m, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                s += (int) lk.findGetter(Box.class, "v", int.class).invokeExact(box);
            }
            return s;
        });
        bench("find-getter-first-invoke", reps, m, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                s += (int) lk.findGetter(Box.class, "v", int.class).invoke(box);
            }
            return s;
        });
        bench("static-first-exact", reps, m, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                s += (int) lk.findStaticGetter(Box.class, "S", int.class).invokeExact();
            }
            return s;
        });

        MethodHandle warm = lk.findGetter(Box.class, "v", int.class);
        bench("getter-warm-exact", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) warm.invokeExact(box);
            return s;
        });
        bench("getter-warm-invoke", reps, n, k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) warm.invoke(box);
            return s;
        });
    }
}
