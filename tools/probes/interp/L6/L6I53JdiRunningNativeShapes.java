// Interpreter round i2, the i53 retirement: the frames of a thread suspended
// while it runs a LONG native outside a blocking region, in the shapes the
// wave-57..62 window still refused or waited on
// (docs/internal/fixed-bugs/interpreter-L6-a-thread-suspended-in-a-running-native-lists-no-frames-FIXED-20261007.md).
//
// The debuggee's `worker` compresses 16 MiB in ONE `Deflater.deflate` call
// (the `deflateBytesBytes` native, which blocks nowhere and runs for seconds
// at level 9), in a loop, reached by the shape `<mode>` names:
//
//   plain    the loop runs in a platform thread (the wave-56 control)
//   reflect  the loop is called through `Method.invoke`
//   virtual  the loop runs in a virtual thread
//   gc       as plain; after the suspension a second thread asks for a
//            collection (`System.gc()`) before the frames are read
//   jni      each call is made from Java a JNI native (`callBack`, in
//            libl6i53jni) calls back into (Linux: build the library from
//            L6I53JdiRunningNativeShapes.c)
//   jnispin  no Deflater: the worker runs a JNI native (`spin`) that spins
//            in C for ~3 s, calling nothing
//   raffifo  no Deflater: the worker waits in `RandomAccessFile.read()` on
//            the FIFO `-Dl6i53.fifo` names (`mkfifo`; Linux), a LEAF native
//            of this VM's that runs outside the native-call funnel; written
//            to at the end. Under a debugger only (a plain run waits).
//
// The debugger suspends the worker in the middle of a call and lists its
// frames: the native on top (location -1), then its callers, with the JDK
// frames between two probe frames counted, not named, down to the probe's
// `body`; a native probe frame is marked `(n)`. HotSpot lists such a thread
// at once, the native still running.
//
// Plain run (no debugger): stdout must equal HotSpot 25's, `ended`.
//
// Under a debugger:
//
//   javac -g -d out L6I53JdiRunningNativeShapes.java
//   cratonvm --java-home $JDK --jdwp-port 5794 -cp out L6I53JdiRunningNativeShapes <mode> wait
//   java -cp out L6I53JdiRunningNativeShapes <mode> debug 5794 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags,
// `java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5794`),
// measured on the Windows box (jni: on the Linux host, 25.0.4):
//
//   plain:   top: deflateBytesBytes native: true
//            frames: deflateBytesBytes < 2 JDK < body
//   reflect: top: deflateBytesBytes native: true
//            frames: deflateBytesBytes < 2 JDK < loop < 2 JDK < body
//   virtual: top: deflateBytesBytes native: true
//            frames: deflateBytesBytes < 2 JDK < body
//   gc:      top: deflateBytesBytes native: true
//            frames: deflateBytesBytes < 2 JDK < body
//   jni:     top: deflateBytesBytes native: true
//            frames: deflateBytesBytes < 2 JDK < once < callBack(n) < body
//
// (each mode prints `== <mode>`, `  prompt: true` -- the suspension and the
// listing took under 1.5 s, where a listing made at the native's return waits
// for the rest of a ~3 s call --, `  stable: true`, the two lines above, and
// `== end` / `  disconnected`.)
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[NATIVE_FRAMES_WINDOW]
// tid=<n> published=true ...` when the debugger read the worker's frames
// through its native frames window (`direct=true` for a read made while a
// collection waited for the native).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;
import java.util.zip.Deflater;

public class L6I53JdiRunningNativeShapes {
    /** Set by the debugger: 1 = start, 3 = done. Set to 2 by the debuggee once the worker is in a call. */
    static volatile int phase;
    static volatile long counter;
    static volatile boolean inCall;
    static volatile boolean stop;
    /** Set by the debugger (gc mode): ask for a collection now. */
    static volatile int gcNow;
    static volatile Thread worker;
    /**
     * raffifo: bytes fed before the worker's `read` waits, so its read site
     * has run that often (`-Dl6i53.feed`; the default is enough for the JIT to
     * compile the loop, whose leaf native call then runs outside the funnel).
     */
    static final int FIFO_FEED = Integer.getInteger("l6i53.feed", 300_000);
    static byte[] in;
    static byte[] out;
    static Deflater d;

    static native void callBack();

    /** Spin in C for about `millis` ms, calling nothing (jnispin). */
    static native void spin(int millis);

    static void prepare() {
        in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < (1 << 20); i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            // A small alphabet: compressible, so level 9 searches hard.
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        // Each copy lies beyond deflate's 32 KiB window: as slow as the first.
        for (int k = 1; k < 16; k++) {
            System.arraycopy(in, 0, in, k << 20, 1 << 20);
        }
        out = new byte[in.length + (1 << 20)];
        d = new Deflater(Deflater.BEST_COMPRESSION);
    }

    /** One compression, called back by `callBack` (jni). */
    static void once() {
        d.reset();
        d.setInput(in);
        d.finish();
        inCall = true;
        d.deflate(out);
        inCall = false;
        counter++;
    }

    public static void loop() {
        while (!stop) {
            once0();
        }
    }

    /** `once`, inlined by hand: the listing then names `body` (or `loop`) under the JDK frames. */
    static void once0() {
        d.reset();
        d.setInput(in);
        d.finish();
        inCall = true;
        d.deflate(out);
        inCall = false;
        counter++;
    }

    static void body(String mode) {
        switch (mode) {
            case "reflect":
                try {
                    L6I53JdiRunningNativeShapes.class.getMethod("loop").invoke(null);
                } catch (ReflectiveOperationException e) {
                    System.out.println("reflection failed: " + e);
                }
                return;
            case "jni":
                while (!stop) {
                    callBack();
                }
                return;
            case "jnispin":
                while (!stop) {
                    inCall = true;
                    spin(3000);
                    inCall = false;
                    counter++;
                }
                return;
            case "raffifo":
                try (java.io.RandomAccessFile raf =
                        new java.io.RandomAccessFile(System.getProperty("l6i53.fifo"), "r")) {
                    while (!stop) {
                        inCall = true;
                        int b = raf.read();
                        inCall = false;
                        counter++;
                        if (b < 0) {
                            return;
                        }
                    }
                } catch (java.io.IOException e) {
                    System.out.println("fifo failed: " + e);
                }
                return;
            default:
                while (!stop) {
                    d.reset();
                    d.setInput(in);
                    d.finish();
                    inCall = true;
                    d.deflate(out);
                    inCall = false;
                    counter++;
                }
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "plain";
        if (args.length == 3 && args[1].equals("debug")) {
            Debugger.run(mode, Integer.parseInt(args[2]));
            return;
        }
        if (mode.startsWith("jni")) {
            // By absolute path, from -Dl6i53.dir (default: the working
            // directory): `jdk-only` drops an explicit -Djava.library.path
            // (filed separately).
            System.load(new java.io.File(System.getProperty("l6i53.dir", "."),
                    System.mapLibraryName("l6i53jni")).getAbsolutePath());
        }
        boolean debugged = args.length == 2 && args[1].equals("wait");
        long until = System.currentTimeMillis() + 600_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        prepare();
        Runnable task = () -> body(mode);
        Thread t = mode.equals("virtual")
                ? Thread.ofVirtual().name("worker").unstarted(task)
                : new Thread(task, "worker");
        if (!mode.equals("virtual")) {
            t.setDaemon(true);
        }
        worker = t;
        t.start();
        // raffifo: the writer end, opened once the worker opens the reader
        // end. `FIFO_FEED` bytes first, so the worker's read site has run
        // (and its loop is compiled), then nothing until the end: the next
        // `read` waits.
        java.io.FileOutputStream fifoOut = mode.equals("raffifo")
                ? new java.io.FileOutputStream(System.getProperty("l6i53.fifo"))
                : null;
        if (fifoOut != null) {
            fifoOut.write(new byte[FIFO_FEED]);
            fifoOut.flush();
            while (counter < FIFO_FEED) {
                Thread.sleep(1);
            }
        }
        if (mode.equals("gc")) {
            Thread gcer = new Thread(() -> {
                while (gcNow == 0 && !stop) {
                    try {
                        Thread.sleep(5);
                    } catch (InterruptedException e) {
                        return;
                    }
                }
                if (!stop) {
                    System.gc();
                }
            }, "gcer");
            gcer.setDaemon(true);
            gcer.start();
        }
        while (!debugged && counter == 0) {
            Thread.sleep(1);
        }
        if (debugged) {
            while (!inCall) {
                Thread.sleep(1);
            }
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        stop = true;
        if (fifoOut != null) {
            fifoOut.write(1);
            fifoOut.close();
        }
        t.join(600_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long counter() {
            return ((LongValue) type.getValue(type.fieldByName("counter"))).value();
        }

        static String frames(List<StackFrame> frames) throws Exception {
            if (frames.isEmpty()) {
                return "none";
            }
            StringBuilder s = new StringBuilder(frames.get(0).location().method().name());
            int jdk = 0;
            for (int i = 1; i < frames.size(); i++) {
                Location at = frames.get(i).location();
                String owner = at.declaringType().name();
                // HotSpot also lists the method-handle frames under the
                // reflection accessor, which this VM does not run (its
                // `Method.invoke` is served by a native): left out, as
                // `L1W47JdiReflectiveFrames` leaves them out.
                if (owner.startsWith("java.lang.invoke.") || owner.contains("/")
                        || at.method().name().equals("invokeImpl")) {
                    continue;
                }
                if (owner.startsWith("java.") || owner.startsWith("jdk.")) {
                    jdk++;
                    continue;
                }
                if (jdk > 0) {
                    s.append(" < ").append(jdk).append(" JDK");
                    jdk = 0;
                }
                s.append(" < ").append(at.method().name());
                if (at.method().isNative()) {
                    s.append("(n)");
                }
                // Down to the probe's `body`: the lambda and `Thread` frames
                // below it are not this probe's question.
                if (at.method().name().equals("body")) {
                    break;
                }
            }
            return s.toString();
        }

        static void run(String mode, int port) throws Exception {
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
            System.out.println("== " + mode);
            type = (ClassType) awaitClass("L6I53JdiRunningNativeShapes");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 300_000;
            IntegerValue ready;
            do {
                if (System.currentTimeMillis() > until) {
                    fail("the worker never ran");
                }
                Thread.sleep(20);
                ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
            } while (ready.value() < 2);
            Value w = type.getValue(type.fieldByName("worker"));
            if (!(w instanceof ThreadReference)) {
                fail("no worker thread: " + w);
            }
            ThreadReference worker = (ThreadReference) w;
            // In the native by now: it entered the call before `phase` was 2.
            Thread.sleep(200);
            long start = System.nanoTime();
            worker.suspend();
            long slept = 0;
            if (mode.equals("gc")) {
                type.setValue(type.fieldByName("gcNow"), vm.mirrorOf(1));
                // Long enough for the collection to be requested and to wait
                // for the native (HotSpot's collects at once).
                Thread.sleep(500);
                slept = 500;
            }
            List<StackFrame> frames = worker.frames();
            // A listing made where the native returns names the native on top
            // too: only the wait tells it from one made while it runs (a call
            // runs ~3 s; HotSpot answers in milliseconds).
            long tookMs = (System.nanoTime() - start) / 1_000_000 - slept;
            System.out.println("  prompt: " + (tookMs < 1500));
            long first = counter();
            Thread.sleep(100);
            long second = counter();
            System.out.println("  stable: " + (first == second));
            if (frames.isEmpty()) {
                System.out.println("  top: none");
            } else {
                Method top = frames.get(0).location().method();
                System.out.println("  top: " + top.name() + " native: " + top.isNative());
            }
            System.out.println("  frames: " + frames(frames));
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            worker.resume();
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(300_000);
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
                    try {
                        next.resume();
                    } catch (VMDisconnectedException gone) {
                        break;
                    }
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
