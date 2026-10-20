// GC round gci, wave i1, lane fr (2026-10-11): a reference local that is IN
// SCOPE (the LocalVariableTable names it) but DEAD by bytecode liveness, read
// by a JDI debugger at a breakpoint after two System.gc() calls, for
// docs/known-issues/gc/gcf-f1f-debugger-local-reads-export-liveness-dead-stale-locals-20261001.md.
// It replaces that page's MANUAL jdb block (gch h3/fd, `GchH3fdDeadLocalJdwpProbe`)
// with a scenario the JDI conformance runner runs unattended.
//
// HotSpot keeps every interpreter local alive while an agent can access
// locals (its interpreter oop maps are type-based, and JDWP holds
// can_access_local_variables), so the debugger sees the array. Before gch
// h1/jp a CratonVM root scan dropped the liveness-dead local, the array could
// be collected, and the debugger was shown null (or whatever was allocated
// over it).
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario GciI1frJdiDeadLocalAfterGc`), with the collectors the f1f page
// compares (`--debuggee-flag -XX:+UseSerialGC --debuggee-flag -Xmx64m`: HotSpot
// runs Serial, CratonVM warns on stderr and runs Generational).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   marker=1048576
//   done
//
// Under a debugger (the debuggee waits up to five minutes for `attached`):
//
//   javac -g -d out GciI1frJdiDeadLocalAfterGc.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -XX:+UseSerialGC -Xmx64m -cp out GciI1frJdiDeadLocalAfterGc wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -XX:+UseSerialGC -Xmx64m -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out GciI1frJdiDeadLocalAfterGc wait)
//   java -cp out GciI1frJdiDeadLocalAfterGc debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot debuggee flags: -XX:+UseSerialGC -Xmx64m
//
// HotSpot 25 as the debuggee (the debugger's stdout; from the f1f page's
// reading of HotSpot and gch h3/fd's jdb block, to be confirmed by the
// runner's --check-headers):
//
//   == attach
//   == breakpoint after two collections
//     breakpoint hit: true
//     dead: byte[] length=1048576 first=7 last=0
//   == end
//     disconnected
//
// A CratonVM transcript with `dead: null`, another type, another length, or
// another first byte is the f1f defect (the root scan dropped the local and
// the array was collected or its span reused); `breakpoint hit: false` means
// the JDWP server did not stop the thread (a void run for this page).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class GciI1frJdiDeadLocalAfterGc {
    /** The `int marker = sink;` line in `probe` (BREAK below). */
    static final int BREAK_LINE = 74;

    /** Set by the debugger once its breakpoint is in place (`wait` mode). */
    static volatile boolean attached;
    static int sink;

    static void probe() {
        byte[] dead = new byte[1 << 20];
        dead[0] = 7;
        sink = dead.length; // the last read of `dead`
        for (int i = 0; i < 64; i++) {
            churn(i);
        }
        System.gc();
        System.gc();
        int marker = sink; // BREAK: `dead` in scope, liveness-dead
        System.out.println("marker=" + marker);
    }

    /** Young garbage between the last read and the collections. */
    static void churn(int i) {
        Object[] junk = new Object[256];
        for (int k = 0; k < junk.length; k++) {
            junk[k] = new byte[64 + (k & 7)];
        }
        if (junk[i & 255] == null) {
            sink++;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (!attached && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
        }
        probe();
        System.out.println("done");
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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ReferenceType type = awaitClass("GciI1frJdiDeadLocalAfterGc");
            List<Location> at = type.locationsOfLine(BREAK_LINE);
            if (at.isEmpty()) {
                fail("no code at line " + BREAK_LINE);
            }
            BreakpointRequest bp = erm.createBreakpointRequest(at.get(0));
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));

            System.out.println("== breakpoint after two collections");
            ThreadReference hit = null;
            long until = System.currentTimeMillis() + 120_000;
            while (hit == null && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        hit = b.thread();
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (hit == null) {
                    set.resume();
                }
            }
            System.out.println("  breakpoint hit: " + (hit != null));
            if (hit != null) {
                StackFrame frame = hit.frame(0);
                LocalVariable local = frame.visibleVariableByName("dead");
                if (local == null) {
                    System.out.println("  dead: not visible");
                } else {
                    System.out.println("  dead: " + describe(frame.getValue(local)));
                }
            }
            erm.deleteEventRequest(bp);

            System.out.println("== end");
            if (hit != null) {
                hit.resume();
            }
            while (true) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    System.out.println("  disconnected");
                    return;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        System.out.println("  disconnected");
                        return;
                    }
                }
                set.resume();
            }
        }

        /** The local's value without ids: type, length and two elements. */
        static String describe(Value v) {
            if (v == null) {
                return "null";
            }
            if (!(v instanceof ArrayReference arr)) {
                return "not an array: " + v.type().name();
            }
            int n = arr.length();
            String s = arr.type().name() + " length=" + n;
            if (n > 0) {
                s += " first=" + arr.getValue(0) + " last=" + arr.getValue(n - 1);
            }
            return s;
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
