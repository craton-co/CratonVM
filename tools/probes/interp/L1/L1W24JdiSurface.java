// Interpreter round i1 wave 24, lane L1: the JDI conformance harness, the
// surface a debugger reaches beyond a line breakpoint
// (docs/internal/fixed-bugs/interpreter-L1-jdi-reachable-jdwp-commands-that-still-answer-not-implemented-FIXED-20260927.md,
// docs/internal/fixed-bugs/interpreter-L1-jdwp-class-only-filter-is-not-evaluated-on-steps-and-class-prepare-FIXED-20260927.md,
// docs/internal/fixed-bugs/interpreter-L1-jdwp-thread-objects-and-thread-groups-are-not-modelled-as-hotspot-does-FIXED-20260927.md):
// VirtualMachine.ClassPaths; MethodEntry / MethodExit(WithReturnValue)
// requests (jdb `trace methods`), co-located with a breakpoint; a
// ClassPrepare request and a step INTO filtered by TYPE (ClassOnly); thread
// values (tag `t`) and the program's thread groups; ClassType.NewInstance
// (and a constructor that throws), InterfaceType.InvokeMethod;
// ReferenceType.ClassLoader and ClassLoaderReference.VisibleClasses;
// ThreadReference.Interrupt and Stop (jdb `interrupt`, `kill`); and the
// VM_DEATH event at the end. Canonical transcript: no ids, no addresses.
//
// Plain run (what the probe runner diffs; no debugger): stdout must equal
// HotSpot 25's:
//
//   traced=10 route=50 sleeper=interrupted stop=none
//
// Under a debugger. The debuggee waits (up to five minutes) until the debugger
// has attached and set its first requests (the static `attached`, set through
// JDWP `ClassType.SetValues`). The debugger is this same class run by
// HotSpot's `java` through JDI:
//
//   javac -g -d out L1W24JdiSurface.java
//   # CratonVM debuggee: a build with --features cratonvm-vm/experimental-debug
//   cratonvm --java-home $JDK --jdwp-port 5741 -cp out L1W24JdiSurface wait
//   # HotSpot debuggee, for the reference transcript:
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5741 \
//       -cp out L1W24JdiSurface wait
//   # the debugger (HotSpot 25's java), in both cases:
//   java -cp out L1W24JdiSurface debug 5741 > transcript.txt
//
// Diff the two transcripts; the debuggee runs to its end and prints
// `traced=10 route=50 sleeper=interrupted stop=java.lang.IllegalArgumentException:stopped by the debugger`.
// To see the wire, put tools/jdi/JdwpTap.java between the two.
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   classPath=[out] bootClassPath=[] baseDirectory=true
//   == until stop1
//   exit waitForDebugger @43 -> void
//   entry traced @0
//   entry inner @0
//   exit inner @3 -> int 5
//   exit traced @6 -> int 10
//   entry stop1 @0
//   Breakpoint at L1W24JdiSurface.stop1:174@0 thread=main
//     (one set: [MethodEntry, Breakpoint] policy=2)
//   == until stepFrom
//   Breakpoint at L1W24JdiSurface.stepFrom:177@0 thread=main
//   == until step into, class filter Base
//   Step at L1W24JdiSurface$Derived.m:140@1 thread=main
//   == until inspect
//   prepare L1W24JdiSurface$Square
//   Breakpoint at L1W24JdiSurface.inspect:183@4 thread=main
//     worker is a ThreadReference=true name=sleeper status=2
//     me equals the event thread=true
//     thread group=main parent=system
//     top-level groups=[system]
//     new Box(7) = instance of L1W24JdiSurface$Box v=7
//     box.show() = "Box7"
//     new Box(-1) threw java.lang.IllegalArgumentException
//     Shape.sides(4) = int 7
//     loader=jdk.internal.loader.ClassLoaders$AppClassLoader
//     visible L1W24JdiSurface=true
//     visible L1W24JdiSurface$Box=true
//     visible java.lang.Object=true
//     visible java.lang.String=true
//     visible int[]=true
//     defines L1W24JdiSurface=true
//     interrupted the sleeper
//   == until victim
//   Breakpoint at L1W24JdiSurface.victim:191@0 thread=main
//   stop sent
//   == until finish
//   Breakpoint at L1W24JdiSurface.finish:199@0 thread=main
//     static sleeperSaw = "interrupted"
//     static stopSaw = "java.lang.IllegalArgumentException:stopped by the debugger"
//   == until the end
//   vm death (policy=0)
//   disconnected
//
// Notes on HotSpot's answers the transcript pins down:
//  * the method exit is reported at the return bytecode (`@43`, `@3`, `@6`)
//    with the value it returns, and no exit is reported for a method left by
//    an exception; the method entry at a breakpoint's location is in the
//    breakpoint's event set, first;
//  * the step INTO filtered to `Base` passes through `Util.route` (not a
//    `Base`) and stops at `Derived.m@1`, not `@0`: HotSpot's back end stops
//    single stepping in a filtered method and resumes it at the next accepted
//    method ENTRY, whose first bytecode single stepping then never sees;
//  * `VisibleClasses` lists 41 classes on HotSpot (the ones the loader
//    initiated); CratonVM lists every class its loader chain can name, a
//    superset, so the probe asks membership only.
//
// CratonVM before wave 24: `classPath` threw (ClassPaths was
// NOT_IMPLEMENTED), and so did the method entry / exit requests
// (INVALID_EVENT_TYPE), `newInstance`, `InterfaceType.invokeMethod`,
// `interrupt` and `stop` (NOT_IMPLEMENTED); the ClassPrepare request reported
// every prepared class and the step stopped in `Util.route`; `worker` was a
// plain ObjectReference, the thread group `system` with no parent, the class
// loader null; and the debugger saw no VMDeathEvent.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W24JdiSurface {
    static final int STEP_LINE = 177;     // `int r = Util.route(d, 5);` in `stepFrom`
    static final int INSPECT_LINE = 183;  // `int marker = 1;` in `inspect`
    static final int VICTIM_LINE = 191;   // `int spin = 0;` in `victim`

    static volatile boolean attached;
    static volatile String sleeperSaw = "none";
    static volatile String stopSaw = "none";

    interface Shape {
        static int sides(int k) {
            return k + 3;
        }
    }

    static final class Square implements Shape {
    }

    static final class Unrelated {
    }

    static class Base {
        int m(int x) {
            return x + 1;
        }
    }

    static final class Derived extends Base {
        int m(int x) {
            return x * 10;
        }
    }

    static final class Util {
        static int route(Base b, int x) {
            return b.m(x);
        }
    }

    static final class Box {
        final int v;

        Box(int v) {
            if (v < 0) {
                throw new IllegalArgumentException("negative");
            }
            this.v = v;
        }

        String show() {
            return "Box" + v;
        }
    }

    static int traced(int x) {
        return inner(x) * 2;
    }

    static int inner(int x) {
        return x + 1;
    }

    static void stop1() {
    }

    static int stepFrom(Base d) {
        int r = Util.route(d, 5);
        return r;
    }

    static void inspect(Thread worker) {
        Thread me = Thread.currentThread();
        int marker = 1;
        if (me == worker) {
            marker++;
        }
    }

    static void victim() {
        try {
            int spin = 0;
            spin++;
        } catch (IllegalArgumentException e) {
            stopSaw = e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static void finish() {
    }

    static void sleep() {
        try {
            Thread.sleep(120_000);
            sleeperSaw = "slept";
        } catch (InterruptedException e) {
            sleeperSaw = "interrupted";
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        Shape.sides(0);
        new Base();
        new Box(1).show();
        if (args.length == 1 && args[0].equals("wait")) {
            waitForDebugger();
        }
        int a = traced(4);
        stop1();
        Base d = new Derived();
        int r = stepFrom(d);
        Class.forName("L1W24JdiSurface$Square");
        Class.forName("L1W24JdiSurface$Unrelated");
        Thread worker = new Thread(L1W24JdiSurface::sleep, "sleeper");
        worker.start();
        while (worker.getState() != Thread.State.TIMED_WAITING) {
            if (!settle()) break;
        }
        if (args.length == 1) {
            inspect(worker);
        } else {
            worker.interrupt();
        }
        worker.join();
        victim();
        finish();
        System.out.println("traced=" + a + " route=" + r + " sleeper=" + sleeperSaw + " stop=" + stopSaw);
    }

    static void waitForDebugger() throws InterruptedException {
        long until = System.currentTimeMillis() + 300_000;
        while (!attached) {
            if (System.currentTimeMillis() > until) {
                System.out.println("no debugger attached");
                System.exit(3);
            }
            Thread.sleep(20);
        }
    }

    static final class Debugger {
        static VirtualMachine vm;
        static EventRequestManager erm;
        static ReferenceType main;

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
            erm = vm.eventRequestManager();
            System.out.println("== attach");
            main = awaitClass("L1W24JdiSurface");
            ThreadReference mainThread = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    mainThread = t;
                }
            }

            // VirtualMachine.ClassPaths (1/13).
            PathSearchingVirtualMachine paths = (PathSearchingVirtualMachine) vm;
            List<String> cp = new ArrayList<>();
            for (String p : paths.classPath()) {
                cp.add(new java.io.File(p).getName());
            }
            System.out.println("classPath=" + cp + " bootClassPath=" + paths.bootClassPath()
                    + " baseDirectory=" + !paths.baseDirectory().isEmpty());

            // `trace methods`: MethodEntry / MethodExit (with the return value)
            // on the main thread, for this class's own methods.
            MethodEntryRequest entries = erm.createMethodEntryRequest();
            entries.addThreadFilter(mainThread);
            entries.addClassFilter("L1W24JdiSurface");
            entries.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            entries.enable();
            MethodExitRequest exits = erm.createMethodExitRequest();
            exits.addThreadFilter(mainThread);
            exits.addClassFilter("L1W24JdiSurface");
            exits.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            exits.enable();
            BreakpointRequest bp1 = erm.createBreakpointRequest(method(main, "stop1").location());
            bp1.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp1.enable();
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its first
            // SUSPEND_ALL stop, and release that stop.
            vm.resume();
            ((ClassType) main).setValue(main.fieldByName("attached"), vm.mirrorOf(true));

            ThreadReference t = awaitStop("stop1", BreakpointEvent.class);
            entries.disable();
            exits.disable();
            // A class filter by TYPE (ClassOnly): only the subtypes of Shape.
            ClassPrepareRequest prepares = erm.createClassPrepareRequest();
            prepares.addClassFilter(awaitClass("L1W24JdiSurface$Shape"));
            prepares.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            prepares.enable();
            BreakpointRequest bp2 = erm.createBreakpointRequest(main.locationsOfLine(STEP_LINE).get(0));
            bp2.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp2.enable();
            vm.resume();

            t = awaitStop("stepFrom", BreakpointEvent.class);
            // A step INTO filtered by type (ClassOnly Base): the call goes through
            // Util.route first, which is not a Base.
            StepRequest step = erm.createStepRequest(t, StepRequest.STEP_LINE, StepRequest.STEP_INTO);
            step.addClassFilter(awaitClass("L1W24JdiSurface$Base"));
            step.addCountFilter(1);
            step.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            step.enable();
            vm.resume();
            t = awaitStop("step into, class filter Base", StepEvent.class);
            erm.deleteEventRequest(step);
            BreakpointRequest bp3 = erm.createBreakpointRequest(main.locationsOfLine(INSPECT_LINE).get(0));
            bp3.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp3.enable();
            vm.resume();

            t = awaitStop("inspect", BreakpointEvent.class);
            inspect(t);
            BreakpointRequest bp4 = erm.createBreakpointRequest(main.locationsOfLine(VICTIM_LINE).get(0));
            bp4.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp4.enable();
            vm.resume();

            t = awaitStop("victim", BreakpointEvent.class);
            ClassType ise = (ClassType) awaitClass("java.lang.IllegalArgumentException");
            Method iseInit = ise.concreteMethodByName("<init>", "(Ljava/lang/String;)V");
            ObjectReference thrown = ise.newInstance(t, iseInit,
                    List.of(vm.mirrorOf("stopped by the debugger")), 0);
            try {
                t.stop(thrown);
                System.out.println("stop sent");
            } catch (InternalException | InvalidTypeException e) {
                System.out.println("stop: " + e.getClass().getSimpleName());
            }
            BreakpointRequest bp5 = erm.createBreakpointRequest(method(main, "finish").location());
            bp5.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp5.enable();
            vm.resume();

            t = awaitStop("finish", BreakpointEvent.class);
            for (String name : new String[] {"sleeperSaw", "stopSaw"}) {
                System.out.println("  static " + name + " = " + fmt(main.getValue(main.fieldByName(name))));
            }
            erm.deleteAllBreakpoints();
            prepares.disable();
            vm.resume();
            awaitEnd();
        }

        static void inspect(ThreadReference t) throws Exception {
            StackFrame f = t.frame(0);
            Value worker = f.getValue(f.visibleVariableByName("worker"));
            Value me = f.getValue(f.visibleVariableByName("me"));
            System.out.println("  worker is a ThreadReference=" + (worker instanceof ThreadReference)
                    + (worker instanceof ThreadReference w ? " name=" + w.name() + " status=" + w.status() : ""));
            System.out.println("  me equals the event thread=" + t.equals(me));
            ThreadGroupReference group = t.threadGroup();
            System.out.println("  thread group=" + group.name() + " parent="
                    + (group.parent() == null ? "null" : group.parent().name()));
            List<String> top = new ArrayList<>();
            for (ThreadGroupReference g : vm.topLevelThreadGroups()) {
                top.add(g.name());
            }
            System.out.println("  top-level groups=" + top);

            // ClassType.NewInstance (3/4) and an invocation on the new object.
            ClassType box = (ClassType) awaitClass("L1W24JdiSurface$Box");
            Method ctor = box.concreteMethodByName("<init>", "(I)V");
            ObjectReference made = box.newInstance(t, ctor, List.of(vm.mirrorOf(7)), 0);
            System.out.println("  new Box(7) = " + fmt(made) + " v="
                    + made.getValue(box.fieldByName("v")));
            Value shown = made.invokeMethod(t, method(box, "show"), List.of(), 0);
            System.out.println("  box.show() = " + fmt(shown));
            try {
                box.newInstance(t, ctor, List.of(vm.mirrorOf(-1)), 0);
                System.out.println("  new Box(-1) returned");
            } catch (InvocationException e) {
                System.out.println("  new Box(-1) threw " + e.exception().referenceType().name());
            }

            // InterfaceType.InvokeMethod (5/1): a static interface method.
            InterfaceType shape = (InterfaceType) awaitClass("L1W24JdiSurface$Shape");
            Value sides = shape.invokeMethod(t, method(shape, "sides"), List.of(vm.mirrorOf(4)), 0);
            System.out.println("  Shape.sides(4) = " + fmt(sides));

            // ReferenceType.ClassLoader and ClassLoaderReference.VisibleClasses.
            ClassLoaderReference loader = main.classLoader();
            System.out.println("  loader=" + (loader == null ? "null" : loader.referenceType().name()));
            if (loader != null) {
                List<ReferenceType> visible = loader.visibleClasses();
                Set<String> names = new HashSet<>();
                for (ReferenceType r : visible) {
                    names.add(r.name());
                }
                for (String n : new String[] {"L1W24JdiSurface", "L1W24JdiSurface$Box",
                        "java.lang.Object", "java.lang.String", "int[]"}) {
                    System.out.println("  visible " + n + "=" + names.contains(n));
                }
                boolean definedSelf = false;
                for (ReferenceType r : loader.definedClasses()) {
                    definedSelf |= r.equals(main);
                }
                System.out.println("  defines L1W24JdiSurface=" + definedSelf);
            }

            // ThreadReference.Interrupt (11/11): the sleeper wakes with an
            // InterruptedException once the VM runs again.
            ((ThreadReference) worker).interrupt();
            System.out.println("  interrupted the sleeper");
        }

        static Method method(ReferenceType type, String name) {
            return type.methodsByName(name).get(0);
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

        /** Print every event until a `kind` event stops the VM; answer its thread. */
        static ThreadReference awaitStop(String what, Class<? extends LocatableEvent> kind)
                throws Exception {
            System.out.println("== until " + what);
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                ThreadReference stopped = null;
                List<String> kinds = new ArrayList<>();
                for (Event e : set) {
                    kinds.add(e.getClass().getSimpleName().replace("EventImpl", ""));
                    describe(e);
                    if (kind.isInstance(e)) {
                        stopped = ((LocatableEvent) e).thread();
                    }
                    if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended while waiting for " + what);
                    }
                }
                if (set.size() > 1) {
                    System.out.println("  (one set: " + kinds + " policy=" + set.suspendPolicy() + ")");
                }
                if (stopped != null) {
                    return stopped;
                }
                set.resume();
            }
            fail("no " + what + " event");
            return null;
        }

        static void describe(Event e) throws Exception {
            if (e instanceof MethodEntryEvent me) {
                System.out.println("entry " + me.method().name() + " @" + me.location().codeIndex());
            } else if (e instanceof MethodExitEvent mx) {
                System.out.println("exit " + mx.method().name() + " @" + mx.location().codeIndex()
                        + " -> " + fmt(mx.returnValue()));
            } else if (e instanceof ClassPrepareEvent cp) {
                System.out.println("prepare " + cp.referenceType().name());
            } else if (e instanceof LocatableEvent le) {
                Location l = le.location();
                System.out.println(e.getClass().getSimpleName().replace("EventImpl", "") + " at "
                        + l.declaringType().name() + "." + l.method().name() + ":" + l.lineNumber()
                        + "@" + l.codeIndex() + " thread=" + le.thread().name());
            }
        }

        static void awaitEnd() throws Exception {
            System.out.println("== until the end");
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(500);
                } catch (VMDisconnectedException gone) {
                    System.out.println("disconnected (exception)");
                    return;
                }
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof VMDeathEvent) {
                        System.out.println("vm death (policy=" + set.suspendPolicy() + ")");
                    } else if (e instanceof VMDisconnectEvent) {
                        System.out.println("disconnected");
                        return;
                    } else {
                        describe(e);
                    }
                }
                set.resume();
            }
            fail("the debuggee did not end");
        }

        static String fmt(Value v) {
            if (v == null) {
                return "null";
            }
            if (v instanceof VoidValue) {
                return "void";
            }
            if (v instanceof StringReference s) {
                return "\"" + s.value() + "\"";
            }
            if (v instanceof ObjectReference o) {
                return "instance of " + o.referenceType().name();
            }
            return v.type().name() + " " + v;
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

    /** A bounded wait (ten seconds) for the threads to reach their states. */
    static long settleDeadline;

    static boolean settle() throws InterruptedException {
        if (settleDeadline == 0) {
            settleDeadline = System.currentTimeMillis() + 10_000;
        }
        Thread.sleep(1);
        return System.currentTimeMillis() < settleDeadline;
    }
}
