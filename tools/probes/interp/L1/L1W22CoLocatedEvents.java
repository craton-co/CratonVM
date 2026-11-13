// Interpreter round i1 wave 22, lane L1: the events a thread reports at one
// location are ONE JDWP event set, and the thread is suspended once for it.
//
// Plain run (what the probe runner diffs; no debugger): stdout must equal
// HotSpot 25's:
//
//   target(20)=41
//
// Under a debugger (the case the wave fixes). The debuggee is CratonVM, the
// debugger is this same class run by HotSpot's `java` through JDI:
//
//   javac -g -d out L1W22CoLocatedEvents.java
//   # debuggee: a CratonVM built with --features cratonvm-vm/experimental-debug
//   # (the JDWP server is not in the default build; `-agentlib:jdwp=...` makes
//   # CratonVM try to dlopen HotSpot's libjdwp and exit)
//   cratonvm --java-home $JDK --jdwp-port 5006 -cp out L1W22CoLocatedEvents wait
//   java -cp out L1W22CoLocatedEvents debug 5006        # debugger, HotSpot 25
//
// (HotSpot as the debuggee: `java -agentlib:jdwp=transport=dt_socket,server=y,
// suspend=n,address=5006 -cp out L1W22CoLocatedEvents wait`.) In `wait` mode
// the debuggee waits, up to five minutes, until the debugger has set its
// breakpoints and then set the static `attached` (JDWP `ClassType.SetValues`),
// so a slow VM boot cannot race the debugger; the debugger retries the attach
// and waits for the class to be prepared.
//
// The debugger sets one breakpoint on LINE_A and TWO on LINE_B of `target`.
// At the LINE_A stop it asks for a line step over, which lands on LINE_B:
// the step and both breakpoints there are co-located. HotSpot 25 as the
// debuggee prints, on the debugger's side:
//
//   set 1: Breakpoint@61
//   set 2: Breakpoint@62 Breakpoint@62 Step@62
//   sets=2 debuggee=exited
//
// and `target(20)=41` on the debuggee's. CratonVM before the fix sent every
// event as its own set (`set 2`, `set 3`, `set 4` of one event each, sets=4)
// and counted one suspension per event, so a debugger resuming the one set it
// is shown left the thread suspended.
public class L1W22CoLocatedEvents {
    static final int LINE_A = 61; // `int y = x * 2;` in `target`
    static final int LINE_B = 62; // `return y + 1;` in `target`

    /** Set by the debugger once its requests are in place (`wait` mode). */
    static volatile boolean attached;

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
        System.out.println("target(20)=" + target(20));
    }

    static int target(int x) {
        int y = x * 2;
        return y + 1;
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
                        vm.classesByName("L1W22CoLocatedEvents");
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    type = found.get(0);
                } else if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("L1W22CoLocatedEvents never loaded");
                } else {
                    Thread.sleep(50);
                }
            }
            com.sun.jdi.request.EventRequestManager erm = vm.eventRequestManager();
            breakpoint(erm, type, LINE_A);
            breakpoint(erm, type, LINE_B);
            breakpoint(erm, type, LINE_B);
            // Release the debuggee.
            ((com.sun.jdi.ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            int sets = 0;
            String end = "debuggee=running";
            long until = System.currentTimeMillis() + 60_000;
            outer:
            while (System.currentTimeMillis() < until) {
                com.sun.jdi.event.EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                java.util.List<String> names = new java.util.ArrayList<>();
                com.sun.jdi.ThreadReference stopped = null;
                for (com.sun.jdi.event.Event e : set) {
                    if (e instanceof com.sun.jdi.event.VMDeathEvent
                            || e instanceof com.sun.jdi.event.VMDisconnectEvent) {
                        end = "debuggee=exited";
                        break outer;
                    }
                    if (e instanceof com.sun.jdi.event.LocatableEvent le) {
                        String kind = e instanceof com.sun.jdi.event.StepEvent ? "Step" : "Breakpoint";
                        names.add(kind + "@" + le.location().lineNumber());
                        stopped = le.thread();
                    }
                    if (e instanceof com.sun.jdi.event.StepEvent) {
                        erm.deleteEventRequest(e.request());
                    }
                }
                if (!names.isEmpty()) {
                    sets++;
                    java.util.Collections.sort(names);
                    System.out.println("set " + sets + ": " + String.join(" ", names));
                    if (names.equals(java.util.List.of("Breakpoint@" + LINE_A))) {
                        com.sun.jdi.request.StepRequest step = erm.createStepRequest(
                                stopped,
                                com.sun.jdi.request.StepRequest.STEP_LINE,
                                com.sun.jdi.request.StepRequest.STEP_OVER);
                        step.setSuspendPolicy(com.sun.jdi.request.EventRequest.SUSPEND_EVENT_THREAD);
                        step.enable();
                    }
                }
                set.resume();
            }
            System.out.println("sets=" + sets + " " + end);
            try {
                vm.dispose();
            } catch (com.sun.jdi.VMDisconnectedException gone) {
                // The debuggee finished first.
            }
        }

        static void breakpoint(com.sun.jdi.request.EventRequestManager erm,
                com.sun.jdi.ReferenceType type, int line) throws Exception {
            com.sun.jdi.request.BreakpointRequest bp =
                    erm.createBreakpointRequest(type.locationsOfLine(line).get(0));
            bp.setSuspendPolicy(com.sun.jdi.request.EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
        }
    }
}
