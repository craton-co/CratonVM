// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L1 -- JDWP `StackFrame.ThisObject` of a
// NATIVE frame (stage 2 of
// docs/internal/fixed-bugs/interpreter-i15-L1-proposal-record-the-running-native-on-the-thread-DONE-20261224.md),
// asked over raw JDWP: JDI's `StackFrame.thisObject()` answers null for a
// native method without asking the VM, so only the wire shows it.
//
// Two threads are suspended:
//   waiter   in `LOCK.wait()`: its top frame is the instance native
//            `Object.wait0(J)V`, then `Object.wait(J)V`, `Object.wait()V`, `run`;
//   sleeper  in `Thread.sleep(..)`: its top frame is the static native
//            `Thread.sleepNanos0(J)V`.
// For the first frames of each the debugger prints the method, whether it is
// at location -1 (native), and what `ThisObject` answers: `lock` (the
// receiver `LOCK`), `null`, another object's tag, or the JDWP error.
//
// Plain run (no debugger): stdout as HotSpot 25: `ended`.
//
// Under a debugger (debug build, both modes, JIT and --nojit):
//   javac -g -d out L1W119RawJdwpNativeThisObject.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5795 -cp out L1W119RawJdwpNativeThisObject wait &
//   java -cp out L1W119RawJdwpNativeThisObject debug 5795
//   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5795 -cp out L1W119RawJdwpNativeThisObject wait)
//
// HotSpot 25.0.3 as the debuggee (run in wave 119, Windows; re-measured in
// wave 127 after the start-up flag fix, default flags x3 and -Xint, same):
//   == attach
//   == waiter
//     frame 0: wait0(J)V native=true this=null
//     frame 1: wait(J)V native=false this=lock
//   == sleeper
//     frame 0: sleepNanos0(J)V native=true this=null
//     frame 1: sleepNanos(J)V native=false this=null
//   == done
//
// So HotSpot answers a NULL `this` for a native frame, instance method or
// not (`wait0` is an instance native), with no error. CratonVM answered the
// same for the listing's head (no locals published), but `OPAQUE_FRAME` (32)
// for a native in the middle of a listing (a spliced JNI native, an opaque
// row); since wave 119 every native frame answers null
// (`commands::sf_this_object_tagged`). Both modes should print HotSpot's
// transcript: under a debugger `--compatible` runs `Object.wait`'s and
// `Thread.sleep`'s JDK bytecode (`debug::BLOCKING_STANDINS`). With
// `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE=0` the stand-in serves them
// and `frame 1: wait(J)V` answers `error 32`
// (docs/internal/fixed-bugs/interpreter-i119-L1-compatible-standin-java-levels-answer-no-this-CLOSED-20261224.md).
public class L1W119RawJdwpNativeThisObject {
    static final Object LOCK = new Object();
    static volatile int go;
    // One flag per thread (wave 127): a shared `ready++` from both threads is
    // a lost update (both read 0, both write 1), and the debugger then waited
    // for `ready == 2` forever ("the threads never started", 4 of 10 sessions).
    static volatile int waiterReady;
    static volatile int sleeperReady;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        Thread waiter = new Thread(() -> {
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
        }, "waiter");
        Thread sleeper = new Thread(() -> {
            sleeperReady = 1;
            while (go == 0 && System.currentTimeMillis() < until) {
                try {
                    Thread.sleep(60_000);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "sleeper");
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

    /** A minimal JDWP client (8-byte ids), as `L1W28RawJdwpStaleFrameId`'s. */
    static final class Debugger {
        static java.io.DataInputStream in;
        static java.io.DataOutputStream out;
        static int nextId = 1;
        static final java.util.Map<Long, java.util.Map<Long, String>> METHODS =
                new java.util.HashMap<>();

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
                p.string("LL1W119RawJdwpNativeThisObject;");
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
            java.util.Map<String, Long> threads = new java.util.HashMap<>();
            r = check(command(1, 4, new byte[0])); // AllThreads
            java.util.List<Long> all = new java.util.ArrayList<>();
            for (int n = r.readInt(); n > 0; n--) {
                all.add(r.readLong());
            }
            for (long t : all) {
                threads.put(readString(check(command(11, 1, new Payload().id(t).bytes()))), t);
            }
            for (String name : new String[] {"waiter", "sleeper"}) {
                long thread = threads.get(name);
                System.out.println("== " + name);
                java.util.List<long[]> frames = frames(thread);
                for (int i = 0; i < Math.min(2, frames.size()); i++) {
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
                        answer = id == 0 ? "null" : id == lock ? "lock" : "other " + tag;
                    }
                    System.out.println("  frame " + i + ": " + methodName(f[1], f[2])
                            + " native=" + (f[3] == -1) + " this=" + answer);
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
