// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 129, lane L1 -- the VIRTUAL-thread variant of
// L1W119RawJdwpNativeThisObject: JDWP `StackFrame.ThisObject` of the frames a
// debugger lists for a virtual thread blocked in `Object.wait` and in
// `Thread.sleep`, asked over raw JDWP (JDI's `thisObject()` answers null for
// a native frame without asking the VM, so only the wire shows it)
// (docs/internal/fixed-bugs/interpreter-i119-L1-compatible-standin-java-levels-answer-no-this-CLOSED-20261224.md).
//
// Two VIRTUAL threads are suspended:
//   vwaiter   in `LOCK.wait()` inside `synchronized (LOCK)`;
//   vsleeper  in `Thread.sleep(..)`.
// For each the debugger prints its frames, top first, down to and including
// the probe's own lambda: the method, whether it is at location -1
// (native), and what `ThisObject` answers: `lock` (the receiver `LOCK`),
// `self` (the virtual thread's own `Thread` object), `null`, another
// object's tag, or the JDWP error.
//
// The debugger finds the virtual threads through the static fields that
// hold them (`ReferenceType.GetValues` answers a thread id), not through
// `VirtualMachine.AllThreads`: that lists virtual threads only under the
// agent's `includevirtualthreads=y` (HotSpot), or
// `CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS=1` (CratonVM; its `-agentlib:jdwp`
// parser accepts `includevirtualthreads` and drops it). So neither VM needs
// the option, and the transcript does not depend on it (HotSpot measured
// both with and without it: the same).
//
// Plain run (no debugger): stdout as HotSpot 25: `ended`.
//
// Under a debugger (debug build, both modes, JIT and --nojit):
//   javac -g -d out L1W129RawJdwpVirtualWaitThisObject.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5796 -cp out L1W129RawJdwpVirtualWaitThisObject wait &
//   java -cp out L1W129RawJdwpVirtualWaitThisObject debug 5796
//   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5796 -cp out L1W129RawJdwpVirtualWaitThisObject wait)
//
// HotSpot 25.0.3 as the debuggee (Windows, 2026-10-19; default flags x3,
// -Xint x1, and default flags with `,includevirtualthreads=y` x1: all five
// identical):
//   == attach
//   == vwaiter virtual=true
//     frame 0: Object.wait0(J)V native=true this=null
//     frame 1: Object.wait(J)V native=false this=lock
//     frame 2: Object.wait()V native=false this=lock
//     frame 3: L1W129RawJdwpVirtualWaitThisObject.lambda$main$0(J)V native=false this=null
//   == vsleeper virtual=true
//     frame 0: VirtualThread.yieldContinuation()Z native=false this=self
//     frame 1: VirtualThread.parkNanos(J)V native=false this=self
//     frame 2: VirtualThread.sleepNanos(J)V native=false this=self
//     frame 3: Thread.sleepNanos(J)V native=false this=null
//     frame 4: Thread.sleep(J)V native=false this=null
//     frame 5: L1W129RawJdwpVirtualWaitThisObject.lambda$main$1(J)V native=false this=null
//   == done
//
// Pin or unmount: on HotSpot 25.0.3 a virtual thread in `LOCK.wait()` inside
// `synchronized` UNMOUNTS (JEP 491): with one carrier
// (`-Djdk.virtualThreadScheduler.parallelism=1 -Djdk.virtualThreadScheduler.maxPoolSize=1`)
// a second virtual thread still runs while it waits (default flags, -Xint,
// and with the JDWP agent loaded). Its listing still heads with the native
// `wait0`, where the continuation was frozen.
//
// The `VirtualThread` rows answer the thread itself (`self`): they are
// instance methods of the virtual thread's own object. CratonVM's default
// virtual threads (`CRATONVM_VT_CONTINUATIONS` on) list the real deposited
// frames of the JDK continuation, whose slot 0 is that object; its bound
// shape (`=0`) lists those rows as opaque, which answered `error 32` until
// wave 129 (now the thread, under `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS=1`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, plain run, no arguments; default flags and -Xint identical)
// ended
// HOTSPOT_EXPECTED_END
public class L1W129RawJdwpVirtualWaitThisObject {
    static final Object LOCK = new Object();
    static volatile int go;
    // One flag per thread (as L1W119 since wave 127): no shared `ready++`.
    static volatile int waiterReady;
    static volatile int sleeperReady;
    static volatile Thread vwaiter;
    static volatile Thread vsleeper;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        Thread waiter = Thread.ofVirtual().name("vwaiter").unstarted(() -> {
            synchronized (LOCK) {
                waiterReady = 1;
                while (go == 0 && System.currentTimeMillis() < until) {
                    try {
                        LOCK.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        });
        Thread sleeper = Thread.ofVirtual().name("vsleeper").unstarted(() -> {
            sleeperReady = 1;
            while (go == 0 && System.currentTimeMillis() < until) {
                try {
                    Thread.sleep(60_000);
                } catch (InterruptedException e) {
                    return;
                }
            }
        });
        vwaiter = waiter;
        vsleeper = sleeper;
        if (wait) {
            waiter.start();
            sleeper.start();
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            synchronized (LOCK) {
                LOCK.notifyAll();
            }
            sleeper.interrupt();
            waiter.join();
            sleeper.join();
        }
        System.out.println("ended");
    }

    /** A minimal JDWP client (8-byte ids), as L1W119RawJdwpNativeThisObject's. */
    static final class Debugger {
        static java.io.DataInputStream in;
        static java.io.DataOutputStream out;
        static int nextId = 1;
        static final java.util.Map<Long, java.util.Map<Long, String>> METHODS =
                new java.util.HashMap<>();
        static final java.util.Map<Long, String> CLASSES = new java.util.HashMap<>();

        static void run(int port) throws Exception {
            long deadline = System.currentTimeMillis() + 300_000;
            java.net.Socket socket = null;
            while (socket == null) {
                try {
                    socket = new java.net.Socket("127.0.0.1", port);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > deadline) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
            socket.setSoTimeout(120_000);
            in = new java.io.DataInputStream(new java.io.BufferedInputStream(socket.getInputStream()));
            out = new java.io.DataOutputStream(socket.getOutputStream());
            byte[] hello = "JDWP-Handshake".getBytes(java.nio.charset.StandardCharsets.US_ASCII);
            out.write(hello);
            out.flush();
            byte[] back = new byte[hello.length];
            in.readFully(back);
            if (!java.util.Arrays.equals(hello, back)) {
                throw new IllegalStateException("no JDWP handshake");
            }
            System.out.println("== attach");
            java.io.DataInputStream r = check(command(1, 7, new byte[0])); // IDSizes
            for (int i = 0; i < 5; i++) {
                if (r.readInt() != 8) {
                    throw new IllegalStateException("ids are not 8 bytes");
                }
            }
            long type = 0;
            while (type == 0) {
                Payload p = new Payload();
                p.string("LL1W129RawJdwpVirtualWaitThisObject;");
                r = check(command(1, 2, p.bytes())); // ClassesBySignature
                if (r.readInt() > 0) {
                    r.readByte();
                    type = r.readLong();
                } else if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("the probe class never loaded");
                } else {
                    Thread.sleep(50);
                }
            }
            java.util.Map<String, Long> fields = new java.util.HashMap<>();
            r = check(command(2, 4, new Payload().id(type).bytes())); // Fields
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                fields.put(name, id);
            }
            // Both threads waiting or sleeping, then the VM suspended.
            while (staticInt(type, fields.get("waiterReady")) == 0
                    || staticInt(type, fields.get("sleeperReady")) == 0) {
                if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("the threads never started");
                }
                Thread.sleep(50);
            }
            Thread.sleep(500);
            check(command(1, 8, new byte[0])); // VirtualMachine.Suspend
            long lock = staticObject(type, fields.get("LOCK"));
            for (String name : new String[] {"vwaiter", "vsleeper"}) {
                long thread = staticObject(type, fields.get(name));
                r = check(command(11, 15, new Payload().id(thread).bytes())); // IsVirtual
                boolean virtual = r.readBoolean();
                System.out.println("== " + name + " virtual=" + virtual);
                java.util.List<long[]> frames = frames(thread);
                for (int i = 0; i < Math.min(8, frames.size()); i++) {
                    long[] f = frames.get(i);
                    Payload p = new Payload().id(thread).id(f[0]);
                    Reply reply = command(16, 3, p.bytes()); // StackFrame.ThisObject
                    String answer;
                    if (reply.error != 0) {
                        answer = "error " + reply.error;
                    } else {
                        java.io.DataInputStream d = reply.data();
                        char tag = (char) d.readByte();
                        long id = d.readLong();
                        answer = id == 0 ? "null"
                                : id == lock ? "lock"
                                : id == thread ? "self"
                                : "other " + tag;
                    }
                    String cls = className(f[1]);
                    String method = methodName(f[1], f[2]);
                    System.out.println("  frame " + i + ": " + cls + "." + method
                            + " native=" + (f[3] == -1) + " this=" + answer);
                    if (cls.equals("L1W129RawJdwpVirtualWaitThisObject")) {
                        break;
                    }
                }
            }
            setStatic(type, fields.get("go"), 1);
            check(command(1, 9, new byte[0])); // VirtualMachine.Resume
            try {
                command(1, 6, new byte[0]); // Dispose
            } catch (java.io.IOException exited) {
                // The debuggee may exit first.
            }
            System.out.println("== done");
            socket.close();
        }

        /** `ThreadReference.Frames`: {frame id, class id, method id, index} per frame, top first. */
        static java.util.List<long[]> frames(long thread) throws Exception {
            Payload p = new Payload().id(thread);
            p.i32(0);
            p.i32(-1);
            java.io.DataInputStream r = check(command(11, 6, p.bytes()));
            java.util.List<long[]> list = new java.util.ArrayList<>();
            for (int n = r.readInt(); n > 0; n--) {
                long frame = r.readLong();
                r.readByte();
                long cls = r.readLong();
                long method = r.readLong();
                long index = r.readLong();
                list.add(new long[] {frame, cls, method, index});
            }
            return list;
        }

        /** The class's simple binary name (`ReferenceType.Signature`), package dropped. */
        static String className(long cls) throws Exception {
            String name = CLASSES.get(cls);
            if (name == null) {
                String sig = readString(check(command(2, 1, new Payload().id(cls).bytes())));
                // `Lpkg/Name;` -> `Name`; `LName;` -> `Name`.
                name = sig.substring(Math.max(sig.lastIndexOf('/'), 0) + 1, sig.length() - 1);
                CLASSES.put(cls, name);
            }
            return name;
        }

        static String methodName(long cls, long method) throws Exception {
            java.util.Map<Long, String> names = METHODS.get(cls);
            if (names == null) {
                names = new java.util.HashMap<>();
                java.io.DataInputStream r = check(command(2, 5, new Payload().id(cls).bytes()));
                for (int n = r.readInt(); n > 0; n--) {
                    long id = r.readLong();
                    String name = readString(r);
                    String sig = readString(r);
                    r.readInt();
                    names.put(id, name + sig);
                }
                METHODS.put(cls, names);
            }
            return names.getOrDefault(method, "?");
        }

        static int staticInt(long type, long field) throws Exception {
            Payload p = new Payload().id(type);
            p.i32(1);
            p.id(field);
            java.io.DataInputStream r = check(command(2, 6, p.bytes())); // ReferenceType.GetValues
            r.readInt();
            r.readByte();
            return r.readInt();
        }

        static long staticObject(long type, long field) throws Exception {
            Payload p = new Payload().id(type);
            p.i32(1);
            p.id(field);
            java.io.DataInputStream r = check(command(2, 6, p.bytes()));
            r.readInt();
            r.readByte();
            return r.readLong();
        }

        static void setStatic(long type, long field, int value) throws Exception {
            Payload p = new Payload().id(type);
            p.i32(1);
            p.id(field);
            p.i32(value);
            check(command(3, 2, p.bytes())); // ClassType.SetValues
        }

        static final class Reply {
            int error;
            byte[] body;

            java.io.DataInputStream data() {
                return new java.io.DataInputStream(new java.io.ByteArrayInputStream(body));
            }
        }

        static java.io.DataInputStream check(Reply reply) {
            if (reply.error != 0) {
                throw new IllegalStateException("JDWP error " + reply.error);
            }
            return reply.data();
        }

        /** Send a command and read its reply, skipping the VM's own packets. */
        static Reply command(int set, int cmd, byte[] data) throws Exception {
            int id = nextId++;
            out.writeInt(11 + data.length);
            out.writeInt(id);
            out.writeByte(0);
            out.writeByte(set);
            out.writeByte(cmd);
            out.write(data);
            out.flush();
            while (true) {
                int length = in.readInt();
                int got = in.readInt();
                int flags = in.readUnsignedByte();
                if ((flags & 0x80) == 0) {
                    in.readUnsignedShort();
                    in.readFully(new byte[length - 11]);
                    continue;
                }
                Reply reply = new Reply();
                reply.error = in.readUnsignedShort();
                reply.body = new byte[length - 11];
                in.readFully(reply.body);
                if (got == id) {
                    return reply;
                }
            }
        }

        static String readString(java.io.DataInputStream r) throws java.io.IOException {
            byte[] b = new byte[r.readInt()];
            r.readFully(b);
            return new String(b, java.nio.charset.StandardCharsets.UTF_8);
        }

        static final class Payload {
            final java.io.ByteArrayOutputStream buf = new java.io.ByteArrayOutputStream();
            final java.io.DataOutputStream d = new java.io.DataOutputStream(buf);

            Payload id(long v) throws java.io.IOException {
                d.writeLong(v);
                return this;
            }

            void i32(int v) throws java.io.IOException {
                d.writeInt(v);
            }

            void string(String s) throws java.io.IOException {
                byte[] b = s.getBytes(java.nio.charset.StandardCharsets.UTF_8);
                d.writeInt(b.length);
                d.write(b);
            }

            byte[] bytes() {
                return buf.toByteArray();
            }
        }
    }
}
