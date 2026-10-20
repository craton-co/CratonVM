// Interpreter round i4 wave 85, lane L1: MEASUREMENT program (JDI, a
// LaunchingConnector) for the order of co-located events in one JDWP event
// set, and for JDWP InstanceOnly filters on a breakpoint
// (docs/internal/fixed-bugs/interpreter-i84-L1-jdwp-event-set-order-and-instance-filters-FIXED-20261119.md:
// instance filters implemented in wave 85, the order waits for this run).
//
// The probe is the debugger; it launches its own nested class `Target` as the
// debuggee through `com.sun.jdi.CommandLineLaunch` (the same command line an
// IDE builds: `<home>/bin/<vmexec> <options> -Xdebug
// -Xrunjdwp:transport=dt_socket,address=<host:port>,suspend=y
// L1W85EventSetOrder$Target`, or the `-agentlib:jdwp=` spelling, depending on
// the JDK's JDI; both map onto the built-in server since wave 85).
//
// Run (HotSpot first; this run is the oracle):
//   javac -g -d /tmp/l1w85order tools/probes/interp/L1/L1W85EventSetOrder.java
//   java -cp /tmp/l1w85order L1W85EventSetOrder
// Against CratonVM (needs -agentlib:jdwp, wave 85, and an experimental-debug
// build): a directory whose bin/ holds the cratonvm binary, and its name:
//   java -cp /tmp/l1w85order L1W85EventSetOrder <dir-with-bin> cratonvm [--jdk-only|--compatible]
//
// What it prints (each line deterministic for one VM):
//   capabilities: instanceFilters=<bool>
//   set1 <policy>: <Kind:name>, ...   -- at Target.work() bci 0: a method
//        entry request ("entry") and two breakpoints there created in the
//        order "bp1", "bp2": the iteration order of one event set
//   set2 <policy>: <Kind:name>, ...   -- a STEP_MIN/STEP_OVER step ("step")
//        landing on work() bci 1, where breakpoint "bp4" sits
//   instance filter: hits=<n> sameThis=<bool>
//        -- breakpoint "bp5" at work() bci 0 filtered to the first Target's
//        `this`; main calls t.work() twice and u.work() once, and the probe
//        is stopped in the first t.work() when it creates bp5: HotSpot
//        reports the second t.work() only (hits=1 sameThis=true)
//   debuggee exit
//
// Fixed after the first HotSpot run (25.0.3): the class-filtered method
// entry request also reports Target.main and Target.<init>; those sets are
// resumed unmeasured, so set1 is the first set AT work() (entry, bp1, bp2 at
// bci 0) and its step lands on bp4 at bci 1, both requested before the
// resume; the receiver for the instance filter is read in set1.
// EXPECTED_BEGIN (MEASURED, HotSpot 25.0.3 (Temurin, Windows); it matched
// the order derived from the back end's sources)
// capabilities: instanceFilters=true
// set1 all: MethodEntry:entry, Breakpoint:bp2, Breakpoint:bp1
// set2 all: Step:step, Breakpoint:bp4
// instance filter: hits=1 sameThis=true
// debuggee exit
// EXPECTED_END
// CratonVM since wave 85 composes the same order (runtime/interpreter.rs
// deliver_breakpoint_if_set: MethodEntry, SingleStep, Breakpoint; each kind
// newest request first, events.rs match_location_events_with_instance).
// Before, it printed set1 as entry, bp1, bp2 and set2 as bp4, step.
import com.sun.jdi.Bootstrap;
import com.sun.jdi.Location;
import com.sun.jdi.Method;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.VirtualMachine;
import com.sun.jdi.connect.Connector;
import com.sun.jdi.connect.LaunchingConnector;
import com.sun.jdi.event.BreakpointEvent;
import com.sun.jdi.event.ClassPrepareEvent;
import com.sun.jdi.event.Event;
import com.sun.jdi.event.EventSet;
import com.sun.jdi.event.LocatableEvent;
import com.sun.jdi.event.MethodEntryEvent;
import com.sun.jdi.event.StepEvent;
import com.sun.jdi.event.VMDeathEvent;
import com.sun.jdi.event.VMDisconnectEvent;
import com.sun.jdi.event.VMStartEvent;
import com.sun.jdi.request.BreakpointRequest;
import com.sun.jdi.request.ClassPrepareRequest;
import com.sun.jdi.request.EventRequest;
import com.sun.jdi.request.EventRequestManager;
import com.sun.jdi.request.MethodEntryRequest;
import com.sun.jdi.request.StepRequest;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

public class L1W85EventSetOrder {
    /** The debuggee. Line numbers do not matter; locations are bytecode indices. */
    public static class Target {
        int field;

        void work() {
            field++;
        }

        public static void main(String[] args) {
            Target t = new Target();
            t.work();
            t.work();
            Target u = new Target();
            u.work();
        }
    }

    public static void main(String[] args) throws Exception {
        LaunchingConnector connector = Bootstrap.virtualMachineManager().defaultConnector();
        Map<String, Connector.Argument> arguments = connector.defaultArguments();
        StringBuilder options = new StringBuilder("-cp " + System.getProperty("java.class.path"));
        for (int i = 2; i < args.length; i++) {
            options.append(' ').append(args[i]);
        }
        arguments.get("options").setValue(options.toString());
        arguments.get("main").setValue("L1W85EventSetOrder$Target");
        if (args.length >= 1) {
            arguments.get("home").setValue(args[0]);
        }
        if (args.length >= 2) {
            arguments.get("vmexec").setValue(args[1]);
        }
        VirtualMachine vm = connector.launch(arguments);
        drain(vm.process().getInputStream());
        drain(vm.process().getErrorStream());
        System.out.println("capabilities: instanceFilters=" + vm.canUseInstanceFilters());

        EventRequestManager erm = vm.eventRequestManager();
        ClassPrepareRequest prepare = erm.createClassPrepareRequest();
        prepare.addClassFilter("L1W85EventSetOrder$Target");
        prepare.setSuspendPolicy(EventRequest.SUSPEND_ALL);
        prepare.enable();

        int set = 0;
        Method work = null;
        ObjectReference firstThis = null;
        BreakpointRequest bp5 = null;
        MethodEntryRequest entry = null;
        int instanceHits = 0;
        boolean sameThis = true;
        boolean done = false;
        while (!done) {
            EventSet events = vm.eventQueue().remove(60_000);
            if (events == null) {
                System.out.println("timeout");
                break;
            }
            boolean resume = true;
            for (Event e : events) {
                if (e instanceof VMDisconnectEvent) {
                    done = true;
                    resume = false;
                }
            }
            if (done) {
                break;
            }
            Event first = events.iterator().next();
            if (first instanceof VMStartEvent || first instanceof VMDeathEvent) {
                // nothing to measure
            } else if (first instanceof ClassPrepareEvent) {
                ReferenceType type = ((ClassPrepareEvent) first).referenceType();
                work = type.methodsByName("work").get(0);
                Location at0 = work.locationOfCodeIndex(0);
                Location at1 = work.locationOfCodeIndex(1);
                entry = erm.createMethodEntryRequest();
                entry.addClassFilter(type);
                entry.putProperty("name", "entry");
                entry.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                entry.enable();
                for (String name : new String[] {"bp1", "bp2"}) {
                    BreakpointRequest bp = erm.createBreakpointRequest(at0);
                    bp.putProperty("name", name);
                    bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                    bp.enable();
                }
                BreakpointRequest bp4 = erm.createBreakpointRequest(at1);
                bp4.putProperty("name", "bp4");
                bp4.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                bp4.enable();
            } else if (first instanceof LocatableEvent && bp5 == null
                    && !((LocatableEvent) first).location().method().equals(work)) {
                // A method entry of another Target method (the constructor,
                // entered before work()): not a measured set.
            } else if (first instanceof LocatableEvent && bp5 == null) {
                set++;
                System.out.println("set" + set + " " + policy(events.suspendPolicy()) + ": " + describe(events));
                ThreadReference thread = ((LocatableEvent) first).thread();
                if (set == 1) {
                    // At work() bci 0, the first t.work(): its receiver, and a
                    // step that lands on bci 1, where bp4 sits (both enabled
                    // before this set is resumed).
                    entry.disable();
                    firstThis = thread.frame(0).thisObject();
                    StepRequest step = erm.createStepRequest(thread, StepRequest.STEP_MIN, StepRequest.STEP_OVER);
                    step.putProperty("name", "step");
                    step.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                    step.enable();
                } else {
                    // set 2: clear everything, then the instance filter.
                    for (EventRequest r : new ArrayList<EventRequest>(erm.stepRequests())) {
                        erm.deleteEventRequest(r);
                    }
                    erm.deleteEventRequests(new ArrayList<EventRequest>(erm.breakpointRequests()));
                    if (firstThis == null) {
                        System.out.println("instance filter: no receiver read at set1");
                    } else {
                        bp5 = erm.createBreakpointRequest(work.locationOfCodeIndex(0));
                        bp5.putProperty("name", "bp5");
                        bp5.addInstanceFilter(firstThis);
                        bp5.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                        bp5.enable();
                    }
                }
            } else if (first instanceof BreakpointEvent) {
                for (Event e : events) {
                    if (e.request() == bp5) {
                        instanceHits++;
                        ObjectReference self = ((BreakpointEvent) e).thread().frame(0).thisObject();
                        sameThis &= firstThis.equals(self);
                    }
                }
            }
            if (resume) {
                events.resume();
            }
        }
        System.out.println("instance filter: hits=" + instanceHits + " sameThis=" + sameThis);
        System.out.println("debuggee exit");
    }

    private static String describe(EventSet events) {
        List<String> parts = new ArrayList<>();
        for (Event e : events) {
            String kind = e instanceof MethodEntryEvent ? "MethodEntry"
                    : e instanceof BreakpointEvent ? "Breakpoint"
                    : e instanceof StepEvent ? "Step"
                    : e.getClass().getSimpleName();
            parts.add(kind + ":" + e.request().getProperty("name"));
        }
        return String.join(", ", parts);
    }

    private static String policy(int policy) {
        switch (policy) {
            case EventRequest.SUSPEND_ALL:
                return "all";
            case EventRequest.SUSPEND_EVENT_THREAD:
                return "thread";
            default:
                return "none";
        }
    }

    private static void drain(InputStream in) {
        Thread t = new Thread(() -> {
            try {
                byte[] buf = new byte[4096];
                while (in.read(buf) >= 0) {
                    // discarded: the debuggee prints nothing of interest
                }
            } catch (Exception ignored) {
                // the debuggee ended
            }
        });
        t.setDaemon(true);
        t.start();
    }
}
