// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 107, lane L1: a JNI-attached thread's detach with
// a JDWP `ThreadDeath` request (`SUSPEND_EVENT_THREAD`) holds the thread
// before a joiner of its `Thread` wakes
// (docs/internal/fixed-bugs/interpreter-i106-L1-jni-detach-posts-thread-death-after-join-FIXED-20261016.md),
// the attached-thread twin of L1W106JdiThreadHeldAtThreadDeath.
//
// HotSpot's `jni_DetachCurrentThread` -> `JavaThread::exit`: `Thread.exit()`,
// `post_thread_end` (the back end suspends the event thread inside the
// callback), then `ensure_join`. CratonVM ran `ensure_join` before posting
// and never held the thread: `joined while attached held=true`. Fixed in
// vm/src/native/jni.rs `jni_detach_current_thread` /
// `post_attached_thread_life`. Not built or run.
//
// Needs the shim tools/probes/interp/L1/L1W107JvmtiDetachThreadEndBeforeJoin.c
// (its `Java_L1W107JdiAttachedThreadHeldAtThreadDeath_*` natives), passed as
// `-Dl1w107.lib=<path>`. NOT in run-jdi-conformance.sh's default list (the
// runner builds no library); after tools/jvmti/run-jvmti-shims.sh has built
// it into its --out directory:
//
//   tools/jdi/run-jdi-conformance.sh --jdk $JDK --cratonvm target/release/cratonvm //       --scenario L1W107JdiAttachedThreadHeldAtThreadDeath //       --debuggee-flag -Dl1w107.lib=/tmp/jvmti-shims/libL1W107JvmtiDetachThreadEndBeforeJoin.so
//
// Plain run (no debugger, with -Dl1w107.lib): stdout must equal HotSpot 25's:
//
//   attached joined
//   ended
//
// HotSpot 25 as the debuggee (derived from jni.cpp, javaThread.cpp and the
// back end's eventHandler.c, NOT measured):
//
//   == attach
//   == events
//     death l1w107-attached
//     joined while attached held=false
//   == end
//     joined after resume=true
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W107JdiAttachedThreadHeldAtThreadDeath {
    /** Set by the debugger: 1 = start, 3 = may end. */
    static volatile int phase;
    /** Set by main once `attached.join()` returned. */
    static volatile boolean joined;

    static native boolean startAttached();

    static native void awaitAttached();

    static final java.util.concurrent.CountDownLatch called = new java.util.concurrent.CountDownLatch(1);
    static volatile Thread attached;

    /** Called once on the JNI-attached thread, before it detaches. */
    static void fromNative() {
        attached = Thread.currentThread();
        called.countDown();
    }

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
        String lib = System.getProperty("l1w107.lib");
        if (lib == null) {
            System.out.println("no -Dl1w107.lib");
            return;
        }
        System.load(lib);
        if (!startAttached()) {
            System.out.println("startAttached failed");
            return;
        }
        called.await();
        Thread t = attached;
        t.join();
        joined = true;
        System.out.println("attached joined");
        awaitAttached();
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
            ClassType type = (ClassType) awaitClass("L1W107JdiAttachedThreadHeldAtThreadDeath");
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
                    fail("the attached thread's death never came");
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
                        if (n.equals("l1w107-attached")) {
                            System.out.println("  death l1w107-attached");
                            Thread.sleep(500);
                            boolean held = ((BooleanValue) type.getValue(joinedField)).value();
                            System.out.println("  joined while attached held=" + held);
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
                        fail("the debuggee ended before the attached thread's death");
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
