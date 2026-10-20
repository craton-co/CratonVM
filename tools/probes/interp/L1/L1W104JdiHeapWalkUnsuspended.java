// Interpreter round i5 wave 104, lane L1: JDWP `VirtualMachine.InstanceCounts`,
// `ReferenceType.Instances` and `ObjectReference.ReferringObjects` with no
// thread suspended, and with unreachable instances on the heap.
//
// HotSpot's back end answers all three whatever the threads do, from JVMTI
// `FollowReferences` (`util.c` `classInstanceCounts`, `classInstances`,
// `getReferrers`): only objects reachable from the roots count, so garbage not
// yet collected is not answered. CratonVM (wave 103) walked every heap object,
// garbage included; since wave 104 it walks from the roots
// (vm/src/jvmti/native_env/heap.rs `debugger_heap_query`,
// `CRATONVM_JDWP_HEAP_WALK_FROM_ROOTS`, default on), on the session's heap
// service when no thread is parked (vm/src/debug/inspect.rs
// `run_heap_command`). Not built or run. Page:
// docs/internal/fixed-bugs/interpreter-i103-L1-jdwp-capabilities-not-served-FIXED-20261016.md
//
// The debuggee keeps three `Item`s (static `items`, `single` naming the
// second) and drops five more made in a method that has returned, then
// sleeps while the debugger asks, nothing suspended. A collection between
// the two leaves the same answer (the five are gone either way).
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=6
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W104JdiHeapWalkUnsuspended.java
//   cratonvm --java-home $JDK --jdwp-port 5799 -cp out L1W104JdiHeapWalkUnsuspended wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5799 \
//       -cp out L1W104JdiHeapWalkUnsuspended wait
//   java -cp out L1W104JdiHeapWalkUnsuspended debug 5799 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from util.c, NOT measured):
//
//   == attach
//     canGetInstanceInfo=true
//   == running, nothing suspended
//     instanceCounts Item=3
//     instances all=3 values=1,2,3
//     instances max 2=2
//     referrers of single=L1W104JdiHeapWalkUnsuspended$Item[],java.lang.Class
//   == end
//     disconnected
//
// With CRATONVM_JDWP_HEAP_WALK_FROM_ROOTS=0 CratonVM may count the five
// dropped `Item`s too (`instanceCounts Item=8`) unless a collection ran.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W104JdiHeapWalkUnsuspended {
    static final class Item {
        final int v;

        Item(int v) {
            this.v = v;
        }
    }

    static Item[] items;
    static Item single;
    static int dropped;
    /** Set by the debugger: 1 = start, 3 = done. 2 is the debuggee's ack. */
    static volatile int phase;

    /** Five `Item`s nothing keeps once this returns. */
    static void makeGarbage() {
        for (int i = 0; i < 5; i++) {
            Item garbage = new Item(100 + i);
            dropped += garbage.v > 0 ? 1 : 0;
        }
    }

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
        items = new Item[] {new Item(1), new Item(2), new Item(3)};
        single = items[1];
        makeGarbage();
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        int sum = 0;
        for (Item i : items) {
            sum += i.v;
        }
        System.out.println("sum=" + sum);
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

        static String sortedNames(List<ObjectReference> objects) {
            List<String> names = new ArrayList<>();
            for (ObjectReference o : objects) {
                names.add(o.referenceType().name());
            }
            Collections.sort(names);
            return String.join(",", names);
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
            System.out.println("  canGetInstanceInfo=" + vm.canGetInstanceInfo());
            type = (ClassType) awaitClass("L1W104JdiHeapWalkUnsuspended");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            await(2);
            System.out.println("== running, nothing suspended");
            if (vm.canGetInstanceInfo()) {
                ReferenceType item = awaitClass("L1W104JdiHeapWalkUnsuspended$Item");
                long[] counts = vm.instanceCounts(List.of(item));
                System.out.println("  instanceCounts Item=" + counts[0]);
                List<ObjectReference> all = item.instances(0);
                List<Integer> values = new ArrayList<>();
                for (ObjectReference o : all) {
                    values.add(((IntegerValue) o.getValue(item.fieldByName("v"))).value());
                }
                Collections.sort(values);
                StringBuilder joined = new StringBuilder();
                for (int v : values) {
                    joined.append(joined.length() == 0 ? "" : ",").append(v);
                }
                System.out.println("  instances all=" + all.size() + " values=" + joined);
                System.out.println("  instances max 2=" + item.instances(2).size());
                ObjectReference single = (ObjectReference) type.getValue(type.fieldByName("single"));
                System.out.println("  referrers of single=" + sortedNames(single.referringObjects(0)));
            }
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
