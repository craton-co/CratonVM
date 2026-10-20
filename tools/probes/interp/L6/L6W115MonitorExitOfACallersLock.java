// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L6 -- stage 3 of proposal
// i23-L7-proposal-per-frame-locked-monitors (CRATONVM_MONITOREXIT_FRAME_STRICT,
// CRATONVM_DBG_MONITOR_LENIENCY).
//
// A hand-written static method `Exit.exit(Object)` is `aload_0; monitorexit;
// return`: it exits a monitor its own frame never entered. HotSpot's
// interpreter looks only in the executing frame's monitor block, so the call
// throws IllegalMonitorStateException (null message) and the CALLER still
// holds the lock -- whether the caller is interpreted or compiled. The probe
// calls it from inside `synchronized (o)` in a caller that is cold (row
// `cold`) and in one called hot enough to be compiled (row `hot`), and on a
// thread that holds the lock through a JNI-free path only (Java synchronized).
//
// CratonVM: with the caller interpreted the record of the caller's frame
// accounts for the hold, so `cold` matches in every arm. With the caller
// compiled, no interpreter record names the hold and the leniency of
// held_monitors::monitorexit_unrecorded RELEASES it unless
// CRATONVM_MONITOREXIT_FRAME_STRICT=1: then `hot` loses its IMSE count and
// the caller's own exit faults. Each row runs on a worker with a 20 s
// watchdog, so a hang prints `timeout` instead of hanging the battery.
//
// Wave 116 (lane L6): `hot: timeout` with the JIT had two legs, a COMPILED
// `Exit.exit` releasing the caller's lock (fixed: such a body is no longer
// compiled, CRATONVM_JIT_UNPAIRED_MONITOR_REFUSE) and the interpreter's
// leniency releasing a compiled caller's lock (CRATONVM_MONITOREXIT_FRAME_STRICT).
// `L6W116UnpairedMonitorExitLegs` runs each leg as its own row.
//
// Run, each mode, JIT and --nojit:
//   cratonvm L6W115MonitorExitOfACallersLock
//   CRATONVM_DBG_MONITOR_LENIENCY=1 cratonvm L6W115MonitorExitOfACallersLock   (stderr names each lenient release)
//   CRATONVM_MONITOREXIT_FRAME_STRICT=1 cratonvm L6W115MonitorExitOfACallersLock
//
// HOTSPOT_EXPECTED_BEGIN
// cold: imse=1 nullMessage=true heldAfter=true
// hot: imse=30000 nullMessage=true heldAfter=30000 other=0
// lock: free=true
// HOTSPOT_EXPECTED_END

import java.lang.reflect.Method;

public class L6W115MonitorExitOfACallersLock {
    /** The class file of `class Exit { static void exit(Object o) { monitorexit o; } }`, version 49. */
    static byte[] exitClass() {
        java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
        try {
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(49);
            // Constant pool: 1 Utf8 Exit, 2 Class #1, 3 Utf8 java/lang/Object,
            // 4 Class #3, 5 Utf8 exit, 6 Utf8 (Ljava/lang/Object;)V, 7 Utf8 Code.
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
            out.writeShort(0x0021); // public super
            out.writeShort(2);
            out.writeShort(4);
            out.writeShort(0); // interfaces
            out.writeShort(0); // fields
            out.writeShort(1); // methods
            out.writeShort(0x0009); // public static
            out.writeShort(5);
            out.writeShort(6);
            out.writeShort(1); // attributes
            out.writeShort(7);
            byte[] code = {0x2a, (byte) 0xc3, (byte) 0xb1};
            out.writeInt(2 + 2 + 4 + code.length + 2 + 2);
            out.writeShort(1); // max_stack
            out.writeShort(1); // max_locals
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(0); // exception table
            out.writeShort(0); // code attributes
            out.writeShort(0); // class attributes
            out.flush();
        } catch (java.io.IOException e) {
            throw new AssertionError(e);
        }
        return bytes.toByteArray();
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L6W115MonitorExitOfACallersLock.class.getClassLoader());
        }

        Class<?> define(byte[] b) {
            return defineClass("Exit", b, 0, b.length);
        }
    }

    static Method exit;

    /** 1 when the callee threw IMSE with a null message, 2 for another outcome; 10 when the lock is still held after. */
    static int callUnder(Object o) {
        int r = 0;
        synchronized (o) {
            try {
                exit.invoke(null, o);
            } catch (java.lang.reflect.InvocationTargetException e) {
                Throwable c = e.getCause();
                r = (c instanceof IllegalMonitorStateException && c.getMessage() == null) ? 1 : 2;
            } catch (ReflectiveOperationException e) {
                r = 2;
            }
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
        Class<?> c = new Loader().define(exitClass());
        exit = c.getMethod("exit", Object.class);
        Object lock = new Object();

        System.out.println("cold: " + withWatchdog(() -> {
            int r = callUnder(lock);
            return "imse=" + (r % 10 == 1 ? 1 : 0) + " nullMessage=" + (r % 10 == 1) + " heldAfter=" + (r >= 10);
        }));

        System.out.println("hot: " + withWatchdog(() -> {
            int imse = 0;
            int held = 0;
            int other = 0;
            boolean nullMessage = true;
            for (int i = 0; i < 30_000; i++) {
                int r = callUnder(lock);
                if (r % 10 == 1) {
                    imse++;
                } else {
                    other++;
                    nullMessage = false;
                }
                if (r >= 10) {
                    held++;
                }
            }
            return "imse=" + imse + " nullMessage=" + nullMessage + " heldAfter=" + held + " other=" + other;
        }));

        // Nobody holds the lock at the end: another thread can take it at once.
        boolean[] free = {false};
        Thread t = new Thread(() -> {
            synchronized (lock) {
                free[0] = true;
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(20_000);
        System.out.println("lock: free=" + free[0]);
    }
}
