// Interpreter round i2 wave 59, lane L6: the state and the top of the stack
// of a thread blocked in a native method, against HotSpot, for every reader
// (docs/internal/fixed-bugs/interpreter-L6-a-thread-blocked-in-a-native-lists-no-native-and-reads-waiting-FIXED-20261023.md).
//
// HotSpot runs a JDK `native` method in `_thread_in_native`: whatever the
// native waits for (a socket `accept` or `read`, a pipe read, a process
// wait), `Thread.getState()` and `ThreadInfo.getThreadState()` say
// RUNNABLE, `ThreadInfo.isInNative()` says true, the stack trace has the
// native on top, and JDWP `ThreadReference.status()` says RUNNING. The
// Java-level waits (`sleep`, `park`, `Object.wait`, `join`) keep their own
// states. Each row starts a thread that blocks, waits until its stack shows
// the blocking call (or two seconds) and 300 ms more, prints what the
// readers say, and releases it. Two rounds (the first-call rows again).
//
// Plain run: stdout must equal HotSpot 25's. MEASURED, HotSpot 25.0.3,
// default flags, the Windows box:
//
//   round 1
//   accept: RUNNABLE mx=RUNNABLE inNative=true top=sun.nio.ch.Net.accept native=true < sun.nio.ch.NioSocketImpl.accept
//   read: RUNNABLE mx=RUNNABLE inNative=true top=sun.nio.ch.SocketDispatcher.read0 native=true < sun.nio.ch.SocketDispatcher.read
//   pipe: RUNNABLE mx=RUNNABLE inNative=true top=java.io.FileInputStream.readBytes native=true < java.io.FileInputStream.read
//   waitFor: RUNNABLE mx=RUNNABLE inNative=true top=java.lang.ProcessImpl.waitForInterruptibly native=true < java.lang.ProcessImpl.waitFor
//   sleep: TIMED_WAITING mx=TIMED_WAITING inNative=false top=java.lang.Thread.sleepNanos0 native=true < java.lang.Thread.sleepNanos
//   park: WAITING mx=WAITING inNative=false top=jdk.internal.misc.Unsafe.park native=true < java.util.concurrent.locks.LockSupport.park
//   wait: WAITING mx=WAITING inNative=false top=java.lang.Object.wait0 native=true < java.lang.Object.wait
//   join: WAITING mx=WAITING inNative=false top=java.lang.Object.wait0 native=true < java.lang.Object.wait
//   round 2
//   (the same eight lines)
//   done
//
// The `waitFor` row is platform-specific: on Linux `ProcessImpl.waitFor` is
// a Java `Object.wait` loop (the reaper thread runs the native), so HotSpot
// prints `waitFor: WAITING mx=WAITING inNative=false top=java.lang.Object.wait0
// native=true < java.lang.Object.wait` there (NOT measured on this box: take
// the host's HotSpot run as the reference).
//
// CratonVM before wave 59 (and with CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE off;
// debug build, both modes): the four native rows read
// `WAITING mx=WAITING inNative=false`, and their top was the native's Java
// caller (`sun.nio.ch.NioSocketImpl.accept native=false < ...`). With the
// switch: the HotSpot lines above under `--jdk-only`, default and --nojit.
// Under `--compatible` the `pipe` and `waitFor` rows read RUNNABLE but list
// the probe's lambda on top: `Process` there is the VM's synthetic class,
// which pushes no JDK frame
// (docs/internal/fixed-bugs/interpreter-i59-L6-compatible-synthetic-process-lists-no-jdk-frames-FIXED-20261115.md).
//
// Under a debugger (JDWP `ThreadReference.status()` and the top frame of each
// blocked thread, the threads all blocked at once):
//
//   javac -g -d out L6W59BlockedNativeStates.java
//   cratonvm --java-home $JDK --jdwp-port 5794 -cp out L6W59BlockedNativeStates wait
//   java -cp out L6W59BlockedNativeStates debug 5794 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box,
// `java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5794`):
//
//   == attach
//     accept: RUNNING top=accept native=true
//     read: RUNNING top=read0 native=true
//     pipe: RUNNING top=readBytes native=true
//     waitFor: RUNNING top=waitForInterruptibly native=true
//     sleep: SLEEPING top=sleepNanos0 native=true
//     park: WAIT top=park native=true
//     wait: WAIT top=wait0 native=true
//     join: WAIT top=wait0 native=true
//   == end
//     disconnected
//
// CratonVM with the switch, `--jdk-only` (debug build): the HotSpot transcript
// above; switch off: the four native rows say `WAIT`. `--compatible`: the
// `pipe` / `waitFor` tops are the probe's lambdas (see above).
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[BLOCKED_NATIVE_ROW]
// tid=<n> native=<name>` when a reader lists the native a thread is blocked
// in.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.io.File;
import java.io.InputStream;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L6W59BlockedNativeStates {
    /** Set by the debugger: 1 = start, 3 = done. Set to 2 by the debuggee once every row is blocked. */
    static volatile int phase;

    interface Blocker {
        void run() throws Exception;
    }

    static final String[] LABELS = {"accept", "read", "pipe", "waitFor", "sleep", "park", "wait", "join"};

    /** One blocked row: its thread, the method its stack shows once inside, and its release. */
    static final class Row {
        final String label;
        final String method;
        final Thread thread;
        final AutoCloseable release;

        Row(String label, String method, Blocker blocker, AutoCloseable release) {
            this.label = label;
            this.method = method;
            this.release = release;
            this.thread = new Thread(() -> {
                try {
                    blocker.run();
                } catch (Exception e) {
                    // released by the close
                }
            }, "blk-" + label);
            this.thread.setDaemon(true);
        }

        boolean inside() {
            for (StackTraceElement e : thread.getStackTrace()) {
                if (e.getMethodName().equals(method)) {
                    return true;
                }
            }
            return false;
        }

        void awaitInside() throws Exception {
            long deadline = System.nanoTime() + 2_000_000_000L;
            while (System.nanoTime() < deadline && !inside()) {
                Thread.sleep(1);
            }
            Thread.sleep(300);
        }
    }

    static String top(StackTraceElement[] trace) {
        if (trace.length < 2) {
            return "(short: " + trace.length + ")";
        }
        return trace[0].getClassName() + "." + trace[0].getMethodName()
                + " native=" + trace[0].isNativeMethod()
                + " < " + trace[1].getClassName() + "." + trace[1].getMethodName();
    }

    static String javaExe() {
        String home = System.getProperty("java.home");
        File exe = new File(home, "bin/java.exe");
        if (!exe.exists()) {
            exe = new File(home, "bin/java");
        }
        return exe.getPath();
    }

    /** A child that reads its stdin to the end and exits, printing nothing. */
    static Process child() throws Exception {
        String cp = System.getProperty("java.class.path");
        return new ProcessBuilder(javaExe(), "-cp", cp, "L6W59BlockedNativeStates", "child")
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start();
    }

    /** The eight rows, unstarted, in `LABELS` order. */
    static List<Row> rows() throws Exception {
        List<Row> rows = new ArrayList<>();
        ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        rows.add(new Row("accept", "accept", () -> server.accept().close(), server));
        ServerSocket listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        Socket client = new Socket(InetAddress.getLoopbackAddress(), listener.getLocalPort());
        Socket peer = listener.accept();
        InputStream in = client.getInputStream();
        rows.add(new Row("read", "read", () -> in.read(), () -> {
            client.close();
            peer.close();
            listener.close();
        }));
        Process pipeChild = child();
        rows.add(new Row("pipe", "read", () -> pipeChild.getInputStream().read(), () -> {
            pipeChild.getOutputStream().close();
            pipeChild.waitFor();
        }));
        Process waitChild = child();
        rows.add(new Row("waitFor", "waitFor", () -> waitChild.waitFor(), () -> waitChild.getOutputStream().close()));
        Thread[] sleeper = new Thread[1];
        Row sleep = new Row("sleep", "sleep", () -> Thread.sleep(600_000), () -> sleeper[0].interrupt());
        sleeper[0] = sleep.thread;
        rows.add(sleep);
        Thread[] parker = new Thread[1];
        java.util.concurrent.atomic.AtomicBoolean unparked = new java.util.concurrent.atomic.AtomicBoolean();
        Row park = new Row("park", "park", () -> {
            while (!unparked.get()) {
                LockSupport.park();
            }
        }, () -> {
            unparked.set(true);
            LockSupport.unpark(parker[0]);
        });
        parker[0] = park.thread;
        rows.add(park);
        Object lock = new Object();
        boolean[] notified = new boolean[1];
        rows.add(new Row("wait", "wait", () -> {
            synchronized (lock) {
                while (!notified[0]) {
                    lock.wait();
                }
            }
        }, () -> {
            synchronized (lock) {
                notified[0] = true;
                lock.notifyAll();
            }
        }));
        Object gate = new Object();
        boolean[] opened = new boolean[1];
        Thread joined = new Thread(() -> {
            synchronized (gate) {
                while (!opened[0]) {
                    try {
                        gate.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "blk-joined");
        joined.setDaemon(true);
        joined.start();
        rows.add(new Row("join", "join", () -> joined.join(), () -> {
            synchronized (gate) {
                opened[0] = true;
                gate.notifyAll();
            }
        }));
        return rows;
    }

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        for (Row row : rows()) {
            row.thread.start();
            row.awaitInside();
            ThreadInfo info = ManagementFactory.getThreadMXBean().getThreadInfo(row.thread.threadId());
            System.out.println(row.label + ": " + row.thread.getState()
                    + " mx=" + info.getThreadState()
                    + " inNative=" + info.isInNative()
                    + " top=" + top(row.thread.getStackTrace()));
            row.release.close();
            row.thread.join(10_000);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            while (System.in.read() >= 0) {
                // to the end of stdin
            }
            return;
        }
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 1 && args[0].equals("wait")) {
            debuggee();
            return;
        }
        round(1);
        round(2);
        System.out.println("done");
    }

    static void debuggee() throws Exception {
        long until = System.currentTimeMillis() + 600_000;
        while (phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        List<Row> rows = rows();
        for (Row row : rows) {
            row.thread.start();
        }
        for (Row row : rows) {
            row.awaitInside();
        }
        phase = 2;
        while (phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        for (Row row : rows) {
            row.release.close();
            row.thread.join(10_000);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;

        static String status(int s) {
            switch (s) {
                case ThreadReference.THREAD_STATUS_RUNNING: return "RUNNING";
                case ThreadReference.THREAD_STATUS_SLEEPING: return "SLEEPING";
                case ThreadReference.THREAD_STATUS_WAIT: return "WAIT";
                case ThreadReference.THREAD_STATUS_MONITOR: return "MONITOR";
                case ThreadReference.THREAD_STATUS_ZOMBIE: return "ZOMBIE";
                case ThreadReference.THREAD_STATUS_NOT_STARTED: return "NOT_STARTED";
                default: return "UNKNOWN(" + s + ")";
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
            ClassType type = (ClassType) awaitClass("L6W59BlockedNativeStates");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 300_000;
            IntegerValue ready;
            do {
                if (System.currentTimeMillis() > until) {
                    fail("the rows never blocked");
                }
                Thread.sleep(20);
                ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
            } while (ready.value() < 2);
            Map<String, ThreadReference> byName = new HashMap<>();
            for (ThreadReference t : vm.allThreads()) {
                byName.put(t.name(), t);
            }
            for (String label : LABELS) {
                ThreadReference t = byName.get("blk-" + label);
                if (t == null) {
                    System.out.println("  " + label + ": missing");
                    continue;
                }
                String state = status(t.status());
                t.suspend();
                String top;
                try {
                    List<StackFrame> frames = t.frames();
                    if (frames.isEmpty()) {
                        top = "none";
                    } else {
                        Method m = frames.get(0).location().method();
                        top = m.name() + " native=" + m.isNative();
                    }
                } finally {
                    t.resume();
                }
                System.out.println("  " + label + ": " + state + " top=" + top);
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
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
