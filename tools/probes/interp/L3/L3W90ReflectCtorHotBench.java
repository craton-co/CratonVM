// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 90, lane L3: is `Class.getConstructor()`'s Java
// half slow because it runs interpreted? `L3W89ReflectCtorSplitBench` makes
// 481 getConstructor() calls in all, just under CratonVM's JIT invocation
// threshold (500, `CRATONVM_JIT_THRESHOLD`), so `getConstructor`,
// `getConstructor0`, `privateGetDeclaredConstructors` and the copy chain may
// never be compiled there, while HotSpot's C1 takes them at ~200 calls. This
// bench measures the same fresh/again pair twice: "cold" exactly as wave 89
// did, then "hot" after 6000 getConstructor() calls on one class.
//
//   getConstructor-fresh-cold   first getConstructor() of a fresh, initialized
//                               class, before the warm-up (wave 89's row)
//   getConstructor-again-cold   a second getConstructor() of the same class
//                               (ReflectionData cached: search + copy only)
//   getConstructor-fresh-hot    the fresh row after the warm-up
//   getConstructor-again-hot    the again row after the warm-up
//   getConstructor-warm         getConstructor() of one class in a loop (the
//                               cached path, compiled)
//
// Reading: again-cold minus again-hot is the price of running the cached Java
// path interpreted; fresh-hot minus again-hot minus the native
// (`CRATONVM_DBG_GDC_PROF`'s `native=`) is reflectionData() creation plus the
// native call boundary.
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>".
// Run: javac -d . L3W90ReflectCtorHotBench.java
//      java -cp . L3W90ReflectCtorHotBench                                       (HotSpot)
//      cratonvm --jdk-only -cp . L3W90ReflectCtorHotBench
//      CRATONVM_DBG_GDC_PROF=1 cratonvm --jdk-only -cp . L3W90ReflectCtorHotBench 2>gdc.txt
//      CRATONVM_DBG_INTERP_FRAMES=1 cratonvm --jdk-only -cp . L3W90ReflectCtorHotBench 2>interp.txt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum getConstructor-fresh-cold 200
// checksum getConstructor-again-cold 200
// checksum getConstructor-warm 20
// checksum getConstructor-fresh-hot 200
// checksum getConstructor-again-hot 200
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Constructor;
import java.util.Arrays;
import java.util.Locale;

public class L3W90ReflectCtorHotBench {
    public static class Plain {
        public Plain() {}
    }

    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L3W90ReflectCtorHotBench.class.getClassLoader());
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

    /** `count` fresh classes: getConstructor() twice each, timed apart. */
    static void freshPair(String suffix, boolean print, int count) throws Throwable {
        Loader loader = new Loader();
        long[] fresh = new long[count];
        long[] again = new long[count];
        int okFresh = 0;
        int okAgain = 0;
        for (int i = 0; i < count; i++) {
            String name = "Rh" + (classes++);
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
            print("getConstructor-fresh-" + suffix, fresh, okFresh);
            print("getConstructor-again-" + suffix, again, okAgain);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        Constructor<Plain> pc = Plain.class.getConstructor();
        pc.newInstance();

        // Cold: wave 89's shape (40 unprinted, then n printed).
        freshPair("cold", false, 40);
        freshPair("cold", true, n);

        // Warm-up: 6000 more calls (20 batches of 300), well past the
        // 500-call threshold, printed as the cached-path floor.
        long[] t = new long[20];
        int okWarm = 0;
        for (int b = 0; b < t.length; b++) {
            Constructor<?> last = null;
            long t0 = System.nanoTime();
            for (int i = 0; i < 300; i++) {
                last = Plain.class.getConstructor();
            }
            t[b] = (System.nanoTime() - t0) / 300;
            okWarm += last != null && last.equals(pc) ? 1 : 0;
        }
        print("getConstructor-warm", t, okWarm);

        // Hot: the same pair with the Java half compiled.
        freshPair("hot", false, 40);
        freshPair("hot", true, n);
        sink = pc;
    }
}
