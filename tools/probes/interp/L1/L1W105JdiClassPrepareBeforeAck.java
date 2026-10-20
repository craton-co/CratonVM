// Interpreter round i5 wave 105, lane L1: a class's `ClassPrepare` reaches
// the debugger before the reply to any command that reads state the
// program reached after preparing it.
//
// HotSpot posts `ClassPrepare` on the preparing thread, and its back end's
// event helper writes it at once; the program then runs on. JDI, once
// `allClasses()` has run, answers `classesByName` from the classes its own
// `ClassPrepare` request reported. So after the debuggee's
// `Class.forName(n, true, ..)` returned and it wrote an acknowledgement
// field, a debugger that read the acknowledgement finds the class. CratonVM
// queued the event and wrote it only at the top of the server's loop: when
// the debugger's next command was already waiting (`SERVER_READ_WAIT`, 2 ms
// against JDI's 20 ms poll) the command's reply went out first, and
// `classesByName` was empty (`L1W102JdiUnpreparedClassesHidden`, 3 runs in
// 24, both modes). Wave 105 (`CRATONVM_JDWP_PROMPT_EVENTS`, default on;
// vm/src/debug/mod.rs `send_event_set`, `ServerInput::Wake`, the drain
// before a command is served) writes the event as soon as it is queued.
// Not built or run. Page:
// docs/internal/fixed-bugs/interpreter-i102-L1-jdwp-lists-unprepared-classes-FIXED-20261016.md
//
// Twenty rounds of the same handshake, one class each, so a race that hits
// one run in eight shows in nearly every run.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W105JdiClassPrepareBeforeAck.java
//   cratonvm --java-home $JDK --jdwp-port 5802 -cp out L1W105JdiClassPrepareBeforeAck wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5802 \
//       -cp out L1W105JdiClassPrepareBeforeAck wait
//   java -cp out L1W105JdiClassPrepareBeforeAck debug 5802 > transcript.txt
//
// HotSpot 25 as the debuggee (derived, NOT measured):
//
//   == attach
//   == rounds
//     found after the ack: 20 of 20
//   == end
//     disconnected
// Orchestrator, 2026-10-16 (Linux w105c): NOT deterministic on HotSpot 25
// either -- the full conformance run had HotSpot print "found after the ack:
// 19 of 20" while CratonVM printed 20 of 20. JDI builds the event's reference
// type on its own event-handler thread, so an event written just before a reply
// can still lose the race inside JDI. Kept out of the conformance list; a
// statistic only (CratonVM 12 failures in 24 runs with the default, 12 in 12
// with CRATONVM_JDWP_PROMPT_EVENTS=0).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W105JdiClassPrepareBeforeAck {
    static final int ROUNDS = 20;

    /** Set by the debugger to 2*i+1 to start round i; the debuggee acks with 2*i+2. 99 = done. */
    static volatile int phase;

    static final class C0 { static int v = 0; }
    static final class C1 { static int v = 1; }
    static final class C2 { static int v = 2; }
    static final class C3 { static int v = 3; }
    static final class C4 { static int v = 4; }
    static final class C5 { static int v = 5; }
    static final class C6 { static int v = 6; }
    static final class C7 { static int v = 7; }
    static final class C8 { static int v = 8; }
    static final class C9 { static int v = 9; }
    static final class C10 { static int v = 10; }
    static final class C11 { static int v = 11; }
    static final class C12 { static int v = 12; }
    static final class C13 { static int v = 13; }
    static final class C14 { static int v = 14; }
    static final class C15 { static int v = 15; }
    static final class C16 { static int v = 16; }
    static final class C17 { static int v = 17; }
    static final class C18 { static int v = 18; }
    static final class C19 { static int v = 19; }

    static String nameOf(int i) {
        return "L1W105JdiClassPrepareBeforeAck$C" + i;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        ClassLoader loader = L1W105JdiClassPrepareBeforeAck.class.getClassLoader();
        for (int i = 0; i < ROUNDS; i++) {
            while (debugged && phase != 2 * i + 1 && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            Class.forName(nameOf(i), true, loader);
            if (debugged) {
                phase = 2 * i + 2;
            }
        }
        while (debugged && phase != 99 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static int phase() {
            return ((IntegerValue) type.getValue(type.fieldByName("phase"))).value();
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
            type = (ClassType) awaitClass("L1W105JdiClassPrepareBeforeAck");
            // From here on JDI answers `classesByName` from its cache, which
            // only `ClassPrepare` events fill.
            vm.allClasses();
            System.out.println("== rounds");
            int found = 0;
            for (int i = 0; i < ROUNDS; i++) {
                type.setValue(type.fieldByName("phase"), vm.mirrorOf(2 * i + 1));
                long until = System.currentTimeMillis() + 120_000;
                while (phase() != 2 * i + 2) {
                    if (System.currentTimeMillis() > until) {
                        fail("round " + i + " never acknowledged");
                    }
                    // L1W102's poll period: HotSpot's event leaves at the
                    // prepare, long before a read 0-20 ms later; JDI builds
                    // events on a thread of its own, so a read landing
                    // microseconds after the write could race JDI itself.
                    Thread.sleep(20);
                }
                if (vm.classesByName(nameOf(i)).size() == 1) {
                    found++;
                }
            }
            System.out.println("  found after the ack: " + found + " of " + ROUNDS);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(99));
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
