// Interpreter round i2 wave 54, lane L6: the frames of a thread suspended
// while it is blocked entering a `synchronized` method (and, as the control
// first and again last, a `synchronized` block).
//
// HotSpot lists the synchronized method itself on top -- its frame is built
// before the method's monitor is locked -- at its first line. CratonVM takes
// a synchronized method's monitor before it pushes the method's frame
// (`vm_exec::monitor_enter_synchronized_method`), and that path opened no
// inspection window and published no listing: `frame(0)` threw
// IndexOutOfBoundsException in section 1. Fixed in wave 54 (lane L6): the
// path opens the window, and the listing heads with the synchronized method
// at bytecode index 0 (`interpreter::synchronized_entry_head`, an opaque
// level). `CRATONVM_FRAME_TRACE=1` prints `[SYNC_ENTRY_HEAD] tid=<n>
// method=enterMethod(I)V` in section 1 (the positive control).
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W54JdiFramesOfASynchronizedMethodEntrant.java
//   cratonvm --java-home $JDK --jdwp-port 5798 -cp out L6W54JdiFramesOfASynchronizedMethodEntrant wait
//   java -cp out L6W54JdiFramesOfASynchronizedMethodEntrant debug 5798 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//   == section 0 (synchronized block)
//     frame 0: enterBlock line +1
//     frame 1: enter line +3
//     contended: java.lang.Object
//   == section 1 (synchronized method)
//     frame 0: enterMethod line +0
//     frame 1: enter line +1
//     contended: java.lang.Class
//   == section 2 (synchronized block)
//     frame 0: enterBlock line +1
//     frame 1: enter line +3
//     contended: java.lang.Object
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W54JdiFramesOfASynchronizedMethodEntrant {
    static final int SECTIONS = 3;
    static volatile int go;
    static volatile int ready;
    static volatile int release;
    static volatile int done;
    static volatile boolean holding;
    static final Object lockA = new Object();
    static final Object lockC = new Object();

    static Object lockOf(int section) {
        return section == 0 ? lockA : section == 1 ? L6W54JdiFramesOfASynchronizedMethodEntrant.class : lockC;
    }

    static void hold(int section) {
        synchronized (lockOf(section)) {
            holding = true;
            while (release < section + 1) {
                try {
                    Thread.sleep(5);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }
    }

    static int counter;

    static void enterBlock(int section) {
        synchronized (lockOf(section)) {
            counter++;
        }
    }

    static synchronized void enterMethod(int section) {
        counter += section;
    }

    static void enter(int section) {
        if (section == 1) {
            enterMethod(section);
        } else {
            enterBlock(section);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long deadline = System.currentTimeMillis() + 300_000;
        for (int s = 0; s < SECTIONS; s++) {
            final int section = s;
            while (debugged && go < section + 1 && System.currentTimeMillis() < deadline) {
                Thread.sleep(5);
            }
            holding = false;
            if (!debugged) {
                release = section + 1;
            }
            Thread holder = new Thread(() -> hold(section), "holder-" + section);
            holder.setDaemon(true);
            holder.start();
            while (!holding && release < section + 1) {
                Thread.sleep(1);
            }
            Thread blocked = new Thread(() -> enter(section), "blocked-" + section);
            blocked.setDaemon(true);
            blocked.start();
            ready = section + 1;
            holder.join(300_000);
            blocked.join(300_000);
            while (debugged && done < section + 1 && System.currentTimeMillis() < deadline) {
                Thread.sleep(5);
            }
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
        }

        static void set(String name, int value) throws Exception {
            type.setValue(type.fieldByName(name), vm.mirrorOf(value));
        }

        static ThreadReference thread(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
            }
            return null;
        }

        static String frame(ThreadReference t, int i) {
            try {
                Location at = t.frame(i).location();
                // The line relative to the method's first line, so the
                // transcript does not move with this file's header.
                return at.method().name() + " line +"
                        + (at.lineNumber() - at.method().location().lineNumber());
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        static void section(int s) throws Exception {
            System.out.println("== section " + s + (s == 1 ? " (synchronized method)" : " (synchronized block)"));
            set("go", s + 1);
            long until = System.currentTimeMillis() + 120_000;
            while (intField("ready") < s + 1) {
                if (System.currentTimeMillis() > until) {
                    fail("section " + s + " never started");
                }
                Thread.sleep(20);
            }
            ThreadReference blocked = thread("blocked-" + s);
            if (blocked == null) {
                fail("no blocked thread");
            }
            while (blocked.status() != ThreadReference.THREAD_STATUS_MONITOR) {
                if (System.currentTimeMillis() > until) {
                    fail("the blocked thread never blocked");
                }
                Thread.sleep(5);
            }
            blocked.suspend();
            System.out.println("  frame 0: " + frame(blocked, 0));
            System.out.println("  frame 1: " + frame(blocked, 1));
            ObjectReference contended = blocked.currentContendedMonitor();
            System.out.println("  contended: " + (contended == null ? "null" : contended.referenceType().name()));
            ThreadReference holder = thread("holder-" + s);
            blocked.resume();
            set("release", s + 1);
            until = System.currentTimeMillis() + 10_000;
            while ((blocked.status() != ThreadReference.THREAD_STATUS_ZOMBIE
                    || (holder != null && holder.status() != ThreadReference.THREAD_STATUS_ZOMBIE))
                    && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            set("done", s + 1);
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
            type = (ClassType) awaitClass("L6W54JdiFramesOfASynchronizedMethodEntrant");
            for (int s = 0; s < SECTIONS; s++) {
                section(s);
            }
            System.out.println("== end");
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
                    try {
                        next.resume();
                    } catch (VMDisconnectedException gone) {
                        break;
                    }
                }
            }
            System.out.println("  disconnected");
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
