// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L1: a JDWP field watch keeps reporting
// after `RedefineClasses` makes a class name the watched field. The switch
// triage (docs/internal/reviews/interpreter-switch-triage-i6-20261018.md, row
// 12) found `CRATONVM_JDWP_SCOPED_FIELD_WATCHES` unsound: a class redefined
// while a watch stands was never matched again, so its new code could run
// compiled and miss `FieldAccess` / `FieldModification`. Since wave 116 the
// redefinition's new class file joins the scope before it is installed
// (vm/src/debug/field_watch_scope.rs `join_redefinitions`). Page:
// docs/internal/fixed-bugs/interpreter-i10-L1-proposal-exception-and-watch-events-from-compiled-code-DONE-20261224.md,
// "Wave 116".
//
// `Bumper.bump()` increments `Counter.decoy`. The debugger watches
// `Counter.watch` (access and modification, SUSPEND_NONE), lets the debuggee
// call `bump()` 20000 times (no event: the old body names only `decoy`), then
// redefines `Bumper` with the constant pool's "decoy" spelled "watch" (the
// same length; the method bytes are unchanged), and lets the debuggee call it
// 20000 times more. Every call must post one access and one modification.
//
// Plain run (no debugger), stdout as HotSpot 25:
//
//   decoy=40000 watch=0
//
// Under a debugger (both modes; JIT and --nojit; CRATONVM_JDWP_SCOPED_FIELD_WATCHES
// off and =1; a build with --features cratonvm-vm/experimental-debug):
//
//   javac -g -d out L1W116JdiFieldWatchAfterRedefine.java
//   cratonvm --java-home $JDK --jdwp-port 5799 -cp out L1W116JdiFieldWatchAfterRedefine wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5799 \
//       -cp out L1W116JdiFieldWatchAfterRedefine wait
//   java -cp out L1W116JdiFieldWatchAfterRedefine debug 5799 > transcript.txt
//
// Transcript, HotSpot 25.0.3 as the debuggee (Windows, run in wave 116):
//
//   == attach
//     canRedefineClasses=true canWatchFieldAccess=true canWatchFieldModification=true
//   == old body
//     accesses=0 modifications=0
//   == redefine
//     redefined
//   == new body
//     accesses=20000 modifications=20000 watch=20000
//   == end
//     disconnected
//
// The debuggee's own stdout under the debugger: `decoy=20000 watch=20000`.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.io.*;
import java.util.*;

public class L1W116JdiFieldWatchAfterRedefine {
    static final int CALLS = 20_000;

    static final class Counter {
        static int watch;
        static int decoy;
    }

    static final class Bumper {
        static void bump() {
            Counter.decoy++;
        }
    }

    /** Set by the debugger: 1 = run the old body, 3 = the new, 5 = done. 2 and 4 are acks. */
    static volatile int phase;

    static void await(boolean debugged, int value, long until) throws InterruptedException {
        while (debugged && phase != value && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        // Loaded and prepared before the debugger looks for them.
        Class.forName(Counter.class.getName());
        Class.forName(Bumper.class.getName());
        long until = System.currentTimeMillis() + 300_000;
        await(debugged, 1, until);
        for (int i = 0; i < CALLS; i++) {
            Bumper.bump();
        }
        if (debugged) {
            phase = 2;
        }
        await(debugged, 3, until);
        for (int i = 0; i < CALLS; i++) {
            Bumper.bump();
        }
        if (debugged) {
            phase = 4;
        }
        await(debugged, 5, until);
        System.out.println("decoy=" + Counter.decoy + " watch=" + Counter.watch);
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType main;
        static int accesses;
        static int modifications;

        static int phaseNow() {
            return ((IntegerValue) main.getValue(main.fieldByName("phase"))).value();
        }

        /** Count watch events until the debuggee acknowledges `value`, then drain. */
        static void countUntil(int value) throws Exception {
            long until = System.currentTimeMillis() + 180_000;
            boolean acked = false;
            while (true) {
                EventSet set = vm.eventQueue().remove(acked ? 1_000 : 100);
                if (set == null) {
                    if (acked) {
                        return;
                    }
                    acked = phaseNow() == value;
                    if (System.currentTimeMillis() > until) {
                        fail("phase " + value + " never came");
                    }
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof ModificationWatchpointEvent) {
                        modifications++;
                    } else if (e instanceof AccessWatchpointEvent) {
                        accesses++;
                    }
                }
                set.resume();
                if (!acked) {
                    acked = phaseNow() == value;
                }
            }
        }

        /** `bytes` with its one CONSTANT_Utf8 "decoy" spelled "watch". */
        static byte[] decoyNamedWatch(byte[] bytes) {
            byte[] from = {1, 0, 5, 'd', 'e', 'c', 'o', 'y'};
            byte[] to = {1, 0, 5, 'w', 'a', 't', 'c', 'h'};
            byte[] out = bytes.clone();
            int found = -1;
            for (int i = 0; i + from.length <= out.length; i++) {
                if (Arrays.equals(out, i, i + from.length, from, 0, from.length)) {
                    if (found >= 0) {
                        fail("\"decoy\" is not unique");
                    }
                    found = i;
                }
            }
            if (found < 0) {
                fail("no \"decoy\" in the pool");
            }
            System.arraycopy(to, 0, out, found, to.length);
            return out;
        }

        static byte[] bumperBytes() throws IOException {
            String resource = "L1W116JdiFieldWatchAfterRedefine$Bumper.class";
            try (InputStream in =
                    L1W116JdiFieldWatchAfterRedefine.class.getResourceAsStream(resource)) {
                if (in == null) {
                    fail("no " + resource);
                }
                return in.readAllBytes();
            }
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
            System.out.println("  canRedefineClasses=" + vm.canRedefineClasses()
                    + " canWatchFieldAccess=" + vm.canWatchFieldAccess()
                    + " canWatchFieldModification=" + vm.canWatchFieldModification());
            main = (ClassType) awaitClass("L1W116JdiFieldWatchAfterRedefine");
            ReferenceType counter = awaitClass("L1W116JdiFieldWatchAfterRedefine$Counter");
            ReferenceType bumper = awaitClass("L1W116JdiFieldWatchAfterRedefine$Bumper");
            Field watch = counter.fieldByName("watch");
            EventRequestManager erm = vm.eventRequestManager();
            AccessWatchpointRequest access = erm.createAccessWatchpointRequest(watch);
            access.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            access.enable();
            ModificationWatchpointRequest modification =
                    erm.createModificationWatchpointRequest(watch);
            modification.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            modification.enable();
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(1));
            countUntil(2);
            System.out.println("== old body");
            System.out.println("  accesses=" + accesses + " modifications=" + modifications);
            accesses = 0;
            modifications = 0;
            System.out.println("== redefine");
            try {
                vm.redefineClasses(Map.of(bumper, decoyNamedWatch(bumperBytes())));
                System.out.println("  redefined");
            } catch (RuntimeException | LinkageError e) {
                System.out.println("  refused " + e.getClass().getName());
            }
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(3));
            countUntil(4);
            int value = ((IntegerValue) counter.getValue(watch)).value();
            System.out.println("== new body");
            System.out.println("  accesses=" + accesses + " modifications=" + modifications
                    + " watch=" + value);
            access.disable();
            modification.disable();
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(5));
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
