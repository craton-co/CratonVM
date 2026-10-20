// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i81-L6-main-thread-id-is-2-pow-40-FIXED-20261115.md.
//
// HotSpot 25 gives the primordial `main` thread `Thread.PRIMORDIAL_TID` (3,
// java/lang/Thread.java: `attached && VM.initLevel() < 1`), and every other
// thread `ThreadIdentifiers.next()`, a counter that starts at 4 and that the
// VM also spends on its own non-Java threads. So main's id is exactly 3, the
// JDK system threads (Reference Handler, Finalizer, Common-Cleaner) have
// larger ids in creation order, and a thread the program creates has a larger
// id still. Before wave 82 CratonVM gave main `(1 << 40) + n`.
//
// Only main's id is printed as a number: the other ids depend on how many
// non-Java threads HotSpot numbered first (GC workers, compiler threads), so
// they are printed as order and relation checks.
//
// Run, each mode:
//   cratonvm L6W82MainThreadId
//   cratonvm --compatible L6W82MainThreadId
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// main threadId: 3
// main getId == threadId: true
// main threadId seen from another thread: 3
// system threads by ascending id: main, Reference Handler, Finalizer, Common-Cleaner
// all visible ids positive and distinct: true
// main has the smallest visible id: true
// new thread id > main id: true
// new thread id > every system thread id: true
// second new thread id > first: true
// new thread's own threadId == the creator's view: true
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

public class L6W82MainThreadId {
    // The JDK threads both VMs run as Java threads, in both modes.
    private static final Set<String> SYSTEM = Set.of(
            "main", "Reference Handler", "Finalizer", "Common-Cleaner");

    public static void main(String[] args) throws Exception {
        Thread main = Thread.currentThread();
        long mainId = main.threadId();
        System.out.println("main threadId: " + mainId);
        @SuppressWarnings("deprecation")
        long legacy = main.getId();
        System.out.println("main getId == threadId: " + (legacy == mainId));

        long[] seen = new long[1];
        Thread viewer = new Thread(() -> seen[0] = main.threadId(), "w82-viewer");
        viewer.start();
        viewer.join();
        System.out.println("main threadId seen from another thread: " + seen[0]);

        // Force the Common-Cleaner into existence the same way in both VMs:
        // Inflater registers with jdk.internal.ref.CleanerFactory.cleaner().
        new java.util.zip.Inflater().end();

        Map<Thread, StackTraceElement[]> all = Thread.getAllStackTraces();
        List<Thread> threads = new ArrayList<>(all.keySet());
        threads.sort(Comparator.comparingLong(Thread::threadId));
        List<String> names = new ArrayList<>();
        Set<Long> ids = new HashSet<>();
        boolean positive = true;
        long maxSystem = Long.MIN_VALUE;
        long minId = Long.MAX_VALUE;
        for (Thread t : threads) {
            long id = t.threadId();
            positive &= id > 0;
            ids.add(id);
            minId = Math.min(minId, id);
            if (SYSTEM.contains(t.getName())) {
                names.add(t.getName());
                maxSystem = Math.max(maxSystem, id);
            }
        }
        System.out.println("system threads by ascending id: " + String.join(", ", names));
        System.out.println("all visible ids positive and distinct: "
                + (positive && ids.size() == threads.size()));
        System.out.println("main has the smallest visible id: " + (minId == mainId));

        long[] own = new long[2];
        Thread first = new Thread(() -> own[0] = Thread.currentThread().threadId(), "w82-first");
        Thread second = new Thread(() -> own[1] = Thread.currentThread().threadId(), "w82-second");
        first.start();
        second.start();
        first.join();
        second.join();
        System.out.println("new thread id > main id: " + (first.threadId() > mainId));
        System.out.println("new thread id > every system thread id: "
                + (first.threadId() > maxSystem));
        System.out.println("second new thread id > first: "
                + (second.threadId() > first.threadId()));
        System.out.println("new thread's own threadId == the creator's view: "
                + (own[0] == first.threadId() && own[1] == second.threadId()));
    }
}
