// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L6 -- the thread list after `main`
// returns, while the VM waits for a non-daemon thread
// (`CRATONVM_DESTROY_JAVA_VM_THREAD`, default on, `=0` kill switch; needs
// `CRATONVM_MAIN_THREAD_ENDS_BEFORE_DESTROY`, default on).
//
// HotSpot 25: `JavaMain`'s `LEAVE()` detaches `main`, then `DestroyJavaVM`
// attaches the same OS thread anew as a thread named `DestroyJavaVM` (main
// group, non-daemon, RUNNABLE, no Java frames) and waits there. So
// `Thread.getAllStackTraces()` and `ThreadMXBean` list `DestroyJavaVM`, not
// `main`, and the shutdown hook sees `DestroyJavaVM` as the thread that
// started it (its parent: `currentThread()` of `Shutdown.runHooks`).
// CratonVM (wave 101) listed neither.
//
// Run: java -cp out L6W102DestroyJavaVmListed
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// main returning
// main listed: false
// DestroyJavaVM listed: true
// DestroyJavaVM state: RUNNABLE
// DestroyJavaVM daemon: false
// DestroyJavaVM group: main
// DestroyJavaVM frames: 0
// DestroyJavaVM id above main's: true
// DestroyJavaVM in ThreadMXBean: true
// old main alive: false
// hook group: main
// HOTSPOT_EXPECTED_END

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.util.Map;

public class L6W102DestroyJavaVmListed {
    public static void main(String[] args) throws Exception {
        Thread main = Thread.currentThread();
        long mainId = main.threadId();
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            ThreadGroup g = Thread.currentThread().getThreadGroup();
            System.out.println("hook group: " + (g == null ? null : g.getName()));
        }, "hook"));
        Thread watcher = new Thread(() -> {
            try {
                main.join(10_000);
                Thread.sleep(200);
            } catch (InterruptedException e) {
                return;
            }
            Map<Thread, StackTraceElement[]> all = Thread.getAllStackTraces();
            Thread destroy = null;
            boolean mainListed = false;
            for (Map.Entry<Thread, StackTraceElement[]> e : all.entrySet()) {
                if (e.getKey() == main || "main".equals(e.getKey().getName())) {
                    mainListed = true;
                }
                if ("DestroyJavaVM".equals(e.getKey().getName())) {
                    destroy = e.getKey();
                }
            }
            System.out.println("main listed: " + mainListed);
            System.out.println("DestroyJavaVM listed: " + (destroy != null));
            if (destroy != null) {
                System.out.println("DestroyJavaVM state: " + destroy.getState());
                System.out.println("DestroyJavaVM daemon: " + destroy.isDaemon());
                ThreadGroup g = destroy.getThreadGroup();
                System.out.println("DestroyJavaVM group: " + (g == null ? null : g.getName()));
                System.out.println("DestroyJavaVM frames: " + all.get(destroy).length);
                System.out.println("DestroyJavaVM id above main's: " + (destroy.threadId() > mainId));
                boolean inMx = false;
                for (ThreadInfo ti : ManagementFactory.getThreadMXBean()
                        .getThreadInfo(ManagementFactory.getThreadMXBean().getAllThreadIds())) {
                    if (ti != null && "DestroyJavaVM".equals(ti.getThreadName())
                            && ti.getThreadId() == destroy.threadId()) {
                        inMx = true;
                    }
                }
                System.out.println("DestroyJavaVM in ThreadMXBean: " + inMx);
            }
            System.out.println("old main alive: " + main.isAlive());
        }, "watcher");
        watcher.start();
        Thread.sleep(100);
        System.out.println("main returning");
    }
}
