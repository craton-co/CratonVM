// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 89, lane L3: `Class.getConstructor()` on a fresh
// class split into what Java can isolate, and the warm `newInstance()` floor.
// Read with the native census `CRATONVM_DBG_GDC_PROF=1` (stderr), which splits
// `getDeclaredConstructors0` itself:
//
//   getConstructor-fresh   first getConstructor() of a fresh, initialized class
//                          (ReflectionData + SoftReference, the native listing,
//                          searchConstructors, copyConstructor)
//   getConstructor-again   a second getConstructor() of the same class
//                          (ReflectionData cached: search + copy only)
//   soft-reference-new     `new SoftReference<>(o)` (what reflectionData() makes)
//   newInstance-warm       the same Constructor's newInstance() in a loop
//   new-direct             `new Plain()` in a loop (the floor)
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>".
// Run: javac -d . L3W89ReflectCtorSplitBench.java
//      java -cp . L3W89ReflectCtorSplitBench                                      (HotSpot)
//      cratonvm --jdk-only -cp . L3W89ReflectCtorSplitBench
//      CRATONVM_DBG_GDC_PROF=1 cratonvm --jdk-only -cp . L3W89ReflectCtorSplitBench 2>gdc.txt
//      CRATONVM_MH_REAL_CONSTRUCTOR_LANE_MEMO=1 cratonvm --jdk-only -cp . L3W89ReflectCtorSplitBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum getConstructor-fresh 200
// checksum getConstructor-again 200
// checksum soft-reference-new 20
// checksum newInstance-warm 20
// checksum new-direct 20
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.ref.SoftReference;
import java.lang.reflect.Constructor;
import java.util.Arrays;
import java.util.Locale;

public class L3W89ReflectCtorSplitBench {
    public static class Plain {
        public Plain() {}
    }

    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L3W89ReflectCtorSplitBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;
    static Object sink;

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

    interface Loop {
        int run(int n) throws Throwable;
    }

    /** 20 batches of `per` iterations; ns per iteration per batch. */
    static void batched(String name, boolean print, int per, Loop body) throws Throwable {
        long[] t = new long[20];
        int ok = 0;
        for (int b = 0; b < t.length; b++) {
            long t0 = System.nanoTime();
            ok += body.run(per);
            t[b] = (System.nanoTime() - t0) / per;
        }
        if (print) {
            print(name, t, ok);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        Constructor<Plain> pc = Plain.class.getConstructor();
        pc.newInstance();
        Object payload = new Object();

        for (int pass = 0; pass < 2; pass++) {
            boolean print = pass == 1;
            int count = print ? n : 40;
            Loader loader = new Loader();
            long[] fresh = new long[count];
            long[] again = new long[count];
            int okFresh = 0;
            int okAgain = 0;
            for (int i = 0; i < count; i++) {
                String name = "Rs" + (classes++);
                Class<?> c = loader.define(name, build(name));
                Class.forName(name, true, loader);
                long t0 = System.nanoTime();
                Constructor<?> k1 = c.getConstructor();
                fresh[i] = System.nanoTime() - t0;
                okFresh += k1.getParameterCount() == 0 ? 1 : 0;
                long t1 = System.nanoTime();
                Constructor<?> k2 = c.getConstructor();
                again[i] = System.nanoTime() - t1;
                okAgain += k2.equals(k1) && k2 != k1 ? 1 : 0;
            }
            if (print) {
                print("getConstructor-fresh", fresh, okFresh);
                print("getConstructor-again", again, okAgain);
            }

            batched("soft-reference-new", print, 2000, per -> {
                SoftReference<Object> last = null;
                for (int i = 0; i < per; i++) {
                    last = new SoftReference<>(payload);
                }
                return last.get() == payload ? 1 : 0;
            });
            batched("newInstance-warm", print, 2000, per -> {
                Object last = null;
                for (int i = 0; i < per; i++) {
                    last = pc.newInstance();
                }
                return last instanceof Plain ? 1 : 0;
            });
            batched("new-direct", print, 2000, per -> {
                Object last = null;
                for (int i = 0; i < per; i++) {
                    last = new Plain();
                }
                sink = last;
                return last instanceof Plain ? 1 : 0;
            });
        }
    }
}
