// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i66-L1-second-exit-caller-reads-waiting-not-blocked-FIXED-20261111.md:
// while `main` runs shutdown (System.exit(0) -> the hook below), a second
// thread calling System.exit(2) blocks for good. HotSpot: it blocks entering
// `synchronized (Shutdown.class)` in Shutdown.exit -- BLOCKED, JMX lock
// java.lang.Class, owner `main`. The process then exits with main's status 0.
//
// Run, each mode (the --compatible native exit needs the wave-79 switch):
//   cratonvm L6W79SecondExitCaller; echo rc=$?
//   CRATONVM_COMPAT_EXIT_BLOCKS_ON_SHUTDOWN=1 cratonvm --compatible L6W79SecondExitCaller; echo rc=$?
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// second exit caller: BLOCKED jmx=BLOCKED lock=java.lang.Class owner=main
// hook done
// HOTSPOT_EXPECTED_END
//
// The exit status is main's: `rc=0` from the echo (not part of the block).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;

public class L6W79SecondExitCaller {
    public static void main(String[] args) {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            Thread second = new Thread(() -> System.exit(2), "w79-second-exit");
            second.setDaemon(true);
            second.start();
            long deadline = System.nanoTime() + 5_000_000_000L;
            while (second.getState() == Thread.State.NEW
                    || second.getState() == Thread.State.RUNNABLE) {
                if (System.nanoTime() > deadline) {
                    break;
                }
                try {
                    Thread.sleep(5);
                } catch (InterruptedException e) {
                    break;
                }
            }
            try {
                Thread.sleep(100);
            } catch (InterruptedException e) {
                // ignore
            }
            ThreadInfo info = mx.getThreadInfo(second.threadId());
            String lock = info == null || info.getLockName() == null ? "null" : info.getLockName();
            int at = lock.indexOf('@');
            System.out.println("second exit caller: " + second.getState()
                    + " jmx=" + (info == null ? "null" : info.getThreadState())
                    + " lock=" + (at < 0 ? lock : lock.substring(0, at))
                    + " owner=" + (info == null ? "null" : info.getLockOwnerName()));
            System.out.println("hook done");
        }, "w79-hook"));
        System.exit(0);
    }
}
