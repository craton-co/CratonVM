// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L6 -- default `Thread-N` names share ONE
// counter, the JDK's.
//
// JDK 25 java/lang/Thread.java: every constructor that is given no name ends
// in the master constructor's `this.name = (name != null) ? name :
// genThreadName();`, and `genThreadName()` is `"Thread-" +
// ThreadNumbering.next()` -- one counter for `new Thread(r)`, `new Thread()`,
// `new Thread(group, r)`, `Thread.ofPlatform().unstarted(r)` and a platform
// builder's `factory()`. A named thread and a virtual thread (unnamed virtual
// threads are named "") consume no number. HotSpot creates no unnamed thread
// before `main`, so the program's first unnamed thread is `Thread-0`.
//
// Before wave 84, `--compatible` answered the three constructor carriers
// (`Thread()`, `Thread(Runnable)`, `Thread(ThreadGroup, Runnable)`) from a
// second, process-global counter in native-builtins, while the builders ran
// the JDK's: `a` and `b` below were both `Thread-0`, `c` was `Thread-1`, ...
// `--jdk-only` runs the JDK constructor and was already right.
//
// Run, each mode:
//   cratonvm L6W84ThreadDefaultNames
//   cratonvm --compatible L6W84ThreadDefaultNames
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from Thread.java)
// new Thread(r): Thread-0
// ofPlatform().unstarted(r): Thread-1
// new Thread(): Thread-2
// new Thread(group, r): Thread-3
// ofPlatform().factory().newThread(r): Thread-4
// new Thread(r, "named"): named
// ofVirtual().unstarted(r): ""
// new Thread(r) again: Thread-5
// all unnamed platform names distinct: true
// HOTSPOT_EXPECTED_END

import java.util.HashSet;
import java.util.List;

public class L6W84ThreadDefaultNames {
    public static void main(String[] args) {
        Runnable r = () -> {};
        Thread a = new Thread(r);
        Thread b = Thread.ofPlatform().unstarted(r);
        Thread c = new Thread();
        Thread d = new Thread(Thread.currentThread().getThreadGroup(), r);
        Thread e = Thread.ofPlatform().factory().newThread(r);
        Thread f = new Thread(r, "named");
        Thread v = Thread.ofVirtual().unstarted(r);
        Thread g = new Thread(r);
        System.out.println("new Thread(r): " + a.getName());
        System.out.println("ofPlatform().unstarted(r): " + b.getName());
        System.out.println("new Thread(): " + c.getName());
        System.out.println("new Thread(group, r): " + d.getName());
        System.out.println("ofPlatform().factory().newThread(r): " + e.getName());
        System.out.println("new Thread(r, \"named\"): " + f.getName());
        System.out.println("ofVirtual().unstarted(r): \"" + v.getName() + "\"");
        System.out.println("new Thread(r) again: " + g.getName());
        List<Thread> unnamed = List.of(a, b, c, d, e, g);
        HashSet<String> names = new HashSet<>();
        for (Thread t : unnamed) {
            names.add(t.getName());
        }
        System.out.println("all unnamed platform names distinct: " + (names.size() == unnamed.size()));
    }
}
