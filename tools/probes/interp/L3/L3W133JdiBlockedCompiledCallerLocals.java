// Interpreter round i6 wave 133, lane L3: the acceptance probe of stage 4 of
// docs/internal/proposals/interpreter/i18-L3-proposal-jdwp-lists-the-compiled-activations-of-a-blocked-thread-20260925.md
// ("a thread blocked in a native under a compiled loop, one frame up, its
// locals compared with HotSpot"; wave 121 said no probe was written).
//
// Two worker threads block in `Thread.sleep` under a method that has been
// compiled before the sleep starts. `entry` calls the instance method
// `Entry.step` 200 000 times (a method-entry compile), then once more with
// `last == true`, which sleeps. `osr` makes ONE call of the static `spin`,
// whose 3 000 000-trip loop compiles on stack (OSR) and sleeps near its end.
// The debugger attaches BEFORE the warm-up (it sets `go`), so the bodies are
// compiled while a debugger observes locals. Once both workers sleep, it
// suspends the VM and prints, per worker: its status, its frames (the JDK
// ones by name, this probe's with their line numbers), `this` of the
// compiled frame, and every visible local of it, sorted by name. It then
// interrupts both sleeps and lets the program end.
//
// Canonical transcript: no ids, no addresses, no timings.
//
//   javac -g -d out L3W133JdiBlockedCompiledCallerLocals.java
//   tools/jdi/run-jdi-conformance.sh --jdk $JDK --cratonvm $CVM \
//       --modes "jdk-only compatible" --scenario L3W133JdiBlockedCompiledCallerLocals
//   # by hand:
//   cratonvm --java-home $JDK --jdwp-port 5795 -cp out L3W133JdiBlockedCompiledCallerLocals wait
//   java -cp out L3W133JdiBlockedCompiledCallerLocals debug 5795        # HotSpot 25's java
//   # HotSpot debuggee:
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5795 -cp out L3W133JdiBlockedCompiledCallerLocals wait
//
// Plain run (no debugger), HotSpot 25.0.3, default flags and -Xint alike:
//
// HOTSPOT_EXPECTED_BEGIN
// entry result=-3533469647941325241
// osr result=-3684361708384435949
// ended
// HOTSPOT_EXPECTED_END
//
// HotSpot 25.0.3 as the debuggee (default flags and -Xint alike):
//
//   == attach
//   == ready
//     entry status: SLEEPING suspended=true
//     entry frames: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos java.lang.Thread.sleep L3W133JdiBlockedCompiledCallerLocals$Entry.step:107 L3W133JdiBlockedCompiledCallerLocals$Entry.run:127
//     entry this: <L3W133JdiBlockedCompiledCallerLocals$Entry>
//     entry locals: acc=-3741056667567956143 i=200000 k=600001 last=true mix=-3533469647942000252 name="entry-last" ratio=75000.125 tag="entry-last"
//     osr status: SLEEPING suspended=true
//     osr frames: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos java.lang.Thread.sleep L3W133JdiBlockedCompiledCallerLocals$Osr.spin:147 L3W133JdiBlockedCompiledCallerLocals$Osr.run:158
//     osr this: static
//     osr locals: acc=2276398903224562742 i=2999000 n=3000000 tag="osr"
//   == end
//     disconnected
//
// CratonVM (a build with --features cratonvm-vm/experimental-debug), JIT on:
//
// * `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1`: the transcript above, in
//   both modes. Positive control: `CRATONVM_DBG_JITC=1` prints
//   `[cratonvm-jitc] debugger home frame ...step... bci=N: write door ...`
//   (and one for `spin`) on the debuggee's stderr when the compiled
//   activation's homes were read (`interpreter::read_compiled_homes`). With
//   no such line the frame was interpreted when it slept (a compile that did
//   not land in time); the run then proves nothing about stage 4 and should
//   be repeated, not counted as a pass.
// * Switch unset: the compiled activations are listed without readable
//   locals, so the `locals` rows (and possibly `this`) differ: the
//   expected, recorded difference, not a regression.
// * `--nojit`: the transcript above in every arm (no compiled activation).
//
// What a difference means: a wrong line number on `step` / `spin` is the
// page's item 1 (the activation reported at the wrong bci, e.g. the OSR back
// edge); a wrong value is item 2 (stale or misread home slots); a missing
// probe frame is the original defect (a compiled activation not listed).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L3W133JdiBlockedCompiledCallerLocals {
    /** Set by the debugger after attaching: the workers may start. */
    static volatile int go;
    /** Set by the debugger once it has looked. */
    static volatile int done;
    /** How many workers are inside their long sleep. */
    static volatile int ready;
    /** The length of the final sleeps: long under a debugger, short without. */
    static long sleepMillis = 1;

    static synchronized void markReady() {
        ready++;
    }

    static final class Entry extends Thread {
        static final int WARM = 200_000;
        long result;

        Entry() {
            super("entry");
            setDaemon(true);
        }

        long step(int i, long acc, String tag, boolean last) {
            int k = i * 3 + 1;
            long mix = acc ^ (k * 0x9E3779B97F4A7C15L);
            double ratio = k / 8.0;
            String name = tag;
            if (last) {
                markReady();
                try {
                    Thread.sleep(sleepMillis);
                } catch (InterruptedException woken) {
                    // The debugger interrupts the long sleep once it has looked.
                }
            }
            return mix + k + (long) ratio + name.length();
        }

        @Override
        public void run() {
            long acc = 17;
            for (int i = 0; i < WARM; i++) {
                acc = step(i, acc, "w", false);
            }
            // Give a background compile time to land before the last call.
            try {
                Thread.sleep(300);
            } catch (InterruptedException ignored) {
                // Not interrupted here.
            }
            result = step(WARM, acc, "entry-last", true);
        }
    }

    static final class Osr extends Thread {
        static final int TRIPS = 3_000_000;
        long result;

        Osr() {
            super("osr");
            setDaemon(true);
        }

        static long spin(int n, String tag) {
            long acc = 7;
            for (int i = 0; i < n; i++) {
                acc = acc * 31 + (i ^ (acc >>> 7));
                if (i == n - 1000) {
                    markReady();
                    try {
                        Thread.sleep(sleepMillis);
                    } catch (InterruptedException woken) {
                        // The debugger interrupts the long sleep once it has looked.
                    }
                }
            }
            return acc + tag.length();
        }

        @Override
        public void run() {
            result = spin(TRIPS, "osr");
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            sleepMillis = 300_000;
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        Entry entry = new Entry();
        Osr osr = new Osr();
        entry.start();
        osr.start();
        entry.join(330_000);
        osr.join(330_000);
        if (debugged) {
            long until = System.currentTimeMillis() + 60_000;
            while (done == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        System.out.println("entry result=" + entry.result);
        System.out.println("osr result=" + osr.result);
        System.out.println("ended");
    }

    static final class Debugger {
        static final String PROBE = "L3W133JdiBlockedCompiledCallerLocals";
        static VirtualMachine vm;

        static String status(ThreadReference t) {
            switch (t.status()) {
                case ThreadReference.THREAD_STATUS_ZOMBIE: return "ZOMBIE";
                case ThreadReference.THREAD_STATUS_RUNNING: return "RUNNING";
                case ThreadReference.THREAD_STATUS_SLEEPING: return "SLEEPING";
                case ThreadReference.THREAD_STATUS_MONITOR: return "MONITOR";
                case ThreadReference.THREAD_STATUS_WAIT: return "WAIT";
                case ThreadReference.THREAD_STATUS_NOT_STARTED: return "NOT_STARTED";
                default: return "UNKNOWN(" + t.status() + ")";
            }
        }

        static String show(Value v) {
            if (v == null) {
                return "null";
            }
            if (v instanceof StringReference) {
                return "\"" + ((StringReference) v).value() + "\"";
            }
            if (v instanceof ObjectReference) {
                return "<" + ((ObjectReference) v).referenceType().name() + ">";
            }
            return v.toString();
        }

        /** The frames, JDK ones by name, this probe's with their line. */
        static String frames(List<StackFrame> frames) {
            StringBuilder sb = new StringBuilder();
            for (StackFrame f : frames) {
                Method m = f.location().method();
                String owner = m.declaringType().name();
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                sb.append(owner).append('.').append(m.name());
                if (m.isNative()) {
                    sb.append("[native]");
                } else if (owner.startsWith(PROBE)) {
                    sb.append(':').append(f.location().lineNumber());
                }
            }
            return sb.toString();
        }

        static void look(String name, ThreadReference t, String method) throws Exception {
            System.out.println("  " + name + " status: " + status(t) + " suspended=" + t.isSuspended());
            List<StackFrame> all = t.frames();
            System.out.println("  " + name + " frames: " + frames(all));
            StackFrame target = null;
            for (StackFrame f : all) {
                if (f.location().method().name().equals(method)) {
                    target = f;
                    break;
                }
            }
            if (target == null) {
                System.out.println("  " + name + " " + method + ": not listed");
                return;
            }
            String self;
            try {
                ObjectReference me = target.thisObject();
                self = me == null ? "static" : "<" + me.referenceType().name() + ">";
            } catch (Exception e) {
                self = e.getClass().getSimpleName();
            }
            System.out.println("  " + name + " this: " + self);
            String locals;
            try {
                List<LocalVariable> vars = new ArrayList<>(target.visibleVariables());
                vars.sort(Comparator.comparing(LocalVariable::name));
                Map<LocalVariable, Value> values = target.getValues(vars);
                StringBuilder sb = new StringBuilder();
                for (LocalVariable v : vars) {
                    if (sb.length() > 0) {
                        sb.append(' ');
                    }
                    sb.append(v.name()).append('=').append(show(values.get(v)));
                }
                locals = sb.toString();
            } catch (Exception e) {
                locals = e.getClass().getSimpleName();
            }
            System.out.println("  " + name + " locals: " + locals);
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
            ClassType type = (ClassType) awaitClass(PROBE);
            type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 240_000;
            while (((IntegerValue) type.getValue(type.fieldByName("ready"))).value() < 2) {
                if (System.currentTimeMillis() > until) {
                    fail("the workers never reached their sleeps");
                }
                Thread.sleep(50);
            }
            // Both have marked themselves; let them enter the native.
            Thread.sleep(500);
            vm.suspend();
            ThreadReference entry = null;
            ThreadReference osr = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("entry")) {
                    entry = t;
                } else if (t.name().equals("osr")) {
                    osr = t;
                }
            }
            if (entry == null || osr == null) {
                fail("no entry or osr thread");
            }
            System.out.println("== ready");
            look("entry", entry, "step");
            look("osr", osr, "spin");
            entry.interrupt();
            osr.interrupt();
            type.setValue(type.fieldByName("done"), vm.mirrorOf(1));
            vm.resume();
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
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
