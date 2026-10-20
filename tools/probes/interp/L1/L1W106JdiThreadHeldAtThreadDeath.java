// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L1: a `ThreadDeath` with
// `SUSPEND_EVENT_THREAD` holds an ordinary platform thread in its exit until
// the debugger resumes it, as lane L6's wave-105 fix holds `main`
// (L6W105JdiMainHeldAtThreadDeath).
//
// HotSpot's `JavaThread::exit` posts JVMTI `ThreadEnd` (the back end's
// `ThreadDeath`, which suspends the event thread inside the callback) BEFORE
// `ensure_join` (TERMINATED, `notifyAll` on the thread object), so another
// thread's `worker.join()` stays blocked while the worker is held. CratonVM
// counted the suspension and went on to the termination, so the join
// returned while the debugger still held the event set. Fix:
// `vm/src/vm/vm_exec.rs` (the thread-start closure, after the `ThreadDeath`)
// waits in `debug::hold_ending_thread_while_suspended`. Not built or run.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   worker joined
//   ended
//
// Under a debugger (in tools/jdi/run-jdi-conformance.sh's default list):
//
//   javac -g -d out L1W106JdiThreadHeldAtThreadDeath.java
//   cratonvm --java-home $JDK --jdwp-port 5801 -cp out L1W106JdiThreadHeldAtThreadDeath wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5801 \
//       -cp out L1W106JdiThreadHeldAtThreadDeath wait
//   java -cp out L1W106JdiThreadHeldAtThreadDeath debug 5801 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from javaThread.cpp and the back end's
// eventHandler.c, NOT measured):
//
//   == attach
//   == events
//     death worker
//     joined while worker held=false
//   == end
//     joined after resume=true
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W106JdiThreadHeldAtThreadDeath {
    /** Set by the debugger: 1 = start, 3 = may end. */
    static volatile int phase;
    /** Set by main once `worker.join()` returned. */
    static volatile boolean joined;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Thread worker = new Thread(() -> {
            // Ends at once.
        }, "worker");
        worker.start();
        worker.join();
        joined = true;
        System.out.println("worker joined");
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;

        static void run(int port) throws Exception {
            AttachingConnector socket = null;
            for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.name().equals("com.sun.jdi.SocketAttach")) {
                    socket = c;
                }
            }
            Map<String, Connector.Argument> a = socket.defaultArguments();
            a.get("hostname").setValue("localhost");
            a.get("port").setValue(Integer.toString(port));
            a.get("timeout").setValue("60000");
            vm = attach(socket, a);
            System.out.println("== attach");
            ClassType type = (ClassType) awaitClass("L1W106JdiThreadHeldAtThreadDeath");
            EventRequestManager erm = vm.eventRequestManager();
            ThreadDeathRequest deaths = erm.createThreadDeathRequest();
            deaths.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            deaths.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            System.out.println("== events");
            Field joinedField = type.fieldByName("joined");
            boolean afterResume = false;
            long until = System.currentTimeMillis() + 120_000;
            boolean done = false;
            while (!done) {
                EventSet next = vm.eventQueue().remove(60_000);
                if (next == null || System.currentTimeMillis() > until) {
                    fail("the worker's death never came");
                }
                boolean resume = true;
                for (Event e : next) {
                    if (e instanceof ThreadDeathEvent d) {
                        String n;
                        try {
                            n = d.thread().name();
                        } catch (ObjectCollectedException gone) {
                            n = "";
                        }
                        if (n.equals("worker")) {
                            System.out.println("  death worker");
                            Thread.sleep(500);
                            boolean held = ((BooleanValue) type.getValue(joinedField)).value();
                            System.out.println("  joined while worker held=" + held);
                            deaths.disable();
                            next.resume();
                            resume = false;
                            long settle = System.currentTimeMillis() + 30_000;
                            while (!afterResume && System.currentTimeMillis() < settle) {
                                Thread.sleep(50);
                                afterResume = ((BooleanValue) type.getValue(joinedField)).value();
                            }
                            done = true;
                        }
                    } else if (e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended before the worker's death");
                    }
                }
                if (resume) {
                    next.resume();
                }
            }
            System.out.println("== end");
            System.out.println("  joined after resume=" + afterResume);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    next.resume();
                }
            }
        }

        static ReferenceType awaitClass(String name) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                List<ReferenceType> found = vm.classesByName(name);
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    return found.get(0);
                }
                if (System.currentTimeMillis() > until) {
                    fail("class " + name + " never loaded");
                }
                Thread.sleep(50);
            }
        }

        static VirtualMachine attach(AttachingConnector socket, Map<String, Connector.Argument> a)
                throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                try {
                    return socket.attach(a);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > until) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
        }

        static void fail(String why) {
            System.out.println("FAILED: " + why);
            try {
                vm.dispose();
            } catch (RuntimeException ignored) {
                // Already gone.
            }
            System.exit(2);
        }
    }
}
