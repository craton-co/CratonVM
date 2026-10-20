// Interpreter round i5 wave 106, lane L1: JDWP hides a JDK class loaded by
// `Class.forName(name, false, null)` BEFORE the debugger attached, and never
// linked, until it is prepared (the residue of
// docs/internal/fixed-bugs/interpreter-i102-L1-jdwp-lists-unprepared-classes-FIXED-20261016.md).
//
// HotSpot's back end leaves a class whose JVMTI status has neither PREPARED
// nor ARRAY out of `AllClasses(WithGeneric)` and `ClassesBySignature`,
// whenever it was loaded. Wave 104 hid such a class only when it was defined
// after JDI's first `ClassPrepare` request (L1W104JdiUnlinkedJdkClassHidden);
// one defined before stayed listed, since this VM may use a trivially linked
// class unlinked while nothing observes `ClassPrepare`. Since wave 106 the two
// paths that do (the JIT `new` row walk, the stackless exceptions) record the
// classes they use (vm/src/debug/mod.rs `UsedUnlinkedClasses`), and a class
// defined before the request that no path used is hidden too
// (`CRATONVM_JDWP_HIDE_UNUSED_UNLINKED_CLASSES`, default on; `=0` lists it).
// Not built or run.
//
// The debuggee loads the class first thing in `main`; the debugger waits
// 3 s before it attaches, so the load precedes the attach. Were the attach
// first, the answer is the same (wave 104's rule then hides it), so the
// transcript does not depend on the order; only the `=0` arm or a build
// without the fix (`classesByName=1 inAllClasses=true` in the first block)
// tells the two rules apart. `java.time.chrono.MinguoEra` is loaded by nothing
// at start-up nor by JDI's attach.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (in tools/jdi/run-jdi-conformance.sh's default list):
//
//   javac -g -d out L1W106JdiUnlinkedJdkClassLoadedBeforeAttach.java
//   cratonvm --java-home $JDK --jdwp-port 5797 -cp out L1W106JdiUnlinkedJdkClassLoadedBeforeAttach wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5797 //       -cp out L1W106JdiUnlinkedJdkClassLoadedBeforeAttach wait
//   java -cp out L1W106JdiUnlinkedJdkClassLoadedBeforeAttach debug 5797 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from VirtualMachineImpl.c, not measured):
//
//   == attach
//   == loaded before the attach, not linked
//     classesByName=0 inAllClasses=false
//   == initialized
//     classesByName=1 prepared=true initialized=true inAllClasses=true
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W106JdiUnlinkedJdkClassLoadedBeforeAttach {
    /** Set by the debugger: 1 = start, 3 = initialize, 5 = done. 2 and 4 are the debuggee's acks. */
    static volatile int phase;

    static final String LAZY = "java.time.chrono.MinguoEra";

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        // Loaded, not linked, before any debugger can have attached.
        Class<?> lazy = Class.forName(LAZY, false, null);
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Class.forName(LAZY, true, null);
        if (lazy.getClassLoader() != null) {
            System.out.println("not a bootstrap class");
        }
        if (debugged) {
            phase = 4;
        }
        while (debugged && phase != 5 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static void await(int value) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("phase"))).value() != value) {
                if (System.currentTimeMillis() > until) {
                    fail("phase " + value + " never came");
                }
                Thread.sleep(20);
            }
        }

        static boolean inAllClasses() {
            for (ReferenceType t : vm.allClasses()) {
                if (t.name().equals(LAZY)) {
                    return true;
                }
            }
            return false;
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
            // Let the debuggee define the class before the attach.
            Thread.sleep(3_000);
            vm = attach(socket, a);
            System.out.println("== attach");
            type = (ClassType) awaitClass("L1W106JdiUnlinkedJdkClassLoadedBeforeAttach");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            await(2);
            System.out.println("== loaded before the attach, not linked");
            System.out.println("  classesByName=" + vm.classesByName(LAZY).size()
                    + " inAllClasses=" + inAllClasses());
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            await(4);
            System.out.println("== initialized");
            // JDI answers `classesByName` from its type cache once
            // `allClasses()` filled it, and adds a class to the cache when
            // its (internal) `ClassPrepare` event is handled, on JDI's own
            // thread: wait for that, bounded, so the line does not race it
            // (the w104c HotSpot run printed `classesByName=0` here once).
            List<ReferenceType> found = vm.classesByName(LAZY);
            long settle = System.currentTimeMillis() + 30_000;
            while ((found.isEmpty() || !found.get(0).isPrepared())
                    && System.currentTimeMillis() < settle) {
                Thread.sleep(50);
                found = vm.classesByName(LAZY);
            }
            String detail = found.isEmpty() ? "" : " prepared=" + found.get(0).isPrepared()
                    + " initialized=" + found.get(0).isInitialized();
            System.out.println("  classesByName=" + found.size() + detail
                    + " inAllClasses=" + inAllClasses());
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(5));
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
