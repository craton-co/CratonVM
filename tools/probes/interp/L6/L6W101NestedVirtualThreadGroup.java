// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L6 -- the residue of
// i100-L6-compatible-first-virtual-thread-group-is-null: a virtual thread
// STARTED BY ANOTHER VIRTUAL THREAD asks for its group first, and no thread
// asked before. Under `--compatible` the outer virtual thread's start (from
// `main`, a platform thread) already initialized `Thread$Constants`
// (`init_virtual_thread_group_before_start`), so the inner one answers
// "VirtualThreads" as on HotSpot. Nothing in this program touches
// `getThreadGroup()` before the innermost thread.
//
// Run: java -cp out L6W101NestedVirtualThreadGroup  (and cratonvm, cratonvm --compatible)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// inner group: VirtualThreads
// inner group parent: system
// inner group max priority: 10
// outer group same object: true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicReference;

public class L6W101NestedVirtualThreadGroup {
    public static void main(String[] args) throws Exception {
        AtomicReference<ThreadGroup> inner = new AtomicReference<>();
        AtomicReference<ThreadGroup> outer = new AtomicReference<>();
        Thread o = Thread.ofVirtual().unstarted(() -> {
            Thread i = Thread.ofVirtual().unstarted(
                    () -> inner.set(Thread.currentThread().getThreadGroup()));
            i.start();
            try {
                i.join();
            } catch (InterruptedException e) {
                return;
            }
            outer.set(Thread.currentThread().getThreadGroup());
        });
        o.start();
        o.join();
        ThreadGroup g = inner.get();
        System.out.println("inner group: " + (g == null ? "null" : g.getName()));
        System.out.println("inner group parent: "
                + (g == null || g.getParent() == null ? "null" : g.getParent().getName()));
        System.out.println("inner group max priority: " + (g == null ? -1 : g.getMaxPriority()));
        System.out.println("outer group same object: " + (g != null && g == outer.get()));
    }
}
