// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Item 2 of docs/internal/fixed-bugs/interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md
// (2026-10-07): a compiled frame that is inside a call when a class it
// spliced is retransformed, where the call's SUCCESSOR is one the single-pass
// post-call exit claim refused until then -- an `invokedynamic`, a
// `monitorenter`, a `monitorexit`, or a local exception handler's entry
// reached by the call's fall-through. The first read of the spliced
// `Callee.value()` after the call returns must run the NEW bytecode (HotSpot
// returns into the interpreter at the call's return point).
//
// Rows, each a method that calls a gate (a real call: it carries an
// exception table) and then reads `Callee.value()` once:
//   indy-*     `"v=" + gateI(..)`: javac's `invokestatic; invokedynamic`
//              (the string-concat bootstrap, which the single-pass tier
//              bridges);
//   menter-*   generated: `invokestatic gate; monitorenter` (the gate
//              returns the lock object);
//   mexit-*    generated: `monitorenter LOCK; invokestatic gate; monitorexit`;
//   handler-*  generated: `invokestatic gateT` falling into the entry of a
//              handler that protects an earlier call.
// `*-parked`: a worker warms the method up, then parks in the gate while
// main retransforms `Callee`; `*-self`: warmed up on main, then the gate
// retransforms on the calling thread itself. The generated class is built
// with `java.lang.classfile` and defined through the probe's lookup.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// indy-parked first-after-new=true
// indy-self first-after-new=true
// menter-parked first-after-new=true
// menter-self first-after-new=true
// mexit-parked first-after-new=true
// mexit-self first-after-new=true
// handler-parked first-after-new=true
// handler-self first-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM: see the i24-L6 page's "Fixed (2026-10-07)" for the measured
// lines before and after `op_invoke.rs::POST_CALL_EXIT_CLAIMS_AT_INDY_AND_MONITORS`
// and `POST_CALL_EXIT_CLAIMS_AT_HANDLER_ENTRIES`. POSITIVE CONTROL,
// `CRATONVM_DBG_DEOPT=1`: a `post-call exit claim at bci=N of
// I24PostCallExitSuccessors...: Own` line per shape.
//
// SETUP: a jar whose manifest has
//     Premain-Class: I24PostCallExitSuccessors$Agent
//     Can-Retransform-Classes: true
// containing I24PostCallExitSuccessors*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I24PostCallExitSuccessors
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class I24PostCallExitSuccessors {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        static int value() {
            return 12345;
        }
    }

    /** Rewrites `sipush 12345; ireturn` to answer 12345 + n on the n-th transform. */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"I24PostCallExitSuccessors$Callee".equals(className)) {
                return null;
            }
            bumps++;
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = (byte) (0x39 + bumps);
                }
            }
            return out;
        }
    }

    static final long WARM = 1L << 16;
    static final Object LOCK = new Object();
    static final RuntimeException THROWN = new RuntimeException("gate");
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static volatile CountDownLatch parkLatch;
    static volatile boolean parkedIn;
    static volatile int[] parkedResult;
    static volatile long sink;

    /** The gate's body: park on `latch` (a parked row) or retransform (a self row). */
    static void pass(CountDownLatch latch, boolean now) throws Exception {
        if (latch != OPEN) {
            parkedIn = true;
        }
        latch.await();
        if (now) {
            inst.retransformClasses(Callee.class);
        }
    }

    /** A real call (its exception table keeps it out of every splice); returns `LOCK`. */
    public static Object gate(CountDownLatch latch, boolean now) {
        try {
            pass(latch, now);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        return LOCK;
    }

    /** As `gate`, returning a throwable. */
    public static Throwable gateT(CountDownLatch latch, boolean now) {
        try {
            pass(latch, now);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        return THROWN;
    }

    /** As `gate`, returning an int. */
    public static int gateI(CountDownLatch latch, boolean now) {
        try {
            pass(latch, now);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        return 7;
    }

    /** Never throws; the call the generated handler protects. */
    public static void nothing() {}

    /** The `invokedynamic` successor, as javac writes it. */
    public static int indy(CountDownLatch latch, boolean now) {
        String s = "v=" + gateI(latch, now);
        return Callee.value() + (s.length() >>> 31);
    }

    static final ClassDesc ME = ClassDesc.of("I24PostCallExitSuccessors");
    static final ClassDesc CALLEE = ClassDesc.of("I24PostCallExitSuccessors$Callee");
    static final ClassDesc LATCH = ClassDesc.of("java.util.concurrent.CountDownLatch");
    static final MethodTypeDesc ROW = MethodTypeDesc.of(ConstantDescs.CD_int, LATCH, ConstantDescs.CD_boolean);

    /** `menter`, `mexit` and `handler`, which javac cannot write. */
    static byte[] generated() {
        return ClassFile.of().build(ClassDesc.of("I24PostCallExitSuccessorsGen"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            // static int menter(latch, now) {
            //   gate(latch, now); monitorenter; v = Callee.value(); LOCK; monitorexit; return v; }
            cb.withMethodBody("menter", ROW, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                code.aload(0).iload(1)
                        .invokestatic(ME, "gate", MethodTypeDesc.of(ConstantDescs.CD_Object, LATCH, ConstantDescs.CD_boolean))
                        .monitorenter()
                        .invokestatic(CALLEE, "value", MethodTypeDesc.of(ConstantDescs.CD_int))
                        .istore(2)
                        .getstatic(ME, "LOCK", ConstantDescs.CD_Object)
                        .monitorexit()
                        .iload(2)
                        .ireturn();
            });
            // static int mexit(latch, now) {
            //   LOCK; monitorenter; gate(latch, now); monitorexit; return Callee.value(); }
            cb.withMethodBody("mexit", ROW, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                code.getstatic(ME, "LOCK", ConstantDescs.CD_Object)
                        .monitorenter()
                        .aload(0).iload(1)
                        .invokestatic(ME, "gate", MethodTypeDesc.of(ConstantDescs.CD_Object, LATCH, ConstantDescs.CD_boolean))
                        .monitorexit()
                        .invokestatic(CALLEE, "value", MethodTypeDesc.of(ConstantDescs.CD_int))
                        .ireturn();
            });
            // static int handler(latch, now) {
            //   try { nothing(); } -- handler H
            //   gateT(latch, now);  -- falls into H with the throwable
            //   H: astore_2; return Callee.value(); }
            cb.withMethodBody("handler", ROW, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label start = code.newLabel();
                Label end = code.newLabel();
                Label h = code.newLabel();
                code.labelBinding(start)
                        .invokestatic(ME, "nothing", MethodTypeDesc.of(ConstantDescs.CD_void))
                        .labelBinding(end)
                        .aload(0).iload(1)
                        .invokestatic(ME, "gateT", MethodTypeDesc.of(ClassDesc.of("java.lang.Throwable"), LATCH, ConstantDescs.CD_boolean))
                        .labelBinding(h)
                        .astore(2)
                        .invokestatic(CALLEE, "value", MethodTypeDesc.of(ConstantDescs.CD_int))
                        .ireturn()
                        .exceptionCatch(start, end, h, ClassDesc.of("java.lang.Throwable"));
            });
        });
    }

    static int call(Method m, CountDownLatch latch, boolean now) throws Exception {
        return (Integer) m.invoke(null, latch, now);
    }

    static void rows(String name, Method m) throws Exception {
        Instrumentation i = inst;
        // parked: a worker warms the method up, then parks in its gate.
        CountDownLatch go = new CountDownLatch(1);
        parkedIn = false;
        Thread worker = new Thread(() -> {
            try {
                long sum = 0;
                for (long k = 0; k < WARM; k++) {
                    sum += call(m, OPEN, false);
                }
                sink = sum;
                int before = Callee.value();
                parkedResult = new int[] {before, call(m, go, false)};
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!parkedIn) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(Callee.class);
        go.countDown();
        worker.join();
        int[] p = parkedResult;
        System.out.println(name + "-parked first-after-new=" + (p[1] != p[0]));

        // self: warmed up on main (already, by the worker's warm-up of the
        // same method), then the gate retransforms on this thread.
        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += call(m, OPEN, false);
        }
        sink = sum;
        int before = Callee.value();
        int after = call(m, OPEN, true);
        System.out.println(name + "-self first-after-new=" + (after != before));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        Class<?> gen = MethodHandles.lookup().defineClass(generated());
        Class<?>[] sig = {CountDownLatch.class, boolean.class};
        rows("indy", I24PostCallExitSuccessors.class.getMethod("indy", sig));
        rows("menter", gen.getMethod("menter", sig));
        rows("mexit", gen.getMethod("mexit", sig));
        rows("handler", gen.getMethod("handler", sig));
    }
}
