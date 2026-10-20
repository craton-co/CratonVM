// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i66-orch-reference-handler-thread-is-absent-FIXED-20261111.md:
// JDK 25's System.initPhase1 ends with
// SharedSecrets.getJavaLangRefAccess().startThreads(), which starts the
// `Reference Handler` (max priority) and `Finalizer` (max - 2) daemons in the
// `system` group. HotSpot initialises java.lang.ref.Finalizer (so Reference,
// whose <clinit> publishes the access object) before initPhase1.
//
// Run, each mode, with and without the wave-79 switch:
//   cratonvm L6W79SystemThreads
//   CRATONVM_INIT_FINALIZER_BEFORE_PHASE1=1 cratonvm L6W79SystemThreads
//   cratonvm --compatible L6W79SystemThreads
//   CRATONVM_INIT_FINALIZER_BEFORE_PHASE1=1 cratonvm --compatible L6W79SystemThreads
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// Reference Handler: daemon=true priority=10 group=system state=RUNNABLE
// Finalizer: daemon=true priority=8 group=system state=WAITING
// done
// HOTSPOT_EXPECTED_END
public class L6W79SystemThreads {
    static Thread named(String name) {
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t.getName().equals(name)) {
                return t;
            }
        }
        return null;
    }

    static void row(String name) throws InterruptedException {
        Thread t = named(name);
        if (t == null) {
            System.out.println(name + ": absent");
            return;
        }
        // Settled: both daemons are idle once startup's references are drained.
        Thread.State s = t.getState();
        for (int i = 0; i < 100 && s == Thread.State.RUNNABLE && name.equals("Finalizer"); i++) {
            Thread.sleep(10);
            s = t.getState();
        }
        ThreadGroup g = t.getThreadGroup();
        System.out.println(name + ": daemon=" + t.isDaemon() + " priority=" + t.getPriority()
                + " group=" + (g == null ? "null" : g.getName()) + " state=" + s);
    }

    public static void main(String[] args) throws Exception {
        Thread.sleep(100);
        row("Reference Handler");
        row("Finalizer");
        System.out.println("done");
    }
}
