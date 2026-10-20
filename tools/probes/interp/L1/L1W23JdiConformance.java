// Interpreter round i1 wave 23, lane L1: the JDI conformance harness, stage 1
// (docs/internal/fixed-bugs/interpreter-L1-proposal-jdi-conformance-harness-FIXED-20261003.md).
// One fixed debugger session, the one a `jdb` user runs — attach,
// classesBySignature, a line breakpoint, `where`, `locals`, `print` of fields,
// statics and arrays, step over / into / out, `cont`, `exit` — printed as a
// canonical transcript (no ids, no addresses, no timings), so CratonVM's
// transcript can be diffed against HotSpot's line for line.
//
// Plain run (what the probe runner diffs; no debugger): stdout must equal
// HotSpot 25's:
//
//   compute=790,785
//
// Under a debugger. The debuggee waits (up to five minutes) until the debugger
// has attached and set its breakpoint: the debugger sets the static `attached`
// through JDWP `ClassType.SetValues`, so a slow VM boot cannot race it. The
// debugger is this same class run by HotSpot's `java` through JDI:
//
//   javac -g -d out L1W23JdiConformance.java
//   # CratonVM debuggee: a build with --features cratonvm-vm/experimental-debug
//   cratonvm --java-home $JDK --jdwp-port 5723 -cp out L1W23JdiConformance wait
//   # HotSpot debuggee, for the reference transcript:
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5723 \
//       -cp out L1W23JdiConformance wait
//   # the debugger (HotSpot 25's java), in both cases:
//   java -cp out L1W23JdiConformance debug 5723 > transcript.txt
//
// Diff the two transcripts. The debuggee prints nothing under the debugger:
// the session ends with `VirtualMachine.Exit(0)` at the second stop, so
// `compute=` never prints and the debuggee's exit status is 0.
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   class L1W23JdiConformance source=L1W23JdiConformance.java prepared=true version=69
//     superclass=java.lang.Object interfaces=[java.lang.Comparable] modifiers=0x21
//     generic=Ljava/lang/Object;Ljava/lang/Comparable<LL1W23JdiConformance;>;
//     field static final int LINE_BP
//     field static volatile boolean attached
//     field static int counter
//     field static java.lang.String label
//     field int width
//     field long total
//     field double ratio
//     field char initial
//     field boolean flag
//     field byte small
//     field short mid
//     field float part
//     field int[] squares
//     field java.lang.String[] names
//     field java.lang.Object nothing
//     field java.util.List tags generic=Ljava/util/List<Ljava/lang/String;>;
//     field L1W23JdiConformance$Pair pair
//     method <init>()V modifiers=0x1
//     method compareTo(LL1W23JdiConformance;)I modifiers=0x1
//     method compute(I)I modifiers=0x0
//     method helper(I)I modifiers=0x8
//     method main([Ljava/lang/String;)V modifiers=0x9
//     method waitForDebugger()V modifiers=0x8
//     method compareTo(Ljava/lang/Object;)I modifiers=0xf0001041 synthetic
//     method <clinit>()V modifiers=0x8
//   locationsOfLine(157) = compute@0
//   helper lines=[166@0, 167@4]
//   compute variables=[i:int, n:int:arg, acc:int, wide:long]
//   Integer.valueOf(int) lines>0=true variables=[i:int:arg]
//   == stop 1 (breakpoint)
//   at L1W23JdiConformance.compute:157@0 thread=main status=1 suspended=true
//     [0] L1W23JdiConformance.compute (L1W23JdiConformance.java:157)
//     [1] L1W23JdiConformance.main (L1W23JdiConformance.java:179)
//     local n = int 3
//     this.width = int 5
//     this.total = long 1099511627776
//     this.ratio = double 0.75
//     this.initial = char q
//     this.flag = boolean true
//     this.small = byte -7
//     this.mid = short 300
//     this.part = float 1.5
//     this.squares = int[4] {0, 1, 4, 9}
//     this.names = java.lang.String[3] {"a", null, "c"}
//     this.nothing = null
//     this.tags = instance of java.util.ImmutableCollections$List12
//     this.pair = instance of L1W23JdiConformance$Pair
//     static counter = int 3
//     static label = "craton"
//   class L1W23JdiConformance$Pair source=L1W23JdiConformance.java modifiers=0x38 static=true final=true
//   == stop 2 (step over)
//   at L1W23JdiConformance.compute:158@5 thread=main status=1 suspended=true
//     local n = int 3
//     local acc = int 5
//   == stop 3 (step over)
//   at L1W23JdiConformance.compute:159@12 thread=main status=1 suspended=true
//     local n = int 3
//     local acc = int 5
//     local i = int 0
//   == stop 4 (step into)
//   at L1W23JdiConformance.helper:166@0 thread=main status=1 suspended=true
//     [0] L1W23JdiConformance.helper (L1W23JdiConformance.java:166)
//     [1] L1W23JdiConformance.compute (L1W23JdiConformance.java:159)
//     [2] L1W23JdiConformance.main (L1W23JdiConformance.java:179)
//     local k = int 0
//   == stop 5 (step out)
//   at L1W23JdiConformance.compute:159@17 thread=main status=1 suspended=true
//     local n = int 3
//     local acc = int 5
//     local i = int 0
//   == stop 6 (cont)
//   at L1W23JdiConformance.compute:157@0 thread=main status=1 suspended=true
//     [0] L1W23JdiConformance.compute (L1W23JdiConformance.java:157)
//     [1] L1W23JdiConformance.main (L1W23JdiConformance.java:180)
//     local n = int 2
//   == exit
//   disconnected
//
// CratonVM before wave 23 died at `locationsOfLine` with
// UnsupportedOperationException (JDWP `ReferenceType.MethodsWithGeneric` was
// not implemented; docs/internal/fixed-bugs/interpreter-L1-jdi-cannot-set-a-line-breakpoint-jdwp-lacks-the-generic-commands-FIXED-20260926.md).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W23JdiConformance implements Comparable<L1W23JdiConformance> {
    static final int LINE_BP = 157;    // `int acc = width;` in `compute`

    static volatile boolean attached;
    static int counter = 3;
    static String label = "craton";
    int width = 5;
    long total = 1L << 40;
    double ratio = 0.75;
    char initial = 'q';
    boolean flag = true;
    byte small = -7;
    short mid = 300;
    float part = 1.5f;
    int[] squares = {0, 1, 4, 9};
    String[] names = {"a", null, "c"};
    Object nothing;
    List<String> tags = List.of("x");
    Pair pair = new Pair();

    // A member class (its modifiers come from `InnerClasses`), and through
    // `Comparable<...>` a generic class signature, a superinterface and a
    // synthetic bridge method `compareTo(Object)`.
    static final class Pair {
        int a = 1;
    }

    public int compareTo(L1W23JdiConformance other) {
        return width - other.width;
    }

    int compute(int n) {
        int acc = width;
        for (int i = 0; i < n; i++) {
            acc += helper(i);
        }
        long wide = total + acc;
        return (int) (wide % 1000);
    }

    static int helper(int k) {
        int doubled = k * 2;
        return doubled + 1;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 1 && args[0].equals("wait")) {
            waitForDebugger();
        }
        L1W23JdiConformance t = new L1W23JdiConformance();
        int first = t.compute(3);
        int second = t.compute(2);
        System.out.println("compute=" + first + "," + second);
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
        static int stops;

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

            ReferenceType type = null;
            long until = System.currentTimeMillis() + 120_000;
            while (type == null) {
                List<ReferenceType> found = vm.classesByName("L1W23JdiConformance");
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    type = found.get(0);
                } else if (System.currentTimeMillis() > until) {
                    fail("class L1W23JdiConformance never loaded");
                } else {
                    Thread.sleep(50);
                }
            }
            describeClass((ClassType) type);

            Location bpAt = type.locationsOfLine(LINE_BP).get(0);
            System.out.println("locationsOfLine(" + LINE_BP + ") = "
                    + bpAt.method().name() + "@" + bpAt.codeIndex());
            Method helper = type.methodsByName("helper").get(0);
            List<String> lines = new ArrayList<>();
            for (Location l : helper.allLineLocations()) {
                lines.add(l.lineNumber() + "@" + l.codeIndex());
            }
            System.out.println("helper lines=" + lines);
            Method compute = type.methodsByName("compute").get(0);
            List<String> vars = new ArrayList<>();
            for (LocalVariable v : compute.variables()) {
                vars.add(v.name() + ":" + v.typeName() + (v.isArgument() ? ":arg" : ""));
            }
            System.out.println("compute variables=" + vars);
            describeJdkMethod();

            BreakpointRequest bp = erm.createBreakpointRequest(bpAt);
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            // Release the debuggee.
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its SUSPEND_ALL
            // breakpoint, and release it.
            vm.resume();
            Field flag = type.fieldByName("attached");
            ((ClassType) type).setValue(flag, vm.mirrorOf(true));

            ThreadReference t = awaitStop("breakpoint", BreakpointEvent.class);
            where(t);
            locals(t.frame(0));
            fields(t.frame(0).thisObject());
            statics(type);
            describeMember("L1W23JdiConformance$Pair");

            step(t, StepRequest.STEP_OVER, "step over");
            locals(t.frame(0));
            step(t, StepRequest.STEP_OVER, "step over");
            locals(t.frame(0));
            step(t, StepRequest.STEP_INTO, "step into");
            where(t);
            locals(t.frame(0));
            step(t, StepRequest.STEP_OUT, "step out");
            locals(t.frame(0));

            vm.resume();
            t = awaitStop("cont", BreakpointEvent.class);
            where(t);
            locals(t.frame(0));

            System.out.println("== exit");
            erm.deleteAllBreakpoints();
            vm.exit(0);
            until = System.currentTimeMillis() + 60_000;
            while (System.currentTimeMillis() < until) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(500);
                } catch (VMDisconnectedException gone) {
                    System.out.println("disconnected");
                    return;
                }
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        System.out.println("disconnected");
                        return;
                    }
                }
            }
            fail("the debuggee did not exit");
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

        static void describeClass(ClassType type) throws Exception {
            System.out.println("class " + type.name() + " source=" + type.sourceName()
                    + " prepared=" + type.isPrepared() + " version=" + type.majorVersion());
            List<String> ifaces = new ArrayList<>();
            for (InterfaceType i : type.interfaces()) {
                ifaces.add(i.name());
            }
            System.out.println("  superclass=" + type.superclass().name() + " interfaces=" + ifaces
                    + " modifiers=0x" + Integer.toHexString(type.modifiers()));
            System.out.println("  generic=" + type.genericSignature());
            for (Field f : type.fields()) {
                System.out.println("  field " + (f.isStatic() ? "static " : "")
                        + (f.isFinal() ? "final " : "") + (f.isVolatile() ? "volatile " : "")
                        + f.typeName() + " " + f.name()
                        + (f.genericSignature() == null ? "" : " generic=" + f.genericSignature()));
            }
            for (Method m : type.methods()) {
                System.out.println("  method " + m.name() + m.signature() + " modifiers=0x"
                        + Integer.toHexString(m.modifiers()) + (m.isSynthetic() ? " synthetic" : "")
                        + (m.genericSignature() == null ? "" : " generic=" + m.genericSignature()));
            }
        }

        static void describeMember(String name) throws Exception {
            ReferenceType member = vm.classesByName(name).get(0);
            System.out.println("class " + member.name() + " source=" + member.sourceName()
                    + " modifiers=0x" + Integer.toHexString(member.modifiers())
                    + " static=" + member.isStatic() + " final=" + member.isFinal());
        }

        static void describeJdkMethod() throws Exception {
            ReferenceType integer = vm.classesByName("java.lang.Integer").get(0);
            Method valueOf = integer.methodsByName("valueOf", "(I)Ljava/lang/Integer;").get(0);
            String vars;
            try {
                List<String> names = new ArrayList<>();
                for (LocalVariable v : valueOf.variables()) {
                    names.add(v.name() + ":" + v.typeName() + (v.isArgument() ? ":arg" : ""));
                }
                vars = names.toString();
            } catch (AbsentInformationException absent) {
                vars = "AbsentInformationException";
            }
            System.out.println("Integer.valueOf(int) lines>0=" + !valueOf.allLineLocations().isEmpty()
                    + " variables=" + vars);
        }

        static ThreadReference awaitStop(String what, Class<? extends LocatableEvent> kind)
                throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (kind.isInstance(e)) {
                        LocatableEvent le = (LocatableEvent) e;
                        stops++;
                        System.out.println("== stop " + stops + " (" + what + ")");
                        ThreadReference t = le.thread();
                        Location l = le.location();
                        System.out.println("at " + l.declaringType().name() + "." + l.method().name()
                                + ":" + l.lineNumber() + "@" + l.codeIndex() + " thread=" + t.name()
                                + " status=" + t.status() + " suspended=" + t.isSuspended());
                        return t;
                    }
                    if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended while waiting for the " + what);
                    }
                }
                set.resume();
            }
            fail("no " + what + " event");
            return null;
        }

        static void step(ThreadReference t, int depth, String what) throws Exception {
            StepRequest step = erm.createStepRequest(t, StepRequest.STEP_LINE, depth);
            for (String excluded : new String[] {"java.*", "javax.*", "sun.*", "com.sun.*", "jdk.internal.*"}) {
                step.addClassExclusionFilter(excluded);
            }
            step.addCountFilter(1);
            step.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            step.enable();
            vm.resume();
            awaitStop(what, StepEvent.class);
            erm.deleteEventRequest(step);
        }

        static void where(ThreadReference t) throws Exception {
            int i = 0;
            for (StackFrame f : t.frames()) {
                Location l = f.location();
                System.out.println("  [" + i++ + "] " + l.declaringType().name() + "."
                        + l.method().name() + " (" + l.sourceName() + ":" + l.lineNumber() + ")");
            }
        }

        static void locals(StackFrame f) throws Exception {
            List<LocalVariable> visible = f.visibleVariables();
            Map<LocalVariable, Value> values = f.getValues(visible);
            for (LocalVariable v : visible) {
                System.out.println("  local " + v.name() + " = " + fmt(values.get(v)));
            }
        }

        static void fields(ObjectReference self) throws Exception {
            List<Field> own = new ArrayList<>();
            for (Field f : self.referenceType().fields()) {
                if (!f.isStatic()) {
                    own.add(f);
                }
            }
            Map<Field, Value> values = self.getValues(own);
            for (Field f : own) {
                System.out.println("  this." + f.name() + " = " + fmt(values.get(f)));
            }
        }

        static void statics(ReferenceType type) throws Exception {
            for (String name : new String[] {"counter", "label"}) {
                System.out.println("  static " + name + " = " + fmt(type.getValue(type.fieldByName(name))));
            }
        }

        static String fmt(Value v) {
            if (v == null) {
                return "null";
            }
            if (v instanceof StringReference s) {
                return "\"" + s.value() + "\"";
            }
            if (v instanceof ArrayReference arr) {
                String type = arr.referenceType().name();
                StringBuilder sb = new StringBuilder(type.substring(0, type.length() - 2))
                        .append('[').append(arr.length()).append("] {");
                List<Value> elems = arr.getValues();
                for (int i = 0; i < elems.size(); i++) {
                    Value e = elems.get(i);
                    sb.append(i == 0 ? "" : ", ").append(e instanceof PrimitiveValue ? e.toString() : fmt(e));
                }
                return sb.append('}').toString();
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
}
