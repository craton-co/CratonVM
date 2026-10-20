// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L6 -- a platform thread constructed ON a
// virtual thread takes the virtual thread's group, priority and daemon status.
//
// JDK 25 java/lang/Thread.java master constructor (not attached):
//   if (g == null) g = parent.getThreadGroup();
//   int priority = Math.min(parent.getPriority(), g.getMaxPriority());
//   this.holder = new FieldHolder(g, task, stackSize, priority, parent.isDaemon());
// and a virtual thread's `getThreadGroup()` is `Thread.virtualThreadGroup()`,
// the group "VirtualThreads" (MAX_PRIORITY), its `getPriority()` NORM_PRIORITY,
// its `isDaemon()` true. So `new Thread(r)` on a virtual thread is a daemon in
// "VirtualThreads" at priority 5; its uncaught exception goes group ->
// parent groups -> the default handler.
//
// Before wave 84, `--compatible`'s constructor carrier
// (`populate_real_thread_holder_full`) read the group from the creator's
// `holder`, which a virtual thread does not have, and left it null:
// `getThreadGroup()` answered null and the uncaught exception never reached
// the default handler. `--jdk-only` runs the JDK constructor.
//
// Run, each mode:
//   cratonvm L6W84PlatformThreadFromVirtual
//   cratonvm --compatible L6W84PlatformThreadFromVirtual
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from Thread.java)
// creator group: VirtualThreads
// new Thread(r): group=VirtualThreads daemon=true priority=5
// new Thread(r, name): group=VirtualThreads daemon=true priority=5
// new Thread(null group, r, name): group=VirtualThreads daemon=true priority=5
// ofPlatform().unstarted(r): group=VirtualThreads daemon=true priority=5
// default handler saw: boom
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.List;

public class L6W84PlatformThreadFromVirtual {
    static String describe(Thread t) {
        ThreadGroup g = t.getThreadGroup();
        return "group=" + (g == null ? "null" : g.getName())
                + " daemon=" + t.isDaemon()
                + " priority=" + t.getPriority();
    }

    public static void main(String[] args) throws Exception {
        List<String> lines = new ArrayList<>();
        List<String> handled = new ArrayList<>();
        Thread.setDefaultUncaughtExceptionHandler((th, e) -> {
            synchronized (handled) {
                handled.add(e.getMessage());
            }
        });
        Runnable r = () -> {};
        Thread vt = Thread.ofVirtual().start(() -> {
            ThreadGroup cg = Thread.currentThread().getThreadGroup();
            lines.add("creator group: " + (cg == null ? "null" : cg.getName()));
            lines.add("new Thread(r): " + describe(new Thread(r)));
            lines.add("new Thread(r, name): " + describe(new Thread(r, "n1")));
            lines.add("new Thread(null group, r, name): "
                    + describe(new Thread((ThreadGroup) null, r, "n2")));
            lines.add("ofPlatform().unstarted(r): " + describe(Thread.ofPlatform().unstarted(r)));
            Thread boom = new Thread(() -> {
                throw new RuntimeException("boom");
            });
            boom.start();
            try {
                boom.join();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        vt.join();
        for (String line : lines) {
            System.out.println(line);
        }
        synchronized (handled) {
            System.out.println("default handler saw: " + String.join(",", handled));
        }
    }
}
