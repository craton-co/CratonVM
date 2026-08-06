import java.util.concurrent.atomic.AtomicInteger;

/**
 * Which compiled call shapes actually ENTER `jit_invoke_virtual_mic` — the
 * helper the x64 backend emits for compiled invokevirtual/invokeinterface —
 * and what one entry costs.
 *
 * This exists because the obvious instruments do not measure the path:
 *
 * <ul>
 * <li>CratonBench never enters it. Its `bintrees` row is `static` methods on a
 *     `static final class Node`, and its `hashmap` row is served by
 *     `native-collections`; both report `distinct JitSiteKeys=0` under
 *     `CRATONVM_DBG_SITE_ALIAS=1`.</li>
 * <li>A hot Java-target virtual call does not enter it per call either, at ANY
 *     receiver-type count. `JIT_PIC_ENTRIES` is 4, so up to four types are
 *     served by the polymorphic inline cache; beyond that the site goes
 *     megamorphic and is still served in compiled code. Measured: 4 types
 *     7.3 ns/call, 8 types 9.0 ns/call, and `CRATONVM_DBG_SITE_ALIAS=1` — which
 *     makes every entry run a hash lookup and three string compares — changed
 *     neither. A "megamorphic" probe is therefore NOT a probe of this helper.</li>
 * <li>A hot call to a NATIVE method does enter it, every call: that is the
 *     leaf-native / site-cached-native fast path the helper carries.</li>
 * </ul>
 *
 * So `mode=native` is the arm that measures the helper, and `mode=virtual` is
 * kept as the control that shows it does not.
 *
 * **Check reach before believing any number here.** Run each arm with
 * `CRATONVM_DBG_SITE_ALIAS=1` and compare against the same arm without it: a
 * shape that truly enters the helper per call gets materially slower with the
 * flag on, because the flag is what makes each entry do real work. A shape
 * whose two arms match is not entering the helper, and its timing says nothing
 * about it.
 *
 * Args: [mode=native|virtual] [iterations] [types] [warmupRounds]
 */
public class JitDispatchHelperProbe {

    interface Op {
        int apply(int x);
    }

    static final class T0 implements Op { public int apply(int x) { return x + 1; } }
    static final class T1 implements Op { public int apply(int x) { return x - 1; } }
    static final class T2 implements Op { public int apply(int x) { return x ^ 3; } }
    static final class T3 implements Op { public int apply(int x) { return x << 1; } }
    static final class T4 implements Op { public int apply(int x) { return x + 2; } }
    static final class T5 implements Op { public int apply(int x) { return x - 2; } }
    static final class T6 implements Op { public int apply(int x) { return x ^ 5; } }
    static final class T7 implements Op { public int apply(int x) { return x >> 1; } }

    static Op[] build(int types) {
        Op[] all = { new T0(), new T1(), new T2(), new T3(),
                     new T4(), new T5(), new T6(), new T7() };
        if (types < 1 || types > all.length) {
            throw new IllegalArgumentException("types must be 1.." + all.length);
        }
        int ring = Integer.highestOneBit(types) == types ? types : Integer.highestOneBit(types) * 2;
        Op[] ops = new Op[ring];
        for (int i = 0; i < ring; i++) {
            ops[i] = all[i % types];
        }
        return ops;
    }

    static long driveVirtual(Op[] ops, int mask, int iterations) {
        long acc = 0;
        for (int i = 0; i < iterations; i++) {
            acc += ops[i & mask].apply(i);
        }
        return acc;
    }

    /**
     * `AtomicInteger.get()` is a registered native, and the surrounding comment
     * on the helper cites exactly this call ("measured 1026 ns before and
     * 926 ns after") as what arrives there from compiled code.
     */
    static long driveNative(AtomicInteger counter, int iterations) {
        long acc = 0;
        for (int i = 0; i < iterations; i++) {
            acc += counter.get();
        }
        return acc;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "native";
        int iterations = args.length > 1 ? Integer.parseInt(args[1]) : 20_000_000;
        int types = args.length > 2 ? Integer.parseInt(args[2]) : 8;
        int warmups = args.length > 3 ? Integer.parseInt(args[3]) : 2;

        Op[] ops = build(types);
        int mask = ops.length - 1;
        AtomicInteger counter = new AtomicInteger(7);

        long guard = 0;
        int warmIters = Math.min(iterations, 2_000_000);
        for (int w = 0; w < warmups; w++) {
            guard += mode.equals("native")
                    ? driveNative(counter, warmIters)
                    : driveVirtual(ops, mask, warmIters);
        }

        long t0 = System.nanoTime();
        long acc = mode.equals("native")
                ? driveNative(counter, iterations)
                : driveVirtual(ops, mask, iterations);
        long ns = System.nanoTime() - t0;

        System.out.println("DISPATCH-RESULT mode=" + mode
                + (mode.equals("native") ? "" : " types=" + types)
                + " iterations=" + iterations
                + " ms=" + (ns / 1_000_000)
                + " ns_per_call=" + String.format("%.2f", (double) ns / iterations)
                + " checksum=" + (acc + guard * 0));
    }
}
