// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L6 -- the two legs of the
// `L6W115MonitorExitOfACallersLock` `hot: timeout` (page
// i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock).
//
// `Exit.exit(Object)` is hand-written `aload_0; monitorexit; return`. HotSpot
// never compiles it (unbalanced monitors) and its interpreter throws
// IllegalMonitorStateException (null message) without touching a lock the
// CALLER holds. Each row below runs on a worker thread with its own lock and a
// 20 s watchdog; a hang prints `timeout` for that row only.
//
//   callee-warm  30000 calls on a lock nobody holds, so the callee gets hot.
//                A COMPILED callee answers with the monitor table's message,
//                not null: `nullMessage=false` names the callee leg.
//   callee-leg   a cold caller, called once, holds a fresh lock and calls the
//                warm callee. A compiled callee releases the caller's lock
//                (`heldAfter=false`), and the caller's own monitorexit then
//                throws inside javac's self-covering catch-any: `timeout`.
//                Fixed by CRATONVM_JIT_UNPAIRED_MONITOR_REFUSE (default on).
//   caller-leg   the caller hot (compiled), the callee interpreted: the hold
//                is the compiled frame's, no interpreter record names it, and
//                held_monitors::monitorexit_unrecorded's leniency releases it
//                -> `timeout` unless CRATONVM_MONITOREXIT_FRAME_STRICT=1.
//
// Run, each mode, JIT and --nojit:
//   cratonvm L6W116UnpairedMonitorExitLegs
//   CRATONVM_MONITOREXIT_FRAME_STRICT=1 cratonvm L6W116UnpairedMonitorExitLegs   (expected SAME with the JIT)
//   CRATONVM_JIT_UNPAIRED_MONITOR_REFUSE=0 CRATONVM_MONITOREXIT_FRAME_STRICT=1 cratonvm L6W116UnpairedMonitorExitLegs
//                                                                  (the pre-fix callee leg: callee-warm/callee-leg differ)
//
// HOTSPOT_EXPECTED_BEGIN
// callee-warm: imse=30000 nullMessage=true
// callee-leg: imse=1 nullMessage=true heldAfter=true
// caller-leg: imse=30000 nullMessage=true heldAfter=30000
// HOTSPOT_EXPECTED_END

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L6W116UnpairedMonitorExitLegs {
    /** `public class Exit { public static void exit(Object o) { monitorexit o; } }`, version 49. */
    static byte[] exitClass() {
        java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
        try {
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(49);
            out.writeShort(8);
            out.writeByte(1);
            out.writeUTF("Exit");
            out.writeByte(7);
            out.writeShort(1);
            out.writeByte(1);
            out.writeUTF("java/lang/Object");
            out.writeByte(7);
            out.writeShort(3);
            out.writeByte(1);
            out.writeUTF("exit");
            out.writeByte(1);
            out.writeUTF("(Ljava/lang/Object;)V");
            out.writeByte(1);
            out.writeUTF("Code");
            out.writeShort(0x0021);
            out.writeShort(2);
            out.writeShort(4);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(1);
            out.writeShort(0x0009);
            out.writeShort(5);
            out.writeShort(6);
            out.writeShort(1);
            out.writeShort(7);
            byte[] code = {0x2a, (byte) 0xc3, (byte) 0xb1};
            out.writeInt(2 + 2 + 4 + code.length + 2 + 2);
            out.writeShort(1);
            out.writeShort(1);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(0);
            out.flush();
        } catch (java.io.IOException e) {
            throw new AssertionError(e);
        }
        return bytes.toByteArray();
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L6W116UnpairedMonitorExitLegs.class.getClassLoader());
        }

        Class<?> define(byte[] b) {
            return defineClass("Exit", b, 0, b.length);
        }
    }

    static Method exit;

    /** 0 returned normally, 1 IMSE with a null message, 2 IMSE with a message, 3 anything else. */
    static int callExit(Object o) {
        try {
            exit.invoke(null, o);
            return 0;
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            if (c instanceof IllegalMonitorStateException) {
                return c.getMessage() == null ? 1 : 2;
            }
            return 3;
        } catch (ReflectiveOperationException e) {
            return 3;
        }
    }

    /** The outcome of `callExit` under `synchronized (o)`, plus 10 when the lock is still held after. */
    static int callUnderHot(Object o) {
        int r;
        synchronized (o) {
            r = callExit(o);
            if (Thread.holdsLock(o)) {
                r += 10;
            }
        }
        return r;
    }

    /** As `callUnderHot`, but a separate method called once (stays interpreted). */
    static int callUnderOnce(Object o) {
        int r;
        synchronized (o) {
            r = callExit(o);
            if (Thread.holdsLock(o)) {
                r += 10;
            }
        }
        return r;
    }

    interface Row {
        String run() throws Exception;
    }

    static String withWatchdog(Row row) throws InterruptedException {
        String[] result = {"timeout"};
        Thread t = new Thread(() -> {
            try {
                result[0] = row.run();
            } catch (Throwable e) {
                result[0] = "threw " + e.getClass().getName();
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(20_000);
        return t.isAlive() ? "timeout" : result[0];
    }

    public static void main(String[] args) throws Exception {
        exit = new Loader().define(exitClass()).getMethod("exit", Object.class);

        System.out.println("callee-warm: " + withWatchdog(() -> {
            Object free = new Object();
            int imse = 0;
            boolean nullMessage = true;
            for (int i = 0; i < 30_000; i++) {
                int r = callExit(free);
                if (r == 1 || r == 2) {
                    imse++;
                }
                if (r != 1) {
                    nullMessage = false;
                }
            }
            return "imse=" + imse + " nullMessage=" + nullMessage;
        }));

        System.out.println("callee-leg: " + withWatchdog(() -> {
            int r = callUnderOnce(new Object());
            int outcome = r % 10;
            return "imse=" + (outcome == 1 || outcome == 2 ? 1 : 0) + " nullMessage=" + (outcome == 1)
                    + " heldAfter=" + (r >= 10);
        }));

        System.out.println("caller-leg: " + withWatchdog(() -> {
            Object lock = new Object();
            int imse = 0;
            int held = 0;
            boolean nullMessage = true;
            for (int i = 0; i < 30_000; i++) {
                int r = callUnderHot(lock);
                int outcome = r % 10;
                if (outcome == 1 || outcome == 2) {
                    imse++;
                }
                if (outcome != 1) {
                    nullMessage = false;
                }
                if (r >= 10) {
                    held++;
                }
            }
            return "imse=" + imse + " nullMessage=" + nullMessage + " heldAfter=" + held;
        }));
    }
}
