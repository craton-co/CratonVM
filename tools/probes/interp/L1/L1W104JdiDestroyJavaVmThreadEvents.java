// Interpreter round i5 wave 104, lane L1: JDWP `ThreadDeath` for `main` and
// `ThreadStart` for `DestroyJavaVM` when `main` returns while a non-daemon
// thread still runs.
//
// HotSpot's launcher detaches `main` (`JavaThread::exit`: JVMTI `ThreadEnd`,
// JDWP `ThreadDeath`) and then attaches the same OS thread anew as
// `DestroyJavaVM` (`attach_current_thread`: JVMTI and JDWP `ThreadStart`),
// which waits for the last non-daemon thread. The two are two `Thread`
// objects, so two JDWP thread ids: after the attach `main`'s reference is a
// ZOMBIE that `allThreads()` no longer lists, and `DestroyJavaVM` is a new
// reference. CratonVM posted neither event and kept one id for both (both
// mirrors live on the launcher's registry entry 0,
// `vm_exec::list_destroy_java_vm_thread`). Since wave 104 the server gives
// them their own ids (vm/src/debug/ids.rs `DESTROY_JAVA_VM_WIRE_ID`,
// vm/src/debug/mod.rs `note_main_thread_ended`,
// `note_destroy_java_vm_attached`; `CRATONVM_DEBUG_DESTROY_JAVA_VM_IDENTITY`,
// default on). Not built or run. Page:
// docs/internal/fixed-bugs/interpreter-i101-L6-main-thread-stays-alive-during-destroy-java-vm-FIXED-20261016.md
//
// `main` starts a non-daemon `lingerer`, waits until its `run()` began (so
// its `ThreadStart` was posted first), and returns; `lingerer` sleeps 1.5 s
// and ends. The requests suspend the event thread, so each line is read
// while the thread it names is held.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   main returns
//   lingerer ends
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W104JdiDestroyJavaVmThreadEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5800 -cp out L1W104JdiDestroyJavaVmThreadEvents wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5800 \
//       -cp out L1W104JdiDestroyJavaVmThreadEvents wait
//   java -cp out L1W104JdiDestroyJavaVmThreadEvents debug 5800 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from java.c, jni.cpp and the back
// end's eventHandler.c, NOT measured):
//
//   == attach
//     main listed=true
//   == events
//     start lingerer
//     death main same=true
//     start DestroyJavaVM same=false
//     main status=ZOMBIE listed=false DestroyJavaVM listed=true
//     death lingerer
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.concurrent.CountDownLatch;

public class L1W104JdiDestroyJavaVmThreadEvents {
    /** Set by the debugger: 1 = start. */
    static volatile int phase;

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
        CountDownLatch running = new CountDownLatch(1);
        Thread lingerer = new Thread(() -> {
            running.countDown();
            try {
                Thread.sleep(1500);
            } catch (InterruptedException ignored) {
                // Ends early.
            }
            System.out.println("lingerer ends");
        }, "lingerer");
        lingerer.start();
        running.await();
        System.out.println("main returns");
    }

    static final class Debugger {
        static VirtualMachine vm;

        static boolean listed(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return true;
                }
            }
            return false;
        }

        /** The thread's name; "" once the VM is gone or the thread collected. */
        static String nameOf(ThreadReference t) {
            try {
                return t.name();
            } catch (VMDisconnectedException | ObjectCollectedException gone) {
                return "";
            }
        }

        static String status(ThreadReference t) {
            try {
                return switch (t.status()) {
                    case ThreadReference.THREAD_STATUS_ZOMBIE -> "ZOMBIE";
                    case ThreadReference.THREAD_STATUS_RUNNING -> "RUNNING";
                    case ThreadReference.THREAD_STATUS_SLEEPING -> "SLEEPING";
                    case ThreadReference.THREAD_STATUS_MONITOR -> "MONITOR";
                    case ThreadReference.THREAD_STATUS_WAIT -> "WAIT";
                    case ThreadReference.THREAD_STATUS_NOT_STARTED -> "NOT_STARTED";
                    default -> "UNKNOWN";
                };
            } catch (ObjectCollectedException gone) {
                return "collected";
            }
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
            ClassType type = (ClassType) awaitClass("L1W104JdiDestroyJavaVmThreadEvents");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            System.out.println("  main listed=" + (main != null));
            if (main == null) {
                fail("no main thread");
            }
            EventRequestManager erm = vm.eventRequestManager();
            ThreadStartRequest starts = erm.createThreadStartRequest();
            starts.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            starts.enable();
            ThreadDeathRequest deaths = erm.createThreadDeathRequest();
            deaths.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            deaths.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            System.out.println("== events");
            long until = System.currentTimeMillis() + 120_000;
            try {
                loop(main, starts, deaths, until);
            } catch (VMDisconnectedException gone) {
                // The debuggee ended while an event was handled: the end.
            }
            System.out.println("== end");
            System.out.println("  disconnected");
        }

        static void loop(ThreadReference main, ThreadStartRequest starts,
                ThreadDeathRequest deaths, long until) throws Exception {
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (next == null || System.currentTimeMillis() > until) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    // The VM may be gone by the time a late event's thread is
                    // asked its name (HotSpot posts `ThreadDeath` for
                    // `DestroyJavaVM` as it dies): that is the end.
                    if (e instanceof ThreadStartEvent s) {
                        String n = nameOf(s.thread());
                        if (n.equals("lingerer")) {
                            System.out.println("  start lingerer");
                        } else if (n.equals("DestroyJavaVM")) {
                            System.out.println("  start DestroyJavaVM same=" + s.thread().equals(main));
                            System.out.println("  main status=" + status(main)
                                    + " listed=" + listed("main")
                                    + " DestroyJavaVM listed=" + listed("DestroyJavaVM"));
                        }
                    } else if (e instanceof ThreadDeathEvent d) {
                        String n = nameOf(d.thread());
                        if (n.equals("main")) {
                            System.out.println("  death main same=" + d.thread().equals(main));
                        } else if (n.equals("lingerer")) {
                            System.out.println("  death lingerer");
                        }
                    } else if (e instanceof VMDeathEvent) {
                        starts.disable();
                        deaths.disable();
                    } else if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    try {
                        next.resume();
                    } catch (VMDisconnectedException gone) {
                        break;
                    }
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
