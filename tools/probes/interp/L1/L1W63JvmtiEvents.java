// The C JVMTI events the table still answered with
// JVMTI_ERROR_NOT_AVAILABLE (98) after wave 61, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md,
// "Next step (from wave 61)", items 1-5): `FieldAccess` / `FieldModification`
// (63 / 64), `NativeMethodBind` (67), `CompiledMethodLoad` /
// `DynamicCodeGenerated` (68 / 70, and `GenerateEvents`), the monitor events
// (73-76), `ResourceExhausted` (80), `GarbageCollectionStart` / `Finish`
// (81 / 82), `ObjectFree` (83), `VMObjectAlloc` (84), `SampledObjectAlloc`
// (86), `VirtualThreadStart` / `End` (87 / 88), HotSpot's extension events
// (`ClassUnload`, `VirtualThreadMount` / `Unmount`), and `ThreadStart` of a
// `RunAgentThread` thread.
//
// The startup agent adds every capability it is offered; each section
// enables its events, runs the case, and prints what the callbacks
// recorded about the probe's own objects and threads.
//
// Needs the native shim tools/probes/interp/L1/L1W63JvmtiEvents.c, loaded both
// as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w63ev.so tools/probes/interp/L1/L1W63JvmtiEvents.c
//   javac -d /tmp/l1w63ev tools/probes/interp/L1/L1W63JvmtiEvents.java
//   java     -agentpath:/tmp/libl1w63ev.so -cp /tmp/l1w63ev L1W63JvmtiEvents /tmp/libl1w63ev.so
//   cratonvm -agentpath:/tmp/libl1w63ev.so -cp /tmp/l1w63ev L1W63JvmtiEvents /tmp/libl1w63ev.so
//
// The Rust port of the shim is tools/probes/interp/L1/rust-ports/L1W63JvmtiEvents.
//
// Expected stdout (HotSpot 25.0.3): see the block at the end of this file.
import java.lang.reflect.Array;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

public class L1W63JvmtiEvents {
    /** Enable (or disable) event `event` for all threads: the error. */
    static native int enable(int event, boolean on);

    /** The lines the callbacks recorded since the last call, cleared. */
    static native String drain();

    /** Field watches: `Set/ClearField{Access,Modification}Watch` of field
     *  `name` of `klass`; `access` picks the kind, `set` set or clear. */
    static native int watch(Class<?> klass, String name, String sig, boolean isStatic, boolean access, boolean set);

    /** `GenerateEvents(event)`: the error, then the count of the events. */
    static native String generate(int event);

    /** `SetHeapSamplingInterval(n)`. */
    static native int sampling(int interval);

    /** `SetExtensionEventCallback(index, recorder or NULL)`. */
    static native int extension(int index, boolean on);

    /** `RunAgentThread` of `t` with a function that returns at once. */
    static native int runAgent(Thread t);

    /** `AllocObject` / `NewObject` of `klass` through JNI. */
    static native Object jniAlloc(Class<?> klass, boolean construct);

    /** A native bound lazily (the first call links it). */
    static native int boundLater(int x);

    /** `RegisterNatives` of `registered()I` to the shim's function. */
    static native int registerNow();

    static native int registered();

    /** A native whose binding the agent's `NativeMethodBind` callback
     *  replaces with its own function (which answers 77). */
    static native int replaced();

    /** `SetTag(o, tag)`. */
    static native int tag(Object o, long tag);

    // ---------------------------------------------------------------------

    public static class Watched {
        public int counter;
        public static String name = "start";
        public long untouched;
    }

    static void fieldRows() {
        Watched w = new Watched();
        System.out.println("watch access counter: err=" + watch(Watched.class, "counter", "I", false, true, true));
        System.out.println("watch modification counter: err=" + watch(Watched.class, "counter", "I", false, false, true));
        System.out.println("watch modification name: err=" + watch(Watched.class, "name", "Ljava/lang/String;", true, false, true));
        System.out.println("watch access counter again: err=" + watch(Watched.class, "counter", "I", false, true, true));
        System.out.println("clear access untouched: err=" + watch(Watched.class, "untouched", "J", false, true, false));
        System.out.println("enable FieldAccess: err=" + enable(63, true));
        System.out.println("enable FieldModification: err=" + enable(64, true));
        touch(w);
        enable(63, false);
        enable(64, false);
        System.out.print(drain());
        System.out.println("clear access counter: err=" + watch(Watched.class, "counter", "I", false, true, false));
        System.out.println("clear modification counter: err=" + watch(Watched.class, "counter", "I", false, false, false));
        System.out.println("clear modification name: err=" + watch(Watched.class, "name", "Ljava/lang/String;", true, false, false));
    }

    static void touch(Watched w) {
        w.counter = w.counter + 5;
        Watched.name = "changed";
        w.untouched = 9;
    }

    // ---------------------------------------------------------------------

    static void monitorRows() throws Exception {
        Object lock = new Object();
        tag(lock, 1);
        System.out.println("enable monitor events: err=" + enable(73, true) + " " + enable(74, true) + " "
            + enable(75, true) + " " + enable(76, true));
        CountDownLatch held = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        Thread holder = new Thread(() -> {
            synchronized (lock) {
                held.countDown();
                try {
                    release.await();
                } catch (InterruptedException e) {
                    // done
                }
            }
        }, "l1w63-holder");
        holder.start();
        held.await();
        Thread contender = new Thread(() -> {
            synchronized (lock) {
                lock.hashCode();
            }
        }, "l1w63-contender");
        contender.start();
        for (int i = 0; i < 500 && contender.getState() != Thread.State.BLOCKED; i++) {
            Thread.sleep(10);
        }
        release.countDown();
        contender.join();
        holder.join();
        synchronized (lock) {
            lock.wait(20);
        }
        Thread notifier = new Thread(() -> {
            try {
                Thread.sleep(200);
            } catch (InterruptedException e) {
                // done
            }
            synchronized (lock) {
                lock.notifyAll();
            }
        }, "l1w63-notifier");
        synchronized (lock) {
            notifier.start();
            lock.wait();
        }
        notifier.join();
        enable(73, false);
        enable(74, false);
        enable(75, false);
        enable(76, false);
        System.out.print(drain());
    }

    // ---------------------------------------------------------------------

    static void gcRows() throws Exception {
        System.out.println("enable gc: err=" + enable(81, true) + " " + enable(82, true));
        System.gc();
        enable(81, false);
        enable(82, false);
        System.out.print(drain());
        System.out.println("enable ObjectFree: err=" + enable(83, true));
        Object doomed = new Object();
        tag(doomed, 4242);
        doomed = null;
        String seen = "";
        for (int i = 0; i < 20 && !seen.contains("4242"); i++) {
            System.gc();
            Thread.sleep(50);
            seen += drain();
        }
        enable(83, false);
        System.out.println("object free 4242: " + (seen.contains("free tag=4242") ? "posted" : "not posted"));
    }

    // ---------------------------------------------------------------------

    public static class Made implements Cloneable {
        public int value = 7;

        public Made() {
        }

        @Override
        public Made clone() throws CloneNotSupportedException {
            return (Made) super.clone();
        }
    }

    static void allocRows() throws Exception {
        Made template = new Made();
        int[] numbers = {1, 2, 3};
        System.out.println("enable VMObjectAlloc: err=" + enable(84, true));
        Made bytecode = new Made();
        Made cloned = template.clone();
        int[] arrayClone = numbers.clone();
        Object array = Array.newInstance(Made.class, 3);
        Object multi = Array.newInstance(int.class, 2, 2);
        Object reflective = Made.class.getDeclaredConstructor().newInstance();
        Object allocated = jniAlloc(Made.class, false);
        Object constructed = jniAlloc(Made.class, true);
        enable(84, false);
        System.out.print(drain());
        System.out.println("kept: " + (bytecode.value + cloned.value + arrayClone.length + Array.getLength(array)
            + Array.getLength(multi) + ((Made) reflective).value + (allocated != null ? 1 : 0)
            + (constructed != null ? 1 : 0)));
    }

    public static class Sampled {
        public long a;
        public long b;
    }

    static Object sink;

    static void sampledRows() {
        System.out.println("sampling 0: err=" + sampling(0));
        System.out.println("enable SampledObjectAlloc: err=" + enable(86, true));
        // HotSpot applies a new interval at the thread's next TLAB refill, so
        // how many of a few allocations it samples varies; many allocations
        // are sampled for certain.
        for (int i = 0; i < 200_000; i++) {
            sink = new Sampled();
        }
        enable(86, false);
        System.out.print(drain());
        System.out.println("sampling 524288: err=" + sampling(524288));
    }

    // ---------------------------------------------------------------------

    static void bindRows() {
        System.out.println("enable NativeMethodBind: err=" + enable(67, true));
        System.out.println("boundLater: " + boundLater(20));
        System.out.println("registerNow: err=" + registerNow());
        System.out.println("registered: " + registered());
        System.out.println("replaced: " + replaced());
        enable(67, false);
        System.out.print(drain());
    }

    static void exhaustedRows() {
        System.out.println("enable ResourceExhausted: err=" + enable(80, true));
        try {
            sink = new long[Integer.MAX_VALUE];
        } catch (OutOfMemoryError e) {
            System.out.println("array limit: " + e.getMessage());
        }
        sink = null;
        enable(80, false);
        System.out.print(drain());
    }

    // ---------------------------------------------------------------------

    static void virtualRows() throws Exception {
        System.out.println("enable VirtualThreadStart/End: err=" + enable(87, true) + " " + enable(88, true));
        System.out.println("mount callbacks: err=" + extension(48, true) + " " + extension(47, true));
        System.out.println("enable mount events: err=" + enable(48, true) + " " + enable(47, true));
        Thread vt = Thread.ofVirtual().name("l1w63-vt").unstarted(() -> {
            LockSupport.park();
        });
        vt.start();
        for (int i = 0; i < 500 && vt.getState() != Thread.State.WAITING; i++) {
            Thread.sleep(10);
        }
        LockSupport.unpark(vt);
        vt.join();
        enable(87, false);
        enable(88, false);
        enable(48, false);
        enable(47, false);
        extension(48, false);
        extension(47, false);
        System.out.print(drain());
    }

    static void unloadRows() throws Exception {
        System.out.println("unload callback: err=" + extension(49, true) + " enable: err=" + enable(49, true));
        byte[] bytes;
        try (java.io.InputStream in = L1W63JvmtiEvents.class.getResourceAsStream("/L1W63Unloaded.class")) {
            bytes = in.readAllBytes();
        }
        ClassLoader loader = new ClassLoader(null) {
            @Override
            protected Class<?> findClass(String name) throws ClassNotFoundException {
                if (name.equals("L1W63Unloaded")) {
                    return defineClass(name, bytes, 0, bytes.length);
                }
                throw new ClassNotFoundException(name);
            }
        };
        Class<?> c = loader.loadClass("L1W63Unloaded");
        System.out.println("defined: " + c.getName());
        c = null;
        loader = null;
        String seen = "";
        for (int i = 0; i < 30 && !seen.contains("L1W63Unloaded"); i++) {
            System.gc();
            Thread.sleep(50);
            seen += drain();
        }
        enable(49, false);
        extension(49, false);
        System.out.println("class unload: " + (seen.contains("unload L1W63Unloaded") ? "posted" : "not posted"));
    }

    // ---------------------------------------------------------------------

    static int hot(int x) {
        int s = 0;
        for (int i = 0; i < 100; i++) {
            s += (x ^ i) & 7;
        }
        return s;
    }

    static void codeRows() throws Exception {
        System.out.println("enable CompiledMethodLoad: err=" + enable(68, true));
        long sum = 0;
        for (int i = 0; i < 200_000; i++) {
            sum += hot(i);
        }
        String seen = drain();
        for (int i = 0; i < 50 && !seen.contains("load hot"); i++) {
            Thread.sleep(50);
            sum += hot(i);
            seen += drain();
        }
        enable(68, false);
        System.out.println("hot compiled: " + (seen.contains("load hot") ? "posted" : "not posted") + " sum>0=" + (sum > 0));
        System.out.println("generate CompiledMethodLoad: " + generate(68));
        System.out.println("generate DynamicCodeGenerated: " + generate(70));
        System.out.println("generate VMInit: " + generate(50));
        drain();
    }

    static void agentThreadRows() throws Exception {
        System.out.println("enable ThreadStart/End: err=" + enable(52, true) + " " + enable(53, true));
        Thread t = new Thread("l1w63-agent");
        System.out.println("RunAgentThread: err=" + runAgent(t));
        t.join(5_000);
        Thread plain = new Thread(() -> { }, "l1w63-plain");
        plain.start();
        plain.join();
        enable(52, false);
        enable(53, false);
        System.out.print(drain());
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W63JvmtiEvents <path of the shim library>");
            return;
        }
        System.load(args[0]);
        fieldRows();
        monitorRows();
        gcRows();
        allocRows();
        sampledRows();
        bindRows();
        exhaustedRows();
        virtualRows();
        unloadRows();
        codeRows();
        agentThreadRows();
        System.out.println("done");
    }
}

/** The class `unloadRows` defines in a loader it then drops. */
class L1W63Unloaded {
    static int value = 1;
}

// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time but for the uptime in the `[...][error][jvmti]` line,
// written `<uptime>` here:
//
//   watch access counter: err=0
//   watch modification counter: err=0
//   watch modification name: err=0
//   watch access counter again: err=40
//   clear access untouched: err=41
//   enable FieldAccess: err=0
//   enable FieldModification: err=0
//   access counter in touch at 2 by main object=set
//   modify counter in touch at 7 by main object=set sig=I value=5
//   modify name in touch at 12 by main object=null sig=L value=object
//   clear access counter: err=0
//   clear modification counter: err=0
//   clear modification name: err=0
//   enable monitor events: err=0 0 0 0
//   monitor contended-enter by l1w63-contender
//   monitor contended-entered by l1w63-contender
//   monitor wait by main timeout=20
//   monitor waited by main timed_out=1
//   monitor wait by main timeout=0
//   monitor waited by main timed_out=0
//   enable gc: err=0 0
//   gc started=1 finished=1 balanced=1
//   enable ObjectFree: err=0
//   object free 4242: posted
//   enable VMObjectAlloc: err=0
//   vm-alloc LL1W63JvmtiEvents$Made; size=16
//   vm-alloc [I size=32
//   vm-alloc [LL1W63JvmtiEvents$Made; size=32
//   vm-alloc [[I size=24
//   vm-alloc [I size=24
//   vm-alloc [I size=24
//   vm-alloc LL1W63JvmtiEvents$Made; size=16
//   kept: 31
//   sampling 0: err=0
//   enable SampledObjectAlloc: err=0
//   sampled Sampled: posted
//   sampling 524288: err=0
//   enable NativeMethodBind: err=0
//   boundLater: 21
//   registerNow: err=0
//   registered: 55
//   replaced: 77
//   bind boundLater address-is-own=1
//   bind registerNow address-is-own=0
//   bind registered address-is-own=1
//   bind replaced address-is-own=1
//   enable ResourceExhausted: err=0
//   [<uptime>][error][jvmti] Posting Resource Exhausted event: Requested array size exceeds VM limit
//   array limit: Requested array size exceeds VM limit
//   exhausted flags=1 description=Requested array size exceeds VM limit
//   enable VirtualThreadStart/End: err=0 0
//   mount callbacks: err=0 0
//   enable mount events: err=0 0
//   vt start
//   vt mount
//   vt unmount
//   vt mount
//   vt unmount
//   vt end
//   unload callback: err=0 enable: err=0
//   defined: L1W63Unloaded
//   class unload: posted
//   enable CompiledMethodLoad: err=0
//   hot compiled: posted sum>0=true
//   generate CompiledMethodLoad: err=0 posted=1
//   generate DynamicCodeGenerated: err=0 posted=1
//   generate VMInit: err=103 posted=0
//   enable ThreadStart/End: err=0 0
//   RunAgentThread: err=0
//   thread start l1w63-agent
//   thread end l1w63-agent
//   thread start l1w63-plain
//   thread end l1w63-plain
//   done
