// Interpreter round i5 wave 103, lane L1: JDWP `VirtualMachine.InstanceCounts`,
// `ReferenceType.Instances` and `ObjectReference.ReferringObjects`
// (`canGetInstanceInfo`), and `ReferenceType.ConstantPool`
// (`canGetConstantPool`), through JDI.
//
// HotSpot's back end walks the heap (JVMTI `IterateThroughHeap` for the
// counts, `FollowReferences` for the instances and the referrers) and reads
// the class's constant pool (JVMTI `GetConstantPool`, the class file's pool
// for a plain class). CratonVM answered both capabilities false and served
// none of the commands; since wave 103 it serves them, the heap walks on a
// thread parked at an event (vm/src/debug/inspect.rs `heap_walk_command`,
// vm/src/jvmti/native_env/heap.rs `debugger_heap_query`) and the pool from
// the class file (vm/src/debug/mod.rs `constant_pool`). Not built or run.
//
// The debugger stops `main` at a breakpoint in `stop()` (SUSPEND_ALL), with
// three `Item`s alive in `items` and `single` naming the second one, so a
// thread is parked when it asks. The `Item`s are made after a `System.gc()`
// and nothing else makes one, so no unreachable `Item` exists.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=6
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W103JdiInstances.java
//   cratonvm --java-home $JDK --jdwp-port 5796 -cp out L1W103JdiInstances wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5796 \
//       -cp out L1W103JdiInstances wait
//   java -cp out L1W103JdiInstances debug 5796 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from util.c, ReferenceTypeImpl.c and
// ObjectReferenceImpl.c, NOT measured; the crc is the class file's pool):
//
//   == attach
//     canGetInstanceInfo=true canGetConstantPool=true
//   == stopped in stop
//     instanceCounts Item=3 Item[]=1
//     instances all=3 values=1,2,3
//     instances max 2=2
//     referrers of single=L1W103JdiInstances$Item[],java.lang.Class
//     referrers max 1=1
//     constantPool count=<n> bytes=<m> crc=<c>
//   == end
//     disconnected
//
// (`<n>`, `<m>`, `<c>` are whatever javac wrote for `Item`: the runner
// compares CratonVM's transcript with HotSpot's measured one.)
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W103JdiInstances {
    static final class Item {
        final int v;

        Item(int v) {
            this.v = v;
        }

        String label() {
            return "item-" + v;
        }
    }

    static Item[] items;
    static Item single;
    /** Set by the debugger: 1 = start. */
    static volatile int phase;

    static void stop() {
        // The debugger's breakpoint.
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
        System.gc();
        items = new Item[] {new Item(1), new Item(2), new Item(3)};
        single = items[1];
        stop();
        int sum = 0;
        for (Item i : items) {
            sum += i.v;
        }
        System.out.println("sum=" + sum);
    }

    static final class Debugger {
        static VirtualMachine vm;

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
            System.out.println("  canGetInstanceInfo=" + vm.canGetInstanceInfo()
                    + " canGetConstantPool=" + vm.canGetConstantPool());
            ClassType type = (ClassType) awaitClass("L1W103JdiInstances");
            Method stop = type.methodsByName("stop").get(0);
            BreakpointRequest bp = vm.eventRequestManager().createBreakpointRequest(stop.location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            EventSet stopped = null;
            while (stopped == null) {
                EventSet next = vm.eventQueue().remove(120_000);
                if (next == null) {
                    fail("no breakpoint");
                }
                for (Event e : next) {
                    if (e instanceof BreakpointEvent) {
                        stopped = next;
                    }
                }
                if (stopped == null) {
                    next.resume();
                }
            }
            System.out.println("== stopped in stop");
            ReferenceType item = vm.classesByName("L1W103JdiInstances$Item").get(0);
            ReferenceType itemArray = vm.classesByName("L1W103JdiInstances$Item[]").get(0);
            if (vm.canGetInstanceInfo()) {
                long[] counts = vm.instanceCounts(List.of(item, itemArray));
                System.out.println("  instanceCounts Item=" + counts[0] + " Item[]=" + counts[1]);
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
                System.out.println("  referrers max 1=" + single.referringObjects(1).size());
            }
            if (vm.canGetConstantPool()) {
                byte[] pool = item.constantPool();
                java.util.zip.CRC32 crc = new java.util.zip.CRC32();
                crc.update(pool);
                System.out.println("  constantPool count=" + item.constantPoolCount()
                        + " bytes=" + pool.length + " crc=" + Long.toHexString(crc.getValue()));
            }
            bp.disable();
            stopped.resume();
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
