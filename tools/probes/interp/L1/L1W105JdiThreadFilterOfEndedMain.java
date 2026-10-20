// Interpreter round i5 wave 105, lane L1: a `ThreadOnly` filter naming
// `main` matches none of `DestroyJavaVM`'s events once `main` has ended.
//
// HotSpot's launcher detaches `main` and attaches the same OS thread anew as
// `DestroyJavaVM`, a different `Thread`; a JDWP `ThreadOnly` filter holds
// `main`'s thread object (eventFilter.c `matchThread`: `isSameObject`), so a
// request filtered on `main` reports nothing `DestroyJavaVM` does: not its
// `ThreadStart`, not the shutdown sequence it runs (`java.lang.Shutdown`).
// CratonVM keeps both on registry entry 0 and, since wave 104, gives them
// their own JDWP ids; but a filter set before `main` ended held VM thread 0
// and went on matching `DestroyJavaVM` (the residue in
// docs/internal/fixed-bugs/interpreter-i101-L6-main-thread-stays-alive-during-destroy-java-vm-FIXED-20261016.md).
// Wave 105 renames the filters at `main`'s end
// (vm/src/debug/events.rs `EventManager::rename_thread_filters`, called by
// vm/src/debug/mod.rs `note_main_thread_ended`). Not built or run.
//
// `main` starts a non-daemon `lingerer`, waits until its `run()` began, and
// returns; `lingerer` sleeps 1.5 s and ends. The unfiltered requests suspend
// the event thread; the two `main`-filtered ones suspend nothing and must
// stay silent after `main`'s death.
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
//   javac -g -d out L1W105JdiThreadFilterOfEndedMain.java
//   cratonvm --java-home $JDK --jdwp-port 5801 -cp out L1W105JdiThreadFilterOfEndedMain wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5801 \
//       -cp out L1W105JdiThreadFilterOfEndedMain wait
//   java -cp out L1W105JdiThreadFilterOfEndedMain debug 5801 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from eventFilter.c and java.c, NOT
// measured):
//
//   == attach
//     main listed=true
//   == events
//     start lingerer
//     death main
//     start DestroyJavaVM
//     death lingerer
//     filtered on main: starts=0 shutdown entries=0
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.concurrent.CountDownLatch;

public class L1W105JdiThreadFilterOfEndedMain {
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
        static int filteredStarts;
        static int filteredEntries;
        static boolean mainDied;
        static boolean reported;

        static String nameOf(ThreadReference t) {
            try {
                return t.name();
            } catch (VMDisconnectedException | ObjectCollectedException gone) {
                return "";
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
            ClassType type = (ClassType) awaitClass("L1W105JdiThreadFilterOfEndedMain");
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
            ThreadStartRequest mainStarts = erm.createThreadStartRequest();
            mainStarts.addThreadFilter(main);
            mainStarts.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            mainStarts.enable();
            MethodEntryRequest mainEntries = erm.createMethodEntryRequest();
            mainEntries.addThreadFilter(main);
            mainEntries.addClassFilter("java.lang.Shutdown");
            mainEntries.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            mainEntries.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            System.out.println("== events");
            long until = System.currentTimeMillis() + 120_000;
            try {
                loop(starts, deaths, mainStarts, mainEntries, until);
            } catch (VMDisconnectedException gone) {
                // The debuggee ended while an event was handled: the end.
            }
            report();
            System.out.println("== end");
            System.out.println("  disconnected");
        }

        static void report() {
            if (!reported) {
                reported = true;
                System.out.println("  filtered on main: starts=" + filteredStarts
                        + " shutdown entries=" + filteredEntries);
            }
        }

        static void loop(ThreadStartRequest starts, ThreadDeathRequest deaths,
                ThreadStartRequest mainStarts, MethodEntryRequest mainEntries, long until)
                throws Exception {
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
                    EventRequest by = e.request();
                    if (by == mainStarts) {
                        // `main` itself started before the request: any hit
                        // here is another thread's start.
                        filteredStarts++;
                    } else if (by == mainEntries) {
                        // Counted only once `main` died: `main` never runs
                        // `java.lang.Shutdown` on HotSpot either way.
                        if (mainDied) {
                            filteredEntries++;
                        }
                    } else if (e instanceof ThreadStartEvent s) {
                        String n = nameOf(s.thread());
                        if (n.equals("lingerer") || n.equals("DestroyJavaVM")) {
                            System.out.println("  start " + n);
                        }
                    } else if (e instanceof ThreadDeathEvent d) {
                        String n = nameOf(d.thread());
                        if (n.equals("main")) {
                            mainDied = true;
                            System.out.println("  death main");
                        } else if (n.equals("lingerer")) {
                            System.out.println("  death lingerer");
                        }
                    } else if (e instanceof VMDeathEvent) {
                        report();
                        starts.disable();
                        deaths.disable();
                        mainStarts.disable();
                        mainEntries.disable();
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
