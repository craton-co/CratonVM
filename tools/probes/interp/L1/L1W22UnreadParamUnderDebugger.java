// Interpreter round i1 wave 22, lane L1: a breakpoint set inside a compiled
// loop whose method has primitive parameters it never reads, and the values
// of those parameters at the stop.
// (docs/internal/fixed-bugs/interpreter-L1-an-unread-primitive-parameter-has-no-kind-in-single-pass-snapshots-FIXED-20260926.md)
//
// Plain run (what the probe runner diffs; no debugger): stdout must equal
// HotSpot 25's:
//
//   warm=2145452336 done
//
// Under a debugger (the case the wave fixes). The debuggee is CratonVM, the
// debugger is this same class run by HotSpot's `java` through JDI:
//
//   javac -g -d out L1W22UnreadParamUnderDebugger.java
//   # debuggee: a CratonVM built with --features cratonvm-vm/experimental-debug
//   # (the JDWP server is not in the default build; `-agentlib:jdwp=...` makes
//   # CratonVM try to dlopen HotSpot's libjdwp and exit)
//   cratonvm --java-home $JDK --jdwp-port 5005 -cp out L1W22UnreadParamUnderDebugger wait
//   java -cp out L1W22UnreadParamUnderDebugger debug 5005   # debugger, HotSpot 25
//
// (HotSpot as the debuggee: `java -agentlib:jdwp=transport=dt_socket,server=y,
// suspend=n,address=5005 -cp out L1W22UnreadParamUnderDebugger wait`.)
// (`CRATONVM_TIER_ENABLED=0` on the debuggee keeps the loop in the single-pass
// tier, which is what the fix covers; the optimizing tier's guard deopts are
// lane L2's i19-L1 page.) In `wait` mode the debuggee waits, up to five
// minutes, until the debugger has attached (it sets the static `attached`),
// warms `spin` up, sets the static `warmed`, and makes a last call that spins
// for up to twelve seconds; so a slow VM boot or warm-up cannot race the
// debugger. The debugger polls `warmed`, waits one more second, sets a
// breakpoint on the loop body line (BREAK_LINE below), prints what it sees at
// the stop, and sets the stopped frame's `deadline` to 0 so the loop ends at
// its next deadline check. HotSpot 25 as the debuggee prints, on the
// debugger's side:
//
//   hit spin line=55 id=7 verbose=true stamp=1234567890123 scale=2.5
//
// and `warm=2145452336 done` on the debuggee's. CratonVM before the fix
// printed `no breakpoint hit` (the single-pass body refused every mode exit,
// because the unread parameters had no kind and every exit map described them
// `Undefined`, so the loop ran compiled to its deadline and the line was never
// reached interpreted again).
public class L1W22UnreadParamUnderDebugger {
    static final int BREAK_LINE = 55; // the `sum += ...` line in `spin`

    /** Set by the debugger once it has attached (`wait` mode). */
    static volatile boolean attached;
    /** Set by the debuggee once `spin` is warm (`wait` mode). */
    static volatile boolean warmed;

    // `id`, `verbose`, `stamp` and `scale` are never read: assigned at entry,
    // dead everywhere, and of no kind a load/store scan can see.
    static long spin(long n, long deadline, int id, boolean verbose, long stamp, double scale) {
        long sum = 0;
        for (long i = 0; i < n; i++) {
            sum += i ^ (sum >>> 3);
            if ((i & 0xFFFFF) == 0 && System.nanoTime() > deadline) {
                break;
            }
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean underDebugger = args.length == 1 && args[0].equals("wait");
        if (underDebugger) {
            long until = System.currentTimeMillis() + 300_000;
            while (!attached && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
        }
        long warm = 0;
        for (int rep = 0; rep < 2000; rep++) {
            warm += spin(500, Long.MAX_VALUE, 7, true, 1234567890123L, 2.5);
        }
        warmed = true;
        // Under a debugger (`wait`): runs compiled until the debugger stops
        // it, or twelve seconds pass. Plain: returns at once.
        long spinFor = underDebugger ? 12_000_000_000L : 0L;
        spin(Long.MAX_VALUE, System.nanoTime() + spinFor, 7, true, 1234567890123L, 2.5);
        System.out.println("warm=" + (int) warm + " done");
    }

    static final class Debugger {
        static void run(int port) throws Exception {
            com.sun.jdi.connect.AttachingConnector socket = null;
            for (com.sun.jdi.connect.AttachingConnector c :
                    com.sun.jdi.Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.name().equals("com.sun.jdi.SocketAttach")) {
                    socket = c;
                }
            }
            java.util.Map<String, com.sun.jdi.connect.Connector.Argument> a = socket.defaultArguments();
            a.get("hostname").setValue("localhost");
            a.get("port").setValue(Integer.toString(port));
            com.sun.jdi.VirtualMachine vm = null;
            long deadline = System.currentTimeMillis() + 120_000;
            while (vm == null) {
                try {
                    vm = socket.attach(a);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > deadline) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
            com.sun.jdi.ReferenceType type = null;
            while (type == null) {
                java.util.List<com.sun.jdi.ReferenceType> found =
                        vm.classesByName("L1W22UnreadParamUnderDebugger");
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    type = found.get(0);
                } else if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("L1W22UnreadParamUnderDebugger never loaded");
                } else {
                    Thread.sleep(50);
                }
            }
            ((com.sun.jdi.ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            com.sun.jdi.Field warmedFlag = type.fieldByName("warmed");
            long warmBy = System.currentTimeMillis() + 300_000;
            while (!((com.sun.jdi.BooleanValue) type.getValue(warmedFlag)).value()) {
                if (System.currentTimeMillis() > warmBy) {
                    throw new IllegalStateException("the debuggee never warmed up");
                }
                Thread.sleep(50);
            }
            // Let the last call's loop run compiled before the breakpoint.
            Thread.sleep(1000);
            com.sun.jdi.Location at = type.locationsOfLine(BREAK_LINE).get(0);
            com.sun.jdi.request.BreakpointRequest bp =
                    vm.eventRequestManager().createBreakpointRequest(at);
            bp.setSuspendPolicy(com.sun.jdi.request.EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            String seen = "no breakpoint hit";
            long until = System.currentTimeMillis() + 60_000;
            outer:
            while (System.currentTimeMillis() < until) {
                com.sun.jdi.event.EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (com.sun.jdi.event.Event e : set) {
                    if (e instanceof com.sun.jdi.event.BreakpointEvent be) {
                        com.sun.jdi.StackFrame f = be.thread().frame(0);
                        com.sun.jdi.Method m = f.location().method();
                        StringBuilder sb = new StringBuilder("hit ")
                                .append(m.name())
                                .append(" line=")
                                .append(f.location().lineNumber());
                        for (String name : new String[] {"id", "verbose", "stamp", "scale"}) {
                            java.util.List<com.sun.jdi.LocalVariable> v = m.variablesByName(name);
                            sb.append(' ').append(name).append('=');
                            try {
                                sb.append(v.isEmpty() ? "?" : f.getValue(v.get(0)));
                            } catch (Exception ex) {
                                sb.append(ex.getClass().getSimpleName());
                            }
                        }
                        seen = sb.toString();
                        try {
                            // End the loop at its next deadline check.
                            f.setValue(m.variablesByName("deadline").get(0), vm.mirrorOf(0L));
                        } catch (Exception ex) {
                            // It then runs to its own deadline.
                        }
                        bp.disable();
                        break outer;
                    }
                    if (e instanceof com.sun.jdi.event.VMDeathEvent
                            || e instanceof com.sun.jdi.event.VMDisconnectEvent) {
                        break outer;
                    }
                }
                set.resume();
            }
            System.out.println(seen);
            try {
                vm.resume();
                vm.dispose();
            } catch (com.sun.jdi.VMDisconnectedException gone) {
                // The debuggee finished first.
            }
        }
    }
}
