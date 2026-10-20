// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i77-L6-debugger-interrupt-writes-the-interrupted-field-after-the-flag-FIXED-20261111.md:
// a JDWP ThreadReference.Interrupt of a thread sleeping in Thread.sleep(60 s),
// once with the VM running (the mutator server) and once with the VM
// suspended, then resumed (the quiesced server). HotSpot's
// JavaThread::interrupt writes Thread.interrupted once, before the wake, so
// the sleeper's InterruptedException consumes it: inside the catch
// isInterrupted() is false and Thread.interrupted() is false (no phantom
// second interrupt).
//
// Before the fix the mutator path wrote the field a second time, after the
// wake (a race: in-catch=true / after-interrupted=true when the late write
// lands after the sleeper's clear), and the quiesced path never wrote it.
//
// Experimental-debug build only. Run:
//
//   javac -g -d out L6W79JdiInterruptField.java
//   cratonvm --java-home $JDK --jdwp-port 5809 -cp out L6W79JdiInterruptField wait
//   java -cp out L6W79JdiInterruptField debug 5809
//
// HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5809 -cp out L6W79JdiInterruptField wait
//
// The debuggee prints `ended`. The debugger's transcript:
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// == attach
//   running: threw=true in-catch=false after-interrupted=false
//   suspended: threw=true in-catch=false after-interrupted=false
// == end
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import java.util.*;

public class L6W79JdiInterruptField {
    /** Set by the debugger: 1 = start, 3 = done. */
    static volatile int phase;
    /** Sleepers inside their sleep so far (approximately: set right before it). */
    static volatile int started;
    static volatile String running;
    static volatile String suspended;

    static String sleepOnce() {
        try {
            Thread.sleep(60_000);
            return "threw=false";
        } catch (InterruptedException e) {
            boolean inCatch = Thread.currentThread().isInterrupted();
            boolean after = Thread.interrupted();
            return "threw=true in-catch=" + inCatch + " after-interrupted=" + after;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length >= 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length >= 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        if (debugged) {
            Thread a = new Thread(() -> {
                synchronized (L6W79JdiInterruptField.class) {
                    started++;
                }
                running = sleepOnce();
            }, "w79-sleeper-running");
            Thread b = new Thread(() -> {
                synchronized (L6W79JdiInterruptField.class) {
                    started++;
                }
                suspended = sleepOnce();
            }, "w79-sleeper-suspended");
            a.setDaemon(true);
            b.setDaemon(true);
            a.start();
            b.start();
            while (phase != 3 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static Value field(String name) {
            return type.getValue(type.fieldByName(name));
        }

        static String awaitString(String name) throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (true) {
                Value v = field(name);
                if (v instanceof StringReference s) {
                    return s.value();
                }
                if (System.currentTimeMillis() > until) {
                    return "no answer within 30 s";
                }
                Thread.sleep(10);
            }
        }

        static ThreadReference thread(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
            }
            fail("no thread " + name);
            return null;
        }

        /** Waits until `t` reads SLEEPING (it is inside its sleep). */
        static void awaitSleeping(ThreadReference t) throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (t.status() != ThreadReference.THREAD_STATUS_SLEEPING) {
                if (System.currentTimeMillis() > until) {
                    fail(t.name() + " never slept (status " + t.status() + ")");
                }
                Thread.sleep(10);
            }
            Thread.sleep(50);
        }

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
            type = (ClassType) awaitClass("L6W79JdiInterruptField");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 60_000;
            while (((IntegerValue) field("started")).value() < 2) {
                if (System.currentTimeMillis() > until) {
                    fail("sleepers did not start");
                }
                Thread.sleep(10);
            }

            // The mutator server: the VM runs.
            ThreadReference r = thread("w79-sleeper-running");
            awaitSleeping(r);
            r.interrupt();
            System.out.println("  running: " + awaitString("running"));

            // The quiesced server: the whole VM is suspended, interrupted, resumed.
            ThreadReference s = thread("w79-sleeper-suspended");
            awaitSleeping(s);
            vm.suspend();
            s.interrupt();
            vm.resume();
            System.out.println("  suspended: " + awaitString("suspended"));

            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            System.out.println("== end");
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
