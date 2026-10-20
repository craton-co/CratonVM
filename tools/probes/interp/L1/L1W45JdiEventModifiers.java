// Interpreter round i1 wave 45, lane L1: JDWP event request modifiers
// against HotSpot, through JDI (the lane's review of `Count`, `ThreadOnly`,
// `ClassMatch` / `ClassExclude`, `LocationOnly`, `ExceptionOnly` caught and
// uncaught, `FieldOnly`, and a step INTO with class filters).
//
// The debuggee waits (`go`) until the debugger has made its requests, then
// runs `work()` on `main`. Every request suspends the event thread; the
// debugger prints one line per event and resumes it:
//
// * `bp count 3`: a breakpoint in `Target.loop` with `addCountFilter(3)`,
//   reached five times: reported once, the third time.
// * `bp other thread`: the same location with `addThreadFilter` naming the
//   `idler` thread, which never runs `loop`: never reported.
// * `entry Target count 2`: `MethodEntryRequest` with
//   `addClassFilter("*$Target")` and `addCountFilter(2)`: the second method
//   of `Target` entered (`b`), and nothing after.
// * `entry excluded`: `MethodEntryRequest` with
//   `addClassFilter("L1W45JdiEventModifiers$Target")` and
//   `addClassExclusionFilter("*$Target")`: never reported.
// * `exception ISE caught`: `ExceptionRequest(IllegalStateException, caught
//   only)`, thrown in this class: the caught `IllegalStateException` and its
//   subclass, not the `IllegalArgumentException`, not the uncaught one.
// * `exception uncaught`: `ExceptionRequest(null, uncaught only)`, thrown in
//   this class: the exception that ends the `helper` thread.
// * `modify v`: `ModificationWatchpointRequest` on `Holder.v` with
//   `addClassExclusionFilter("*$Holder")`: the class filters test the
//   LOCATION's class, so the writes from `work()` report and the one inside
//   `Holder.set` does not.
// * `entry Target count 2` counts `Target.loop`'s entries too.
// * `bp at the valueOf line`, then a step INTO (LINE) with
//   `addClassExclusionFilter` for `java.*`, `jdk.*` and `sun.*` and
//   `addCountFilter(1)`: it skips `String.valueOf` and stops in `Obj.toString`,
//   which the JDK method calls.
//
// Canonical transcript: no ids, no addresses, no timings; lines are relative
// to the line of `Target.loop`'s first statement.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W45JdiEventModifiers`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   helper ended
//   done v=3 s=obj
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W45JdiEventModifiers.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W45JdiEventModifiers wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W45JdiEventModifiers wait)
//   java -cp out L1W45JdiEventModifiers debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (three runs, the Windows box, the same each time):
//
//   == attach
//     entry Target count 2: Target.loop:0
//     bp count 3: Target.loop:0
//     exception ISE caught: java.lang.IllegalStateException "caught ise" in main caught=true
//     exception ISE caught: SubIse "caught sub" in main caught=true
//     exception uncaught: java.lang.IllegalStateException "uncaught in helper" in helper caught=false
//     modify v: 1 at L1W45JdiEventModifiers.work:68
//     modify v: 3 at L1W45JdiEventModifiers.work:70
//     bp at the valueOf line: L1W45JdiEventModifiers.work:72
//     step into, JDK excluded: Obj.toString:31
//   == end
//     disconnected
//
// CratonVM (predicted from `debug::events`, not run): the same. Each
// modifier is evaluated in request order (`location_request_reports`,
// `match_subject_events`); the class filters of a field event test the
// location's class; the step INTO follows HotSpot's method-entry mode
// (`step_entry_gate`). A differing line is a new divergence: file it.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W45JdiEventModifiers {
    /** 0 until the debugger has made its requests. */
    static volatile int go;

    static final class Target {
        static int loop(int i) {
            int x = i * 2; // BASE_LINE
            return x + 1;
        }

        static void a() {
        }

        static void b() {
        }

        static void c() {
        }
    }

    static final class SubIse extends IllegalStateException {
        SubIse(String m) {
            super(m);
        }
    }

    static final class Holder {
        int v;

        void set(int x) {
            v = x;
        }
    }

    static final class Obj {
        @Override
        public String toString() {
            String r = "obj";
            return r;
        }
    }

    static int sink;

    static String work() throws Exception {
        for (int i = 0; i < 5; i++) {
            sink += Target.loop(i);
        }
        Target.a();
        Target.b();
        Target.c();
        try {
            throw new IllegalStateException("caught ise");
        } catch (IllegalStateException e) {
            sink++;
        }
        try {
            throw new IllegalArgumentException("caught iae");
        } catch (IllegalArgumentException e) {
            sink++;
        }
        try {
            throw new SubIse("caught sub");
        } catch (IllegalStateException e) {
            sink++;
        }
        Thread helper = new Thread(() -> {
            throw new IllegalStateException("uncaught in helper");
        }, "helper");
        helper.setUncaughtExceptionHandler((t, e) -> { });
        helper.start();
        helper.join();
        System.out.println("helper ended");
        Holder h = new Holder();
        h.v = 1;
        h.set(2);
        h.v = 3;
        Obj o = new Obj();
        String s = String.valueOf(o); // VALUEOF_LINE
        return "v=" + h.v + " s=" + s;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        // Loaded and prepared before the debugger looks for them.
        Class.forName("L1W45JdiEventModifiers$Target");
        Class.forName("L1W45JdiEventModifiers$Holder");
        Class.forName("java.lang.IllegalStateException");
        Thread idler = new Thread(() -> {
            try {
                Thread.sleep(600_000);
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        }, "idler");
        idler.setDaemon(true);
        idler.start();
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
            return l.declaringType().name().replace("L1W45JdiEventModifiers$", "") + "."
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
            ClassType main = (ClassType) awaitClass("L1W45JdiEventModifiers");
            ReferenceType target = awaitClass("L1W45JdiEventModifiers$Target");
            ReferenceType holder = awaitClass("L1W45JdiEventModifiers$Holder");
            ThreadReference mainThread = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    mainThread = t;
                }
            }
            Method loop = target.methodsByName("loop").get(0);
            Location loopFirst = loop.allLineLocations().get(0);
            base = loopFirst.lineNumber();
            Method work = main.methodsByName("work").get(0);
            // `String s = String.valueOf(o);`: work's last line but one.
            TreeSet<Integer> workLines = new TreeSet<>();
            for (Location l : work.allLineLocations()) {
                workLines.add(l.lineNumber());
            }
            List<Integer> wl = new ArrayList<>(workLines);
            int valueOfLine = wl.get(wl.size() - 2);
            Location valueOfAt = work.locationsOfLine(valueOfLine).get(0);

            Field detailMessage = vm.classesByName("java.lang.Throwable").get(0)
                    .fieldByName("detailMessage");
            Map<EventRequest, String> names = new HashMap<>();
            BreakpointRequest bpCount = erm.createBreakpointRequest(loopFirst);
            bpCount.addCountFilter(3);
            names.put(bpCount, "bp count 3");
            // A thread that never runs `loop`: the debuggee's `idler`.
            ThreadReference other = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("idler")) {
                    other = t;
                }
            }
            if (other == null) {
                fail("no idler thread");
            }
            BreakpointRequest bpOther = erm.createBreakpointRequest(loopFirst);
            bpOther.addThreadFilter(other);
            names.put(bpOther, "bp other thread");
            MethodEntryRequest entry = erm.createMethodEntryRequest();
            entry.addClassFilter("*$Target");
            entry.addCountFilter(2);
            names.put(entry, "entry Target count 2");
            MethodEntryRequest excluded = erm.createMethodEntryRequest();
            excluded.addClassFilter("L1W45JdiEventModifiers$Target");
            excluded.addClassExclusionFilter("*$Target");
            names.put(excluded, "entry excluded");
            ReferenceType ise = awaitClass("java.lang.IllegalStateException");
            ExceptionRequest caught = erm.createExceptionRequest(ise, true, false);
            caught.addClassFilter("L1W45JdiEventModifiers*");
            names.put(caught, "exception ISE caught");
            ExceptionRequest uncaught = erm.createExceptionRequest(null, false, true);
            uncaught.addClassFilter("L1W45JdiEventModifiers*");
            names.put(uncaught, "exception uncaught");
            ModificationWatchpointRequest modify =
                    erm.createModificationWatchpointRequest(holder.fieldByName("v"));
            modify.addClassExclusionFilter("*$Holder");
            names.put(modify, "modify v");
            BreakpointRequest bpValueOf = erm.createBreakpointRequest(valueOfAt);
            bpValueOf.addThreadFilter(mainThread);
            names.put(bpValueOf, "bp at the valueOf line");
            for (EventRequest r : names.keySet()) {
                r.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                r.enable();
            }
            main.setValue(main.fieldByName("go"), vm.mirrorOf(1));

            boolean ended = false;
            long until = System.currentTimeMillis() + 120_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    String name = names.get(e.request());
                    if (e instanceof BreakpointEvent b) {
                        System.out.println("  " + name + ": " + at(b.location()));
                        if (e.request() == bpValueOf) {
                            StepRequest step = erm.createStepRequest(b.thread(),
                                    StepRequest.STEP_LINE, StepRequest.STEP_INTO);
                            step.addClassExclusionFilter("java.*");
                            step.addClassExclusionFilter("jdk.*");
                            step.addClassExclusionFilter("sun.*");
                            step.addCountFilter(1);
                            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                            names.put(step, "step into, JDK excluded");
                            step.enable();
                        }
                    } else if (e instanceof StepEvent s) {
                        System.out.println("  " + name + ": " + at(s.location()));
                        erm.deleteEventRequest(e.request());
                    } else if (e instanceof MethodEntryEvent m) {
                        System.out.println("  " + name + ": " + at(m.location()));
                    } else if (e instanceof ExceptionEvent x) {
                        String message = ((StringReference) x.exception().getValue(
                                detailMessage)).value();
                        System.out.println("  " + name + ": " + x.exception().referenceType().name()
                                .replace("L1W45JdiEventModifiers$", "") + " \"" + message + "\" in "
                                + x.thread().name() + " caught=" + (x.catchLocation() != null));
                    } else if (e instanceof ModificationWatchpointEvent w) {
                        System.out.println("  " + name + ": " + w.valueToBe() + " at "
                                + at(w.location()));
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
