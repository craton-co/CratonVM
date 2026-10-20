// Interpreter round i5 wave 102, lane L1: JDWP `ThreadGroupReference.Children`
// lists platform threads only.
//
// HotSpot's back end answers `Children` from JVMTI `GetThreadGroupChildren`,
// which returns live platform threads (JDK 21+; a virtual thread is never on
// HotSpot's threads list), minus the debugger's own threads;
// `includevirtualthreads=y` adds virtual threads to `AllThreads` only. So the
// `VirtualThreads` group (`java.lang.Thread$Constants.VTHREAD_GROUP`, a child
// of `system`) has no thread children, whatever runs in it. CratonVM's live
// server listed every live virtual thread there (vm/src/debug/inspect.rs,
// `thread_group_reference`), and the bare `system` model listed them with
// `CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS=1`. Fixed, not built or run; the
// JVMTI twin is L1W102JvmtiPlatformThreadsOnly.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L1W102JdiGroupChildren.java
//   cratonvm --java-home $JDK --jdwp-port 5792 -cp out L1W102JdiGroupChildren wait
//   # HotSpot reference debuggee:
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5792 \
//       -cp out L1W102JdiGroupChildren wait
//   java -cp out L1W102JdiGroupChildren debug 5792 > transcript.txt
//   # and once more with CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS=1 on CratonVM
//   # (HotSpot: includevirtualthreads=y) -- the only row that changes is
//   # `allThreads virtual=3`.
//
// HotSpot 25 as the debuggee (derived from ThreadGroupReferenceImpl.c and the
// JVMTI specification, not measured):
//
//   == attach
//     group VirtualThreads threads=0 virtual=0
//     group main p-one=true virtual=0
//     allThreads virtual=0
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;
import java.util.concurrent.CountDownLatch;

public class L1W102JdiGroupChildren {
    /** Set by the debugger: 1 = start, 3 = done. 2 once every thread runs. */
    static volatile int phase;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        CountDownLatch release = new CountDownLatch(1);
        CountDownLatch started = new CountDownLatch(4);
        Runnable hold = () -> {
            started.countDown();
            try {
                release.await();
            } catch (InterruptedException ignored) {
                // Ends the thread.
            }
        };
        List<Thread> all = new ArrayList<>();
        Thread platform = new Thread(hold, "p-one");
        platform.setDaemon(true);
        platform.start();
        all.add(platform);
        for (int i = 0; i < 3; i++) {
            all.add(Thread.ofVirtual().name("v-" + i).start(hold));
        }
        started.await();
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        release.countDown();
        for (Thread t : all) {
            t.join();
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;

        static ThreadGroupReference find(List<ThreadGroupReference> groups, String name) {
            for (ThreadGroupReference g : groups) {
                if (g.name().equals(name)) {
                    return g;
                }
                ThreadGroupReference inner = find(g.threadGroups(), name);
                if (inner != null) {
                    return inner;
                }
            }
            return null;
        }

        static int virtuals(List<ThreadReference> threads) {
            int n = 0;
            for (ThreadReference t : threads) {
                if (t.name().startsWith("v-")) {
                    n++;
                }
            }
            return n;
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
            ClassType type = (ClassType) awaitClass("L1W102JdiGroupChildren");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("phase"))).value() < 2) {
                if (System.currentTimeMillis() > until) {
                    fail("the threads never started");
                }
                Thread.sleep(20);
            }
            vm.suspend();
            ThreadGroupReference vgroup = find(vm.topLevelThreadGroups(), "VirtualThreads");
            if (vgroup == null) {
                System.out.println("  group VirtualThreads missing");
            } else {
                List<ThreadReference> threads = vgroup.threads();
                System.out.println("  group VirtualThreads threads=" + threads.size()
                        + " virtual=" + virtuals(threads));
            }
            ThreadGroupReference main = find(vm.topLevelThreadGroups(), "main");
            if (main == null) {
                System.out.println("  group main missing");
            } else {
                boolean one = false;
                for (ThreadReference t : main.threads()) {
                    one |= t.name().equals("p-one");
                }
                System.out.println("  group main p-one=" + one + " virtual=" + virtuals(main.threads()));
            }
            System.out.println("  allThreads virtual=" + virtuals(vm.allThreads()));
            vm.resume();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
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
