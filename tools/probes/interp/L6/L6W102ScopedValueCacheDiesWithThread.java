// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L6 -- the per-thread scoped-value cache
// (`Thread.scopedValueCache` / `setScopedValueCache`) across many short-lived
// platform and virtual threads.
//
// HotSpot 25 keeps the cache in the `JavaThread`, so it ends with the thread.
// CratonVM keeps it in a per-VM map keyed by `Thread.tid` behind a JNI global
// root, and a platform thread that ended with a cache installed left its
// entry (and the array) for the life of the VM: one leaked global root per
// dead thread that ever read a bound `ScopedValue` (fixed: the entry goes in
// `release_current_thread_locals`). The leak is not visible to Java; the
// orchestrator checks it with `CRATONVM_DBG=exit-census` global-root counts
// (or a heap histogram of `Object[16]`) between the two arms of
// `ROUNDS`. The printed lines are the semantic guard: each thread sees its
// own binding, an unbound thread sees none, and a new virtual thread does
// not inherit its starter's binding.
//
// Run: java -cp out L6W102ScopedValueCacheDiesWithThread
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// platform threads saw their own binding: true
// virtual threads saw their own binding: true
// unbound read throws: true
// new virtual thread inherits binding: false
// HOTSPOT_EXPECTED_END

import java.util.NoSuchElementException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;

public class L6W102ScopedValueCacheDiesWithThread {
    static final ScopedValue<Integer> SV = ScopedValue.newInstance();
    static final int ROUNDS = Integer.getInteger("rounds", 2_000);

    static boolean run(boolean virtual) throws InterruptedException {
        AtomicInteger good = new AtomicInteger();
        for (int i = 0; i < ROUNDS; i++) {
            final int v = i;
            Runnable r = () -> ScopedValue.where(SV, v).run(() -> {
                if (SV.get() == v && SV.get() == v) {
                    good.incrementAndGet();
                }
            });
            Thread t = virtual ? Thread.ofVirtual().start(r) : Thread.ofPlatform().start(r);
            t.join();
        }
        return good.get() == ROUNDS;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("platform threads saw their own binding: " + run(false));
        System.out.println("virtual threads saw their own binding: " + run(true));
        boolean threw = false;
        try {
            SV.get();
        } catch (NoSuchElementException e) {
            threw = true;
        }
        System.out.println("unbound read throws: " + threw);
        AtomicBoolean inherited = new AtomicBoolean();
        ScopedValue.where(SV, 7).run(() -> {
            try {
                Thread.ofVirtual().start(() -> inherited.set(SV.isBound())).join();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        System.out.println("new virtual thread inherits binding: " + inherited.get());
    }
}
