// Interpreter round i1 wave 46, lane L1: requests whose `Count` filter is
// spent and which the debugger leaves in place, through JDI
// (docs/internal/fixed-bugs/interpreter-L1-proposal-a-spent-count-filter-releases-its-request-FIXED-20261010.md).
//
// The debuggee waits (`go`) until the debugger has made its requests, then
// runs `work()` on `main`: three calls of `Target.hit`, then a hot loop of
// 1,000,000 calls of `Target.calc`, then its last line. The debugger:
//
// * `entry count 1`: a `MethodEntryRequest` filtered to `*$Target` with
//   `addCountFilter(1)`, never deleted: reported once, at the first
//   `Target.hit`, and never again (JDWP: "subsequent events are never
//   reported for this request").
// * `bp start`: a breakpoint on `work`'s first line; at it the debugger makes
//   a step OVER (LINE) with `addCountFilter(1)` and does NOT delete it after
//   its event (JDI does not do it for the debugger): `step over count 1`.
// * `bp end`: a breakpoint on `work`'s last line: the session still reports
//   a request without a count after the loop.
//
// Every request suspends the event thread; the debugger prints one line per
// event and resumes it. Canonical transcript: no ids, no addresses, no
// timings; lines are relative to `work`'s first line.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W46JdiSpentCountFilter`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   done sink=15500122486185
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W46JdiSpentCountFilter.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W46JdiSpentCountFilter wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W46JdiSpentCountFilter wait)
//   java -cp out L1W46JdiSpentCountFilter debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (three runs, the Windows box, the same each time):
//
//   == attach
//     bp start: L1W46JdiSpentCountFilter.work:0
//     entry count 1: Target.hit:-9
//     step over count 1: L1W46JdiSpentCountFilter.work:1
//     bp end: L1W46JdiSpentCountFilter.work:8
//   == end
//     disconnected
//
// CratonVM, base and wave 46 alike (predicted from `debug::events`): the
// same transcript. A spent `Count` suppresses for good
// (`location_request_reports`). What wave 46 changes is what the spent
// requests cost: on the base they stay in the request table and keep the
// gates armed (`EventManager::method_events_requested`,
// `has_single_steps`), so every method of the VM runs interpreted, through
// the suspend point's request match, for the whole hot loop, and every
// compiled body stays withdrawn. With `events::SPENT_COUNT_RELEASES_GATES`
// the gate questions skip spent requests and the matcher that spent one
// republishes the gates. Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[JDWP_COUNT_SPENT] request=<id> gates republished` twice (the entry
// request, then the step request), and never on the base (the line does not
// exist there); `CRATONVM_DBG_JITC=1` prints no `interpreter-only
// withdrawal` line after them, where the base keeps the whole-cache
// withdrawal armed to the end. The debuggee's `work` takes seconds on the
// base (`--nojit` and JIT alike) and a fraction of that with the switch.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W46JdiSpentCountFilter {
    /** 0 until the debugger has made its requests. */
    static volatile int go;

    static long sink;

    static final class Target {
        static int hit(int i) {
            return i + 1;
        }

        static int calc(int i) {
            return (i * 31) ^ (i >>> 3);
        }
    }

    static String work() {
        sink += Target.hit(1); // work's first line
        sink += Target.hit(2);
        sink += Target.hit(3);
        long acc = 0;
        for (int i = 0; i < 1_000_000; i++) {
            acc += Target.calc(i);
        }
        sink += acc;
        return "sink=" + sink; // work's last line
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        // Loaded and prepared before the debugger looks for it.
        Class.forName("L1W46JdiSpentCountFilter$Target");
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        System.out.println("done " + work());
    }

    static final class Debugger {
        static VirtualMachine vm;
        static int base;

        static String at(Location l) {
            return l.declaringType().name().replace("L1W46JdiSpentCountFilter$", "") + "."
                    + l.method().name() + ":" + (l.lineNumber() - base);
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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ClassType main = (ClassType) awaitClass("L1W46JdiSpentCountFilter");
            awaitClass("L1W46JdiSpentCountFilter$Target");
            Method work = main.methodsByName("work").get(0);
            TreeSet<Integer> lines = new TreeSet<>();
            for (Location l : work.allLineLocations()) {
                lines.add(l.lineNumber());
            }
            base = lines.first();
            Location first = work.locationsOfLine(lines.first()).get(0);
            Location last = work.locationsOfLine(lines.last()).get(0);

            Map<EventRequest, String> names = new HashMap<>();
            MethodEntryRequest entry = erm.createMethodEntryRequest();
            entry.addClassFilter("*$Target");
            entry.addCountFilter(1);
            names.put(entry, "entry count 1");
            BreakpointRequest start = erm.createBreakpointRequest(first);
            names.put(start, "bp start");
            BreakpointRequest end = erm.createBreakpointRequest(last);
            names.put(end, "bp end");
            for (EventRequest r : names.keySet()) {
                r.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                r.enable();
            }
            main.setValue(main.fieldByName("go"), vm.mirrorOf(1));

            boolean ended = false;
            long until = System.currentTimeMillis() + 240_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    String name = names.get(e.request());
                    if (e instanceof BreakpointEvent b) {
                        System.out.println("  " + name + ": " + at(b.location()));
                        if (e.request() == start) {
                            StepRequest step = erm.createStepRequest(b.thread(),
                                    StepRequest.STEP_LINE, StepRequest.STEP_OVER);
                            step.addCountFilter(1);
                            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                            names.put(step, "step over count 1");
                            step.enable();
                        }
                    } else if (e instanceof StepEvent s) {
                        // Left in place: its count is spent.
                        System.out.println("  " + name + ": " + at(s.location()));
                    } else if (e instanceof MethodEntryEvent m) {
                        System.out.println("  " + name + ": " + at(m.location()));
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("== end");
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
