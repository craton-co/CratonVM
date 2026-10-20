// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe (diagnostic)
//
// Interpreter round i6 wave 134, lane L2c: splits the JIT-only defect of
// tools/probes/interp/L2b/L2bW133GenericIndyThreadRow.java (phase1
// mismatches=1196, phase2 mismatches=1200, HotSpot 0; --nojit equal) into the
// layer that answers wrong. Page:
// docs/known-issues/interpreter/i134-L2c-generic-indy-false-arm-under-jit-20261224.md.
//
// The L2b numbers are exactly "every b=false iteration of every worker fails
// BOTH checks": 4 threads x 150 b=false iterations x 2 checks = 1200, and the
// 4 that pass in phase 1 are the two b=false iterations at n=0 (seeds 1 and
// 3) that ran before anything was compiled. Every main-thread line of that
// probe passes b=true, which is why they all equal HotSpot. So the question
// is not per-instruction linkage (the main thread's `both` answers pair#80 |
// pair#81) but which layer loses `b=false` or `-i`: the target `tag` itself
// (`prefix + (b ? i : -i)`), the bound handle, the generic call site's boolean
// boxing, the compiled `walk`/`both` staging of a `Z` operand, or a compiled
// caller passing `b`. Each section below asks one of those, single threaded
// first:
//
//   direct      compiled loop calling tag(...) directly (no handle, no indy)
//   mh          compiled loop calling the bound handle through invokeExact
//   indy-main   compiled loop on main calling walk/both/flag (flag answers the
//               boolean the TARGET saw: prefix + i + "/" + b)
//   indy-const  as indy-main with a literal `false` argument
//   indy-one    indy-main's alternating loop on ONE worker thread
//   indy-four   L2b's four-thread shape, the three checks counted apart
//
// Each section warms with b=true first (the L2b profile: main linked and ran
// every site with b=true), then alternates. A mismatch line names the first
// wrong answer; HotSpot prints none.
//
// Run: javac -d . L2cW134GenericIndyFalseArm.java
//      java -cp . L2cW134GenericIndyFalseArm                                    (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L2cW134GenericIndyFalseArm
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// linked bootstraps=83 walk=860 both=pair#80:3|pair#81:3 flag=flag#82:3/true
// direct mismatches=0
// mh mismatches=0
// indy-main walk=0 both=0 flag=0
// indy-const walk=0 both=0 flag=0
// indy-one walk=0 both=0 flag=0
// indy-four walk=0 both=0 flag=0
// single walk=940 both=pair#80:-7|pair#81:-7 flag=flag#82:7/false
// bootstraps=83
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

public class L2cW134GenericIndyFalseArm {
    public interface Walker {
        int walk(Object o, int i, boolean b);

        String both(Object o, int i, boolean b);

        String flag(Object o, int i, boolean b);
    }

    static final int SITES = 80;
    static final int WARM = 3000;
    static final int RUN = 3000;

    static int bootstraps;
    static volatile Object sink;

    public static Object tag(String prefix, Object o, int i, boolean b) {
        return prefix + (b ? i : -i);
    }

    public static Object tagFlag(String prefix, Object o, int i, boolean b) {
        return prefix + i + "/" + b;
    }

    public static synchronized CallSite bsm(MethodHandles.Lookup l, String name, MethodType type)
            throws ReflectiveOperationException {
        int id = bootstraps++;
        String target = name.startsWith("flag") ? "tagFlag" : "tag";
        MethodHandle h = l.findStatic(L2cW134GenericIndyFalseArm.class, target,
                MethodType.methodType(Object.class, String.class, Object.class, int.class, boolean.class));
        return new MutableCallSite(MethodHandles.insertArguments(h, 0, name + "#" + id + ":"));
    }

    /** The signed value the target renders, without a ternary. */
    static int signed(int i, boolean b) {
        int v = i;
        if (!b) {
            v = -i;
        }
        return v;
    }

    static int expectedWalk(int i, boolean b) {
        int digits = Integer.toString(signed(i, b)).length();
        int sum = 0;
        for (int k = 0; k < SITES; k++) {
            sum += new StringBuilder("site").append(k).append('#').append(k).append(':').length() + digits;
        }
        return sum;
    }

    static String expectedBoth(int i, boolean b) {
        String v = Integer.toString(signed(i, b));
        return new StringBuilder("pair#80:").append(v).append("|pair#81:").append(v).toString();
    }

    static String expectedFlag(int i, boolean b) {
        return new StringBuilder("flag#82:").append(i).append('/').append(b).toString();
    }

    /** Counts of the three checks, and the first wrong answer of each. */
    static final class Tally {
        final AtomicInteger walk = new AtomicInteger();
        final AtomicInteger both = new AtomicInteger();
        final AtomicInteger flag = new AtomicInteger();
        final AtomicReference<String> first = new AtomicReference<>();

        void check(Walker w, int i, boolean b) {
            int got = w.walk("x", i, b);
            if (got != expectedWalk(i, b)) {
                walk.incrementAndGet();
                first.compareAndSet(null, "walk(" + i + "," + b + ")=" + got + " want " + expectedWalk(i, b));
            }
            String s = w.both("y", i, b);
            if (!s.equals(expectedBoth(i, b))) {
                both.incrementAndGet();
                first.compareAndSet(null, "both(" + i + "," + b + ")=" + s);
            }
            String f = w.flag("z", i, b);
            if (!f.equals(expectedFlag(i, b))) {
                flag.incrementAndGet();
                first.compareAndSet(null, "flag(" + i + "," + b + ")=" + f);
            }
        }

        void print(String label) {
            System.out.println(label + " walk=" + walk.get() + " both=" + both.get() + " flag=" + flag.get());
            if (first.get() != null) {
                System.out.println(label + " first-mismatch " + first.get());
            }
        }
    }

    static void warm(Walker w) {
        for (int n = 0; n < WARM; n++) {
            int i = n % 1000;
            sink = w.walk("x", i, true);
            sink = w.both("y", i, true);
            sink = w.flag("z", i, true);
        }
    }

    static void alternate(Walker w, Tally t, int seed, int iters) {
        for (int n = 0; n < iters; n++) {
            int i = (n * 7 + seed) % 1000;
            boolean b = ((n + seed) & 1) == 0;
            t.check(w, i, b);
        }
    }

    static void constFalse(Walker w, Tally t) {
        for (int n = 1; n <= RUN; n++) {
            int i = n % 1000;
            int got = w.walk("x", i, false);
            if (got != expectedWalk(i, false)) {
                t.walk.incrementAndGet();
                t.first.compareAndSet(null, "walk(" + i + ",false)=" + got);
            }
            String s = w.both("y", i, false);
            if (!s.equals(expectedBoth(i, false))) {
                t.both.incrementAndGet();
                t.first.compareAndSet(null, "both(" + i + ",false)=" + s);
            }
            String f = w.flag("z", i, false);
            if (!f.equals(expectedFlag(i, false))) {
                t.flag.incrementAndGet();
                t.first.compareAndSet(null, "flag(" + i + ",false)=" + f);
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        Class<?> gen = MethodHandles.lookup().defineClass(genBytes());
        Walker w = (Walker) gen.getConstructor().newInstance();

        int walk0 = w.walk("x", 3, true);
        String both0 = w.both("y", 3, true);
        String flag0 = w.flag("z", 3, true);
        System.out.println("linked bootstraps=" + bootstraps + " walk=" + walk0 + " both=" + both0
                + " flag=" + flag0);

        // direct: the target method itself, called by a compiled loop.
        int direct = 0;
        String directFirst = null;
        for (int n = 0; n < WARM; n++) {
            sink = tag("p:", null, n % 1000, true);
        }
        for (int n = 0; n < RUN; n++) {
            int i = n % 1000;
            boolean b = (n & 1) == 0;
            Object got = tag("p:", null, i, b);
            String want = "p:" + Integer.toString(signed(i, b));
            if (!want.equals(got)) {
                direct++;
                if (directFirst == null) {
                    directFirst = "tag(" + i + "," + b + ")=" + got;
                }
            }
        }
        System.out.println("direct mismatches=" + direct);
        if (directFirst != null) {
            System.out.println("direct first-mismatch " + directFirst);
        }

        // mh: the bound handle of the call sites' shape, invokeExact.
        MethodHandle bound = MethodHandles.insertArguments(
                MethodHandles.lookup().findStatic(L2cW134GenericIndyFalseArm.class, "tag",
                        MethodType.methodType(Object.class, String.class, Object.class, int.class,
                                boolean.class)),
                0, "q:");
        int mh = 0;
        String mhFirst = null;
        for (int n = 0; n < WARM; n++) {
            sink = (Object) bound.invokeExact((Object) "x", n % 1000, true);
        }
        for (int n = 0; n < RUN; n++) {
            int i = n % 1000;
            boolean b = (n & 1) == 0;
            Object got = (Object) bound.invokeExact((Object) "x", i, b);
            String want = "q:" + Integer.toString(signed(i, b));
            if (!want.equals(got)) {
                mh++;
                if (mhFirst == null) {
                    mhFirst = "mh(" + i + "," + b + ")=" + got;
                }
            }
        }
        System.out.println("mh mismatches=" + mh);
        if (mhFirst != null) {
            System.out.println("mh first-mismatch " + mhFirst);
        }

        // indy-main: the generic sites from a compiled loop on main.
        warm(w);
        Tally main = new Tally();
        alternate(w, main, 0, RUN);
        main.print("indy-main");

        Tally konst = new Tally();
        constFalse(w, konst);
        konst.print("indy-const");

        Tally one = new Tally();
        Thread t1 = new Thread(() -> alternate(w, one, 1, RUN));
        t1.start();
        t1.join();
        one.print("indy-one");

        Tally four = new Tally();
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            final int seed = t;
            ts[t] = new Thread(() -> alternate(w, four, seed, 300));
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        four.print("indy-four");

        System.out.println("single walk=" + w.walk("x", 7, false) + " both=" + w.both("y", 7, false)
                + " flag=" + w.flag("z", 7, false));
        System.out.println("bootstraps=" + bootstraps);
    }

    // ---------------------------------------------------------------------
    // Class assembly (the L2b probe's, plus a `flag` site)
    // ---------------------------------------------------------------------

    static byte[] genBytes() {
        ClassDesc self = ClassDesc.of("L2cW134GenericIndyFalseArm");
        ClassDesc walker = ClassDesc.of("L2cW134GenericIndyFalseArm$Walker");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        MethodTypeDesc siteType = MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object,
                ConstantDescs.CD_int, ConstantDescs.CD_boolean);
        MethodTypeDesc walkType = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object,
                ConstantDescs.CD_int, ConstantDescs.CD_boolean);
        MethodTypeDesc strType = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_Object,
                ConstantDescs.CD_int, ConstantDescs.CD_boolean);
        MethodTypeDesc lengthType = MethodTypeDesc.of(ConstantDescs.CD_int);
        MethodTypeDesc concatType = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_String);
        return ClassFile.of().build(ClassDesc.of("GenL2cW134FalseArm"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(ConstantDescs.CD_Object);
            cb.withInterfaceSymbols(walker);
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, code -> code
                    .aload(0)
                    .invokespecial(ConstantDescs.CD_Object, "<init>", ConstantDescs.MTD_void)
                    .return_());
            cb.withMethodBody("walk", walkType, ClassFile.ACC_PUBLIC, code -> {
                code.iconst_0().istore(4);
                for (int k = 0; k < SITES; k++) {
                    code.aload(1).iload(2).iload(3)
                            .invokedynamic(DynamicCallSiteDesc.of(bsm, "site" + k, siteType))
                            .checkcast(ConstantDescs.CD_String)
                            .invokevirtual(ConstantDescs.CD_String, "length", lengthType)
                            .iload(4).iadd().istore(4);
                }
                code.iload(4).ireturn();
            });
            DynamicCallSiteDesc pair = DynamicCallSiteDesc.of(bsm, "pair", siteType);
            cb.withMethodBody("both", strType, ClassFile.ACC_PUBLIC, code -> code
                    .aload(1).iload(2).iload(3).invokedynamic(pair)
                    .checkcast(ConstantDescs.CD_String)
                    .ldc("|")
                    .invokevirtual(ConstantDescs.CD_String, "concat", concatType)
                    .aload(1).iload(2).iload(3).invokedynamic(pair)
                    .checkcast(ConstantDescs.CD_String)
                    .invokevirtual(ConstantDescs.CD_String, "concat", concatType)
                    .areturn());
            cb.withMethodBody("flag", strType, ClassFile.ACC_PUBLIC, code -> code
                    .aload(1).iload(2).iload(3)
                    .invokedynamic(DynamicCallSiteDesc.of(bsm, "flag", siteType))
                    .checkcast(ConstantDescs.CD_String)
                    .areturn());
        });
    }
}
