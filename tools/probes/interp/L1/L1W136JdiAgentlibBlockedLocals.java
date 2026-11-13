// Interpreter round i7 wave 136, lane L1: under `-agentlib:jdwp` (what every
// IDE and JDI's launching connector use) a thread that parked BEFORE the
// debugger attached lists its frames, and the liveness-dead `blocker`
// argument of `LockSupport.park(Object)` reads the blocker, as under
// `--jdwp-port`. Page:
// docs/feature-designs/jvmti-delivery-threading.md
//
// `-agentlib:jdwp` is mapped onto the built-in server as
// `VmConfig::jdwp_agent`, but `jvmti::debugger_observes_locals` (every local
// a GC root; compiles keep dead locals; a blocked thread's dead slot read
// through its tracked address) and `interpreter::open_blocked_inspection`
// (a thread that blocks before a session exists keeps an inspection window)
// asked `jdwp_port` alone. Fixed behind `CRATONVM_JDWP_AGENT_DEBUGGER_FACTS`
// (parity, default ON, `=0` off).
//
// Canonical transcript: no ids, no addresses, no timings.
//
//   javac -g -d out L1W136JdiAgentlibBlockedLocals.java
//   <debuggee> -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5793 -cp out L1W136JdiAgentlibBlockedLocals wait
//   java -cp out L1W136JdiAgentlibBlockedLocals debug 5793 > transcript.txt
//
// with <debuggee> one of `java`, `java -Xint`, `cratonvm [--compatible] [--nojit]`
// (a debug `cratonvm`, `experimental-debug`). Plain run (no debugger): `ended`.
//
// Prediction for CratonVM before wave 136 (and with
// CRATONVM_JDWP_AGENT_DEBUGGER_FACTS=0): `frames listed: false` (no window,
// the wave-41 symptom) or, if the frames are listed, `args: [null]` (the
// wave-126 symptom). Positive control: the same debuggee with
// `--jdwp-port 5793` in place of `-agentlib:jdwp=...` prints HotSpot's block.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, default flags and -Xint identical)
// == attach
//   status wait: true
//   frames listed: true
//   frame 0: jdk.internal.misc.Unsafe.park native=true
//   frame 1: java.util.concurrent.locks.LockSupport.park native=false
//   frame 2: L1W136JdiAgentlibBlockedLocals.parkHere native=false
//   args: [L1W136JdiAgentlibBlockedLocals$Blocker]
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L1W136JdiAgentlibBlockedLocals {
    static final class Blocker {}

    /** 2 once the parker is about to park (set by the parker); 3 = done (set by the debugger). */
    static volatile int phase;
    static volatile boolean stop;
    static volatile Object sink;

    static void parkHere() {
        Blocker blocker = new Blocker();
        while (!stop) {
            phase = Math.max(phase, 2);
            LockSupport.park(blocker);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        // The parker parks at once, before any debugger can attach.
        Thread parker = new Thread(L1W136JdiAgentlibBlockedLocals::parkHere, "parker");
        parker.setDaemon(true);
        parker.start();
        long until = System.currentTimeMillis() + 300_000;
        while (phase < 2 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        // Collections while it is parked: its frames' references move.
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 20_000; i++) {
                sink = new byte[64];
            }
            System.gc();
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        stop = true;
        LockSupport.unpark(parker);
        parker.join(10_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

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
            type = (ClassType) awaitClass("L1W136JdiAgentlibBlockedLocals");
            long until = System.currentTimeMillis() + 120_000;
            ThreadReference parker = null;
            while (parker == null) {
                if (System.currentTimeMillis() > until) {
                    fail("the parker never parked");
                }
                Thread.sleep(20);
                IntegerValue ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
                if (ready.value() < 2) {
                    continue;
                }
                for (ThreadReference t : vm.allThreads()) {
                    if (t.name().equals("parker") && t.status() == ThreadReference.THREAD_STATUS_WAIT) {
                        parker = t;
                    }
                }
            }
            // Let the debuggee's collections run first.
            Thread.sleep(1_000);
            parker.suspend();
            System.out.println("  status wait: "
                    + (parker.status() == ThreadReference.THREAD_STATUS_WAIT));
            int count = parker.frameCount();
            System.out.println("  frames listed: " + (count >= 3));
            if (count >= 3) {
                for (int i = 0; i < 3; i++) {
                    Location at = parker.frame(i).location();
                    System.out.println("  frame " + i + ": " + at.declaringType().name() + "."
                            + at.method().name() + " native=" + at.method().isNative());
                }
                StringBuilder argTypes = new StringBuilder("[");
                try {
                    List<Value> values = parker.frame(1).getArgumentValues();
                    for (int i = 0; i < values.size(); i++) {
                        Value v = values.get(i);
                        argTypes.append(i == 0 ? "" : ", ").append(v == null ? "null" : v.type().name());
                    }
                    argTypes.append("]");
                } catch (RuntimeException e) {
                    argTypes = new StringBuilder(e.getClass().getName());
                }
                System.out.println("  args: " + argTypes);
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            parker.resume();
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
                    next.resume();
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
