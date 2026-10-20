// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 88, lane L3: the reflective-constructor rows of
// L4W87DefineClassSplitBench, isolated, plus a trace check for the switch
// CRATONVM_MH_REAL_CONSTRUCTOR_LANE (default off).
//
// Rows (each "row <name> min <ns> med <ns> ns/op", over batches):
//   newInstance-warm   the same public no-arg Constructor, newInstance() in a loop
//                      (the accessor exists after the first call: every call is
//                      DirectConstructorHandleAccessor.newInstance -> invokeExact)
//   newInstance-2      a fresh class per sample (java.lang.classfile + a user
//                      loader), first newInstance() untimed, the SECOND timed
//   getConstructor     a fresh initialized class per sample, getConstructor() timed
// Deterministic lines: "checksum <row> <n>", and the trace of a throwing
// constructor reached through Constructor.newInstance (classes and methods of
// the cause's first six frames; HotSpot hides the LambdaForm frames).
//
// Run: javac -d . L3W88ReflectCtorBench.java
//      java -cp . L3W88ReflectCtorBench                                   (HotSpot)
//      cratonvm --jdk-only -cp . L3W88ReflectCtorBench
//      CRATONVM_MH_REAL_CONSTRUCTOR_LANE=1 cratonvm --jdk-only -cp . L3W88ReflectCtorBench
//      (and the same two with --compatible; --nojit must not regress)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// trace: java.lang.reflect.InvocationTargetException <- java.lang.IllegalStateException: boom
// frame: L3W88ReflectCtorBench$Thrower.<init>
// frame: jdk.internal.reflect.DirectConstructorHandleAccessor.invokeImpl
// frame: jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance
// frame: java.lang.reflect.Constructor.newInstanceWithCaller
// frame: java.lang.reflect.Constructor.newInstance
// frame: L3W88ReflectCtorBench.main
// checksum newInstance-warm 20
// checksum newInstance-2 200
// checksum getConstructor 200
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.util.Arrays;
import java.util.Locale;

public class L3W88ReflectCtorBench {
    public static class Plain {
        public Plain() {}
    }

    public static class Thrower {
        public Thrower() {
            throw new IllegalStateException("boom");
        }
    }

    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L3W88ReflectCtorBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    static void print(String name, long[] t, int ok) {
        long[] c = t.clone();
        Arrays.sort(c);
        double med = c.length % 2 == 1 ? c[c.length / 2] : (c[c.length / 2 - 1] + c[c.length / 2]) / 2.0;
        System.out.printf(Locale.ROOT, "row %s min %d med %.1f ns/op%n", name, c[0], med);
        System.out.println("checksum " + name + " " + ok);
    }

    public static void main(String[] args) throws Throwable {
        try {
            Thrower.class.getConstructor().newInstance();
            System.out.println("trace: no exception");
        } catch (InvocationTargetException e) {
            Throwable cause = e.getCause();
            System.out.println("trace: " + e.getClass().getName() + " <- " + cause);
            StackTraceElement[] st = cause.getStackTrace();
            for (int i = 0; i < Math.min(6, st.length); i++) {
                System.out.println("frame: " + st[i].getClassName() + "." + st[i].getMethodName());
            }
        }

        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;

        // newInstance-warm: 20 batches of 2000 calls; ns per call per batch.
        Constructor<Plain> pc = Plain.class.getConstructor();
        pc.newInstance();
        for (int pass = 0; pass < 2; pass++) {
            long[] t = new long[20];
            int ok = 0;
            for (int b = 0; b < t.length; b++) {
                long t0 = System.nanoTime();
                Object last = null;
                for (int i = 0; i < 2000; i++) {
                    last = pc.newInstance();
                }
                t[b] = (System.nanoTime() - t0) / 2000;
                ok += last instanceof Plain ? 1 : 0;
            }
            if (pass == 1) {
                print("newInstance-warm", t, ok);
            }
        }

        for (int pass = 0; pass < 2; pass++) {
            int count = pass == 1 ? n : 40;
            Loader loader = new Loader();
            long[] t2 = new long[count];
            long[] tg = new long[count];
            int ok2 = 0;
            int okg = 0;
            for (int i = 0; i < count; i++) {
                String name = "Rc" + (classes++);
                Class<?> c = loader.define(name, build(name));
                Class.forName(name, true, loader);
                long g0 = System.nanoTime();
                Constructor<?> ctor = c.getConstructor();
                tg[i] = System.nanoTime() - g0;
                okg += ctor.getParameterCount() == 0 ? 1 : 0;
                ctor.newInstance();
                long t0 = System.nanoTime();
                Object o = ctor.newInstance();
                t2[i] = System.nanoTime() - t0;
                ok2 += o.getClass() == c ? 1 : 0;
            }
            if (pass == 1) {
                print("newInstance-2", t2, ok2);
                print("getConstructor", tg, okg);
            }
        }
    }
}
