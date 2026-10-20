// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 105, lane L6: a `ThreadDeath` of `main` with
// `SUSPEND_EVENT_THREAD` holds `main` in its exit until the debugger resumes
// it (page `i101-L6-main-thread-stays-alive-during-destroy-java-vm`, residue).
//
// HotSpot's `JavaThread::exit` posts JVMTI `ThreadEnd` (the back end's
// `ThreadDeath`, which suspends the event thread inside the callback) BEFORE
// `ensure_join` (TERMINATED, `notifyAll` on the mirror), so a thread blocked
// in `mainThread.join()` stays blocked while `main` is held. CratonVM posted
// the event and went on: the join returned while the debugger still held the
// event set (`vm/src/vm/vm_exec.rs` `end_main_thread_java_side` now waits on
// `ENDED_MAIN_VM_ID`'s suspension). Not built or run.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   main returns
//   joiner: main joined
//
// Under a debugger (add to tools/jdi/run-jdi-conformance.sh's list, or):
//
//   javac -g -d out L6W105JdiMainHeldAtThreadDeath.java
//   cratonvm --java-home $JDK --jdwp-port 5800 -cp out L6W105JdiMainHeldAtThreadDeath wait
//   java -cp out L6W105JdiMainHeldAtThreadDeath debug 5800 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from javaThread.cpp and the back end's
// eventHandler.c, NOT measured):
//
//   == attach
//   == events
//     death main
//     joined while main held=false
//   == end
//     joined after resume=true
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.concurrent.CountDownLatch;

public class L6W105JdiMainHeldAtThreadDeath {
    /** Set by the debugger: 1 = start. */
    static volatile int phase;
    /** Set by the joiner once `mainThread.join()` returned. */
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
        Thread mainThread = Thread.currentThread();
        CountDownLatch running = new CountDownLatch(1);
        Thread joiner = new Thread(() -> {
            running.countDown();
            try {
                mainThread.join();
                joined = true;
                // Keep the VM up a little so the debugger can read the flag.
                Thread.sleep(debugged ? 1500 : 0);
            } catch (InterruptedException ignored) {
                // Ends early.
            }
            System.out.println("joiner: main joined");
        }, "joiner");
        joiner.start();
        running.await();
        System.out.println("main returns");
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
            ClassType type = (ClassType) awaitClass("L6W105JdiMainHeldAtThreadDeath");
            EventRequestManager erm = vm.eventRequestManager();
            ThreadDeathRequest deaths = erm.createThreadDeathRequest();
            deaths.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            deaths.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            System.out.println("== events");
            Field joinedField = type.fieldByName("joined");
            boolean afterResume = false;
            long until = System.currentTimeMillis() + 120_000;
            try {
                boolean done = false;
                while (!done) {
                    EventSet next = vm.eventQueue().remove(60_000);
                    if (next == null || System.currentTimeMillis() > until) {
                        fail("the debuggee did not end");
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
                            if (n.equals("main")) {
                                System.out.println("  death main");
                                Thread.sleep(500);
                                boolean held = ((BooleanValue) type.getValue(joinedField)).value();
                                System.out.println("  joined while main held=" + held);
                                deaths.disable();
                                next.resume();
                                resume = false;
                                Thread.sleep(500);
                                afterResume = ((BooleanValue) type.getValue(joinedField)).value();
                                done = true;
                            }
                        } else if (e instanceof VMDisconnectEvent) {
                            done = true;
                            resume = false;
                        }
                    }
                    if (resume) {
                        next.resume();
                    }
                }
            } catch (VMDisconnectedException gone) {
                // The debuggee ended early.
            }
            System.out.println("== end");
            System.out.println("  joined after resume=" + afterResume);
            try {
                vm.dispose();
            } catch (RuntimeException ignored) {
                // Already gone.
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
