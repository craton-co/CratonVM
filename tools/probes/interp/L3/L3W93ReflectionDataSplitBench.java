// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 93, lane L3 (i88-L3): what a fresh class's
// `Class.reflectionData()` creation costs inside `getConstructor()`, measured
// WITHOUT the native census. Wave 92 pointed at the `SoftReference`
// constructor native (`native_soft_ref_init`); by reading, that native is not
// registered on this path in either default mode (the `java.lang.ref`
// constructors are retired: `--jdk-only` by `RETIRED_SHADOW_SBR3_REF_TRIPLES`,
// `--compatible` by the `JdkReferences` compat family), so `new
// SoftReference<>(rd)` is the JDK's constructor bytecode plus discovery at
// allocation. These rows settle how much of the fresh-class gap that is.
//
//   soft-reference-new        `new SoftReference<>(o)` in a loop (hot)
//   soft-reference-after-define
//                             ONE `new SoftReference<>(o)` timed right after a
//                             fresh class is defined and initialised (what
//                             newReflectionData pays, epoch moved by the define)
//   holder-new-after-define   the control: ONE `new Holder(o)` (a one-field
//                             user object) timed at the same point
//   getInterfaces-fresh       first `getInterfaces()` of a fresh class:
//                             reflectionData() creation + the cheap
//                             `getInterfaces0` native + an empty-array clone
//   getInterfaces-again       the second call (ReflectionData cached)
//   getConstructor-fresh      first `getConstructor()` of a fresh class (the
//                             wave-90 row, reflectionData() included)
//   getConstructor-after-rd   first `getConstructor()` of a fresh class whose
//                             ReflectionData already exists (an untimed
//                             `getInterfaces()` made it): the same work minus
//                             reflectionData() creation
//
// Reading: getConstructor-fresh minus getConstructor-after-rd is the share of
// reflectionData() creation; getInterfaces-fresh minus getInterfaces-again
// bounds it from the other side. soft-reference-after-define minus
// holder-new-after-define is the SoftReference's own share of that.
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>".
// Run: javac -d . L3W93ReflectionDataSplitBench.java
//      java -cp . L3W93ReflectionDataSplitBench                                  (HotSpot)
//      cratonvm --jdk-only -cp . L3W93ReflectionDataSplitBench
//      cratonvm --compatible -cp . L3W93ReflectionDataSplitBench
// Positive control that the constructor native never runs (expect no
// "wire=1" line in either mode; a line would name the reference class):
//      CRATONVM_DBG_REFDISC=1 cratonvm --jdk-only -cp . L3W93ReflectionDataSplitBench 2>refdisc.txt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum soft-reference-new 20
// checksum soft-reference-after-define 200
// checksum holder-new-after-define 200
// checksum getInterfaces-fresh 200
// checksum getInterfaces-again 200
// checksum getConstructor-fresh 200
// checksum getConstructor-after-rd 200
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.ref.SoftReference;
import java.lang.reflect.Constructor;
import java.util.Arrays;
import java.util.Locale;

public class L3W93ReflectionDataSplitBench {
    public static class Plain {
        public Plain() {}
    }

    static final class Holder {
        final Object value;

        Holder(Object value) {
            this.value = value;
        }
    }

    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L3W93ReflectionDataSplitBench.class.getClassLoader());
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

    /** A fresh, defined and initialised class of `loader`. */
    static Class<?> fresh(Loader loader) throws Throwable {
        String name = "Rd" + (classes++);
        Class<?> c = loader.define(name, build(name));
        Class.forName(name, true, loader);
        return c;
    }

    static void print(String name, long[] t, int ok) {
        long[] c = t.clone();
        Arrays.sort(c);
        double med = c.length % 2 == 1 ? c[c.length / 2] : (c[c.length / 2 - 1] + c[c.length / 2]) / 2.0;
        System.out.printf(Locale.ROOT, "row %s min %d med %.1f ns/op%n", name, c[0], med);
        System.out.println("checksum " + name + " " + ok);
    }

    /** One pass of every per-class row over `count` fresh classes each. */
    static void pass(boolean print, int count, Object payload) throws Throwable {
        Loader loader = new Loader();

        long[] soft = new long[count];
        long[] holder = new long[count];
        int okSoft = 0;
        int okHolder = 0;
        for (int i = 0; i < count; i++) {
            fresh(loader);
            long t0 = System.nanoTime();
            SoftReference<Object> r = new SoftReference<>(payload);
            soft[i] = System.nanoTime() - t0;
            okSoft += r.get() == payload ? 1 : 0;
            fresh(loader);
            long t1 = System.nanoTime();
            Holder h = new Holder(payload);
            holder[i] = System.nanoTime() - t1;
            okHolder += h.value == payload ? 1 : 0;
        }

        long[] ifFresh = new long[count];
        long[] ifAgain = new long[count];
        int okIfFresh = 0;
        int okIfAgain = 0;
        for (int i = 0; i < count; i++) {
            Class<?> c = fresh(loader);
            long t0 = System.nanoTime();
            Class<?>[] a1 = c.getInterfaces();
            ifFresh[i] = System.nanoTime() - t0;
            okIfFresh += a1.length == 0 ? 1 : 0;
            long t1 = System.nanoTime();
            Class<?>[] a2 = c.getInterfaces();
            ifAgain[i] = System.nanoTime() - t1;
            okIfAgain += a2.length == 0 ? 1 : 0;
        }

        long[] ctorFresh = new long[count];
        long[] ctorAfterRd = new long[count];
        int okCtorFresh = 0;
        int okCtorAfterRd = 0;
        for (int i = 0; i < count; i++) {
            Class<?> c = fresh(loader);
            long t0 = System.nanoTime();
            Constructor<?> k1 = c.getConstructor();
            ctorFresh[i] = System.nanoTime() - t0;
            okCtorFresh += k1.getParameterCount() == 0 ? 1 : 0;

            Class<?> d = fresh(loader);
            sink = d.getInterfaces();
            long t1 = System.nanoTime();
            Constructor<?> k2 = d.getConstructor();
            ctorAfterRd[i] = System.nanoTime() - t1;
            okCtorAfterRd += k2.getParameterCount() == 0 ? 1 : 0;
        }

        if (print) {
            print("soft-reference-after-define", soft, okSoft);
            print("holder-new-after-define", holder, okHolder);
            print("getInterfaces-fresh", ifFresh, okIfFresh);
            print("getInterfaces-again", ifAgain, okIfAgain);
            print("getConstructor-fresh", ctorFresh, okCtorFresh);
            print("getConstructor-after-rd", ctorAfterRd, okCtorAfterRd);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        Object payload = new Object();
        Constructor<Plain> pc = Plain.class.getConstructor();
        pc.newInstance();

        pass(false, 40, payload);

        // Warm-up past the JIT threshold, as wave 90's "hot" phase: the cached
        // reflective paths and the SoftReference constructor chain.
        int warm = 0;
        for (int i = 0; i < 6000; i++) {
            warm += Plain.class.getConstructor() != null ? 1 : 0;
            warm += Plain.class.getInterfaces().length;
        }
        sink = warm;

        // Hot SoftReference loop: 20 batches of 2000.
        long[] t = new long[20];
        int ok = 0;
        for (int b = 0; b < t.length; b++) {
            SoftReference<Object> last = null;
            long t0 = System.nanoTime();
            for (int i = 0; i < 2000; i++) {
                last = new SoftReference<>(payload);
            }
            t[b] = (System.nanoTime() - t0) / 2000;
            ok += last.get() == payload ? 1 : 0;
        }
        print("soft-reference-new", t, ok);

        pass(true, n, payload);
        sink = pc;
    }
}
