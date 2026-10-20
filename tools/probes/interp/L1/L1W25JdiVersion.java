// Interpreter round i1 wave 25, lane L1: the JDI conformance harness's
// `version` scenario, for
// docs/internal/fixed-bugs/interpreter-L1-proposal-answer-jdwp-as-the-jdk-it-runs-FIXED-20260930.md:
// what JDI decides from `VirtualMachine.Version` — the JDWP version, whether
// modules and virtual threads exist — and the commands it then sends:
// ReferenceType.Module, VirtualMachine.AllModules, ModuleReference.Name /
// ClassLoader, ThreadReference.IsVirtual, and a ThreadStart request with the
// PlatformThreadsOnly modifier (jdb's default thread requests).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   virtual thread ran
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W25JdiVersion.java
//   cratonvm --java-home $JDK --jdwp-port 5781 -cp out L1W25JdiVersion wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5781 -cp out L1W25JdiVersion wait)
//   java -cp out L1W25JdiVersion debug 5781 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//     version is the debuggee's java.version=true
//     canGetModuleInfo=true
//   == stop at checkpoint thread=main
//     platform-only thread starts=[platform-started]
//   == modules
//     L1W25JdiVersion module named=false loader=jdk.internal.loader.ClassLoaders$AppClassLoader
//     String module=java.base loader=null
//     allModules lists java.base=true
//   == virtual threads
//     main isVirtual=false
//     virtual-one is a ThreadReference isVirtual=true
//   == end
//   vm death
//   disconnected
//
// The JDWP commands behind it that a JDWP 1.8 target is never sent (a
// tools/jdi/JdwpTap.java log of this session against HotSpot 25): 1/22
// AllModules, 2/19 ReferenceType.Module, 18/1 ModuleReference.Name (once
// per listed module; "" for an unnamed one), 18/2 ModuleReference.ClassLoader,
// 11/15 ThreadReference.IsVirtual, and EventRequest.Set with modifier 13
// PlatformThreadsOnly (JDI drops the filter for a target below JDWP 19).
//
// CratonVM before wave 25 (from reading the code): `version is the
// debuggee's java.version=false` (it answered "1.8.0"),
// `canGetModuleInfo=false`, `modules unsupported`, both threads
// `isVirtual=false` (JDI did not ask), and the ThreadStart request without
// its filter reported `[platform-started, virtual-one]` if the virtual
// thread's start was reported at all.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W25JdiVersion {
    static volatile boolean attached;
    static volatile boolean release;
    static volatile Thread virtualRef;

    static void checkpoint(int count) {
        int twice = count * 2;
        if (twice < 0) {
            System.out.println("never");
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            waitForDebugger();
        }
        Thread platform = new Thread(() -> { }, "platform-started");
        platform.start();
        platform.join();
        Thread v = Thread.ofVirtual().name("virtual-one").unstarted(() -> {
            while (!release) {
                Thread.onSpinWait();
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
            System.out.println("virtual thread ran");
        });
        virtualRef = v;
        v.start();
        checkpoint(1);
        release = true;
        v.join();
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
            String version = vm.version();
            System.out.println("  version is the debuggee's java.version="
                    + version.equals(System.getProperty("java.version")));
            System.out.println("  canGetModuleInfo=" + vm.canGetModuleInfo());
            ReferenceType type = awaitClass("L1W25JdiVersion");
            ThreadStartRequest starts = erm.createThreadStartRequest();
            starts.addPlatformThreadsOnlyFilter();
            starts.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            starts.enable();
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its first
            // SUSPEND_ALL stop, and release that stop.
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            List<String> started = new ArrayList<>();
            ThreadReference main = awaitStop(started);
            Collections.sort(started);
            System.out.println("  platform-only thread starts=" + started);

            System.out.println("== modules");
            try {
                ModuleReference m = type.module();
                System.out.println("  L1W25JdiVersion module named=" + (m.name() != null)
                        + " loader=" + (m.classLoader() == null ? "null"
                                : m.classLoader().referenceType().name()));
                ReferenceType string = vm.classesByName("java.lang.String").get(0);
                ModuleReference base = string.module();
                System.out.println("  String module=" + base.name() + " loader="
                        + (base.classLoader() == null ? "null" : "not null"));
                boolean listed = false;
                for (ModuleReference each : vm.allModules()) {
                    if ("java.base".equals(each.name())) {
                        listed = true;
                    }
                }
                System.out.println("  allModules lists java.base=" + listed);
            } catch (UnsupportedOperationException e) {
                System.out.println("  modules unsupported");
            }

            System.out.println("== virtual threads");
            System.out.println("  main isVirtual=" + main.isVirtual());
            Value v = type.getValue(type.fieldByName("virtualRef"));
            if (v instanceof ThreadReference t) {
                System.out.println("  virtual-one is a ThreadReference isVirtual=" + t.isVirtual());
            } else {
                System.out.println("  virtual-one: " + (v == null ? "null" : v.getClass().getSimpleName()));
            }

            System.out.println("== end");
            erm.deleteAllBreakpoints();
            vm.resume();
            while (true) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    System.out.println("disconnected");
                    return;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDeathEvent) {
                        System.out.println("vm death");
                    } else if (e instanceof VMDisconnectEvent) {
                        System.out.println("disconnected");
                        return;
                    }
                }
                set.resume();
            }
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

        static ThreadReference awaitStop(List<String> started) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof ThreadStartEvent s) {
                        String n = s.thread().name();
                        if (n.equals("platform-started") || n.equals("virtual-one")) {
                            started.add(n);
                        }
                    }
                    if (e instanceof BreakpointEvent b) {
                        System.out.println("== stop at " + b.location().method().name()
                                + " thread=" + b.thread().name());
                        return b.thread();
                    }
                    if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
            }
            fail("no breakpoint");
            return null;
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
