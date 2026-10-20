// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z3/ja (2026-10-12): compiled small reference-array allocation on ZGC.
//
// Before zr z3/ja no compiled tier bumped `new Object[n]` inline on any
// collector: every compiled `anewarray` called `jit_anewarray_object`
// (`zgc-zr2a2-compiled-reference-arrays-never-bump-inline-FIXED-20261012.md`).
// `CRATONVM_JIT_INLINE_TLAB_ANEWARRAY` (default ON, `=0` restores the helper)
// gives it the primitive arm's bump on ZGC. This probe prices it and checks it.
//
// Two phases, both inside methods that get compiled (the checks run in the
// compiled kernel itself, not in `main`):
//
// 1. CHECK (deterministic; its line must equal HotSpot's): `rounds` x 4096
//    arrays of lengths 0..64, alternately `Object[]` and `String[]`, each
//    verified in the kernel -- length, every element null (the body is the
//    TLAB's zeroed chunk), the runtime class, and, every 64th `String[]` of
//    non-zero length, that storing an `Integer` through an `Object[]` view
//    throws `ArrayStoreException` (the class word is the component class) and
//    that storing a `String` does not.
// 2. TIMED: `threads` threads allocate escaping arrays of lengths 0..maxlen
//    into a per-thread ring for `seconds`; CPU from ThreadMXBean per thread.
//
// Usage: java -Xmx512m Zr3JaRefArrayAlloc [threads=1] [seconds=5] [maxlen=64] [kind=ref] [rounds=400]
//   kind = ref (Object[]) | str (String[]) | prim (long[] of the same lengths, the control shape)
//
// The figure of merit is ns_per_alloc_cpu. Compare, interleaved:
//   default vs CRATONVM_JIT_INLINE_TLAB_ANEWARRAY=0 (kill arm) on kind=ref,
//   kind=ref vs kind=prim on the default arm (should be within ~1.3x),
//   HotSpot twins: java -XX:+UseZGC -Xmx512m Zr3JaRefArrayAlloc <same args>.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.atomic.AtomicLong;

public final class Zr3JaRefArrayAlloc {
    static volatile boolean stop;
    static volatile Object sink;
    static final Integer BOXED = 12345;

    /// One CHECK round: 4096 allocations, each verified here. Returns the
    /// number of failed checks; `ase[0]` counts the ArrayStoreExceptions seen.
    static int checkRound(int round, Object[] keep, int[] ase) {
        int bad = 0;
        for (int i = 0; i < 4096; i++) {
            int len = (i + round) % 65;
            Object[] a;
            boolean str = (i & 1) == 1;
            if (str) {
                a = new String[len];
            } else {
                a = new Object[len];
            }
            if (a.length != len) {
                bad++;
            }
            for (int k = 0; k < a.length; k++) {
                if (a[k] != null) {
                    bad++;
                }
            }
            if (str) {
                if (a.getClass() != String[].class || !(a instanceof String[])) {
                    bad++;
                }
                if (len > 0 && (i & 127) == 1) {
                    try {
                        a[len - 1] = BOXED;
                        bad++; // must have thrown
                    } catch (ArrayStoreException e) {
                        ase[0]++;
                    }
                    a[0] = "s";
                    if (a[0] != "s") {
                        bad++;
                    }
                }
            } else if (a.getClass() != Object[].class) {
                bad++;
            }
            keep[i & 255] = a;
        }
        return bad;
    }

    static void burstRef(Object[] ring, long n, int mod) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new Object[(int) ((n + k) % mod)];
        }
    }

    static void burstStr(Object[] ring, long n, int mod) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new String[(int) ((n + k) % mod)];
        }
    }

    static void burstPrim(Object[] ring, long n, int mod) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[(int) ((n + k) % mod)];
        }
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int maxlen = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        String kind = args.length > 3 ? args[3] : "ref";
        int rounds = args.length > 4 ? Integer.parseInt(args[4]) : 400;
        final int code = switch (kind) {
            case "ref" -> 0;
            case "str" -> 1;
            case "prim" -> 2;
            default -> throw new IllegalArgumentException("kind must be ref, str or prim: " + kind);
        };

        // Phase 1: CHECK.
        Object[] keep = new Object[256];
        int[] ase = new int[1];
        long bad = 0;
        for (int r = 0; r < rounds; r++) {
            bad += checkRound(r, keep, ase);
        }
        sink = keep;
        System.out.println("Zr3JaRefArrayAlloc check rounds=" + rounds + " arrays=" + (rounds * 4096L)
                + " bad=" + bad + " ase=" + ase[0] + " ok=" + (bad == 0));

        // Phase 2: TIMED.
        final int mod = maxlen + 1;
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        AtomicLong allocs = new AtomicLong();
        AtomicLong cpu = new AtomicLong();
        long gcCount0 = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount0 += Math.max(0, gc.getCollectionCount());
        }
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ts[i] = new Thread(() -> {
                long n = 0;
                Object[] ring = new Object[1024];
                while (!stop) {
                    switch (code) {
                        case 0 -> burstRef(ring, n, mod);
                        case 1 -> burstStr(ring, n, mod);
                        default -> burstPrim(ring, n, mod);
                    }
                    n += 64;
                }
                sink = ring;
                allocs.addAndGet(n);
                cpu.addAndGet(mx.getCurrentThreadCpuTime());
            });
            ts[i].start();
        }
        Thread.sleep(seconds * 1000L);
        stop = true;
        for (Thread t : ts) {
            t.join();
        }
        long gcCount = -gcCount0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount += Math.max(0, gc.getCollectionCount());
        }
        double cs = Math.max(1, cpu.get()) / 1e9;
        System.out.printf(java.util.Locale.ROOT,
                "timing: Zr3JaRefArrayAlloc kind=%s threads=%d maxlen=%d allocs=%d cpu_s=%.3f"
                        + " ns_per_alloc_cpu=%.2f allocs_per_cpu_s=%.1f allocs_per_s=%.1f gc_count=%d%n",
                kind, threads, maxlen, allocs.get(), cs,
                allocs.get() == 0 ? 0.0 : cpu.get() / (double) allocs.get(),
                allocs.get() / cs, allocs.get() / (double) seconds, gcCount);
    }
}
