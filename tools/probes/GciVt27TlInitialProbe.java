// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i27/vt (gci-i20st-compatible-threadlocal-initial-value-pins-a-virtual-thread):
 * the semantics of a `ThreadLocal.withInitial` miss, which under --compatible
 * now runs as a tail call into a bytecode helper (supplier.get(), then
 * tl.set(v)) instead of a nested call under the `ThreadLocal.get` native. The
 * liveness half (a supplier that parks unmounts) is GciVp18NestedBodyPinProbe
 * tl; this probe pins what must not change:
 *
 *   cache       the supplier runs once per thread; get() returns the same object
 *   throws      a supplier exception propagates and stores nothing (the next
 *               get() runs the supplier again)
 *   self-set    a supplier that set()s its own ThreadLocal: the supplier's
 *               return value wins (setInitialValue stores after the call)
 *   remove      remove() then get() runs the supplier again
 *   recursive   a supplier that calls get() on its own ThreadLocal once (a
 *               nested miss): HotSpot stores the inner value first, then the
 *               outer one wins
 *   vt          N virtual threads whose supplier sleeps 20 ms: N supplier
 *               calls, each thread's two get()s answer the same object
 *
 *   GciVt27TlInitialProbe [N]   (default 64)
 *
 * stdout (deterministic, the same on HotSpot 25):
 *   vt27tl cache same=true calls=1
 *   vt27tl throws threw=first after=second fails=2
 *   vt27tl self-set first=outer again=outer
 *   vt27tl remove calls=2
 *   vt27tl recursive value=outer depth=2 again=outer
 *   vt27tl vt n=<N> calls=<N> mismatches=0
 */
public class GciVt27TlInitialProbe {
    static final AtomicInteger CALLS = new AtomicInteger();
    static final ThreadLocal<Object> CACHED = ThreadLocal.withInitial(() -> {
        CALLS.incrementAndGet();
        return new Object();
    });

    static final AtomicInteger FAILS = new AtomicInteger();
    static final ThreadLocal<String> THROWS = ThreadLocal.withInitial(() -> {
        if (FAILS.getAndIncrement() == 0) {
            throw new IllegalStateException("first");
        }
        return "second";
    });

    static ThreadLocal<String> selfSet;

    static final AtomicInteger DEPTH = new AtomicInteger();
    static ThreadLocal<String> recursive;

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 64;

        Object a = CACHED.get();
        Object b = CACHED.get();
        System.out.println("vt27tl cache same=" + (a == b) + " calls=" + CALLS.get());

        String threw = "none";
        try {
            THROWS.get();
        } catch (IllegalStateException e) {
            threw = e.getMessage();
        }
        String after = THROWS.get();
        System.out.println("vt27tl throws threw=" + threw + " after=" + after + " fails=" + FAILS.get());

        selfSet = ThreadLocal.withInitial(() -> {
            selfSet.set("inner");
            return "outer";
        });
        String first = selfSet.get();
        String again = selfSet.get();
        System.out.println("vt27tl self-set first=" + first + " again=" + again);

        CACHED.remove();
        CACHED.get();
        System.out.println("vt27tl remove calls=" + CALLS.get());

        recursive = ThreadLocal.withInitial(() -> {
            if (DEPTH.incrementAndGet() == 1) {
                recursive.get();
                return "outer";
            }
            return "inner";
        });
        String value = recursive.get();
        String again2 = recursive.get();
        System.out.println("vt27tl recursive value=" + value + " depth=" + DEPTH.get() + " again=" + again2);

        AtomicInteger vcalls = new AtomicInteger();
        AtomicInteger mismatches = new AtomicInteger();
        ThreadLocal<Integer> perThread = ThreadLocal.withInitial(() -> {
            int c = vcalls.incrementAndGet();
            try {
                Thread.sleep(20);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            return Integer.valueOf(1000 + c);
        });
        Thread[] ts = new Thread[n];
        for (int i = 0; i < n; i++) {
            ts[i] = Thread.ofVirtual().start(() -> {
                Integer x = perThread.get();
                Integer y = perThread.get();
                if (x != y) {
                    mismatches.incrementAndGet();
                }
            });
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("vt27tl vt n=" + n + " calls=" + vcalls.get() + " mismatches=" + mismatches.get());
    }
}
