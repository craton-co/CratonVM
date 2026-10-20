// Interpreter round i5 wave 101, lane L1: a platform thread parked in
// `LockSupport.park(Object)` while a debugger is attached lists HotSpot's
// frames in BOTH modes -- `Unsafe.park` (native) above `LockSupport.park`, a
// real Java frame whose argument the debugger can read -- the park-family
// stage of
// docs/known-issues/interpreter/i47-L1-proposal-compatible-runs-the-blocking-methods-bytecode-under-a-debugger-20261011.md.
//
// Since interpreter round i2 wave 69 (`JdkLockSupport` HotSpot-alignment row,
// `CRATONVM_COMPAT_JDK_LOCK_SUPPORT`, default on) `--compatible` over a real
// JDK drops the `LockSupport` stand-ins and runs the JDK bodies, as
// `--jdk-only` does; the page's last stage needs no debugger-only switch.
// With that row `=0` the stand-in runs: `frame 1` then reads as an opaque
// row (`args:` answers an exception) -- the regression this probe guards.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W101JdiCompatibleParkFrames`), in both modes.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L1W101JdiCompatibleParkFrames.java
//   cratonvm --compatible --java-home $JDK --jdwp-port 5791 -cp out L1W101JdiCompatibleParkFrames wait
//   java -cp out L1W101JdiCompatibleParkFrames debug 5791 > transcript.txt
//
// Interpreter round i6 wave 126 (lane L1): the `args:` row read `[null]` in
// both modes, with the defaults and with `CRATONVM_COMPAT_JDK_LOCK_SUPPORT=0`
// (run lane, w122-dbg binary; HotSpot answered the blocker). `blocker` is
// liveness-dead at `LockSupport.park`'s `Unsafe.park` call, and a blocked
// thread's dead reference locals were published as null; fixed behind
// `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` (default on; `=0` gives `[null]` again,
// the negative control).
//
// Wave 128 turned that switch back to default off (a dead slot's referent may
// die). Wave 130 (lane L1a): `CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS`
// (default on) reads the dead slot only in a VM with a JDWP port, where every
// local is a GC root, so the referent cannot die; this probe's debuggee always
// has one. Negative control: `CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS=0`
// (with `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` unset) gives `[null]`.
//
// HotSpot 25 as the debuggee (derived from JDK 25's LockSupport / Unsafe sources; the
// `args:` row confirmed against a live HotSpot debuggee by the wave-122 run lane):
//
//   == attach
//     status wait: true
//     frame 0: jdk.internal.misc.Unsafe.park native=true
//     frame 1: java.util.concurrent.locks.LockSupport.park native=false
//     frame 2: L1W101JdiCompatibleParkFrames.parkHere native=false
//     args: [L1W101JdiCompatibleParkFrames$Blocker]
//     blocker field: L1W101JdiCompatibleParkFrames$Blocker
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L1W101JdiCompatibleParkFrames {
    static final class Blocker {}

    /** Set by the debugger: 1 = start, 3 = done. 2 once the parker is about to park. */
    static volatile int phase;
    static volatile boolean stop;

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
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Thread parker = new Thread(L1W101JdiCompatibleParkFrames::parkHere, "parker");
        parker.setDaemon(true);
        parker.start();
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
            type = (ClassType) awaitClass("L1W101JdiCompatibleParkFrames");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
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
                    if (t.name().equals("parker")) {
                        parker = t;
                    }
                }
            }
            // Until the parker is suspended inside the park (it may still be
            // on its way to it on the first rounds).
            boolean parked = false;
            while (!parked) {
                if (System.currentTimeMillis() > until) {
                    fail("the parker was never suspended in Unsafe.park");
                }
                Thread.sleep(50);
                parker.suspend();
                if (parker.frameCount() > 0) {
                    Location top = parker.frame(0).location();
                    parked = top.method().name().equals("park")
                            && top.declaringType().name().equals("jdk.internal.misc.Unsafe");
                }
                if (!parked) {
                    parker.resume();
                }
            }
            System.out.println("  status wait: "
                    + (parker.status() == ThreadReference.THREAD_STATUS_WAIT));
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
            // Thread.parkBlocker, which LockSupport.park(Object) sets.
            ObjectReference thread = parker;
            Field blockerField = thread.referenceType().fieldByName("parkBlocker");
            Value blocker = blockerField == null ? null : thread.getValue(blockerField);
            System.out.println("  blocker field: " + (blocker == null ? "null" : blocker.type().name()));
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
