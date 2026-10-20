// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 133, lane L2b (`CRATONVM_INDY_GENERIC_THREAD_ROW`,
// default off; stage 3 of
// docs/internal/proposals/interpreter/i2-L6-proposal-generic-indy-invoke-path-20260923.md):
// a `Linked` GENERIC `invokedynamic` row (a bootstrap that is not a JDK
// factory, the shape of every Groovy / JRuby call site) is copied into the
// executing thread's per-instruction table as a heap-free marker, so the next
// execution skips the per-entry resolution-cache probe. The risks this probe
// covers, every line identical with the switch off and on:
//
//   * per-instruction linkage: `both` executes ONE constant-pool entry from two
//     instructions; each instruction is linked (and bootstrapped) separately,
//     so the two results name different call sites (JVMS 5.4.3.6);
//   * the thread table is direct-mapped (64 slots): `walk` runs 80 sites, so
//     copies evict each other and every site must still answer its own target;
//   * four threads run the linked sites at once (linked first on main, so the
//     bootstrap count is deterministic);
//   * the copy holds no CallSite address: after System.gc() over garbage the
//     sites still answer, and after setTarget on every MutableCallSite the
//     threads see the new targets.
//
// The class is assembled with java.lang.classfile and defined with
// Lookup.defineClass. Timing lines start with "TIME ".
//
// Run: javac -d . L2bW133GenericIndyThreadRow.java
//      java -cp . L2bW133GenericIndyThreadRow                                    (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L2bW133GenericIndyThreadRow
//      CRATONVM_INDY_GENERIC_THREAD_ROW=1 cratonvm ... L2bW133GenericIndyThreadRow
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint; TIME lines removed)
// linked bootstraps=82 walk=860 both=pair#80:3|pair#81:3
// phase1 threads=4 mismatches=0
// after-gc walk=860 both=pair#80:8|pair#81:8
// phase2 threads=4 mismatches=0 walk=390
// bootstraps=82
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
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

public class L2bW133GenericIndyThreadRow {
    public interface Walker {
        int walk(Object o, int i, boolean b);

        String both(Object o, int i, boolean b);
    }

    static final int SITES = 80;
    static final int THREADS = 4;

    static int bootstraps;
    static final List<MutableCallSite> SITE_LIST = new ArrayList<>();
    static final List<String> SITE_NAMES = new ArrayList<>();
    static volatile Object sink;

    public static Object tag(String prefix, Object o, int i, boolean b) {
        return prefix + (b ? i : -i);
    }

    public static synchronized CallSite bsm(MethodHandles.Lookup l, String name, MethodType type)
            throws ReflectiveOperationException {
        int id = bootstraps++;
        MethodHandle tag = l.findStatic(L2bW133GenericIndyThreadRow.class, "tag",
                MethodType.methodType(Object.class, String.class, Object.class, int.class, boolean.class));
        MutableCallSite site = new MutableCallSite(MethodHandles.insertArguments(tag, 0, name + "#" + id + ":"));
        SITE_LIST.add(site);
        SITE_NAMES.add(name);
        return site;
    }

    /** What `walk` answers when site k's prefix is prefix(k). */
    static int expectedWalk(int i, boolean b, boolean retargeted) {
        int sum = 0;
        String v = String.valueOf(b ? i : -i);
        for (int k = 0; k < SITES; k++) {
            String prefix = retargeted ? "r" + k + ":" : "site" + k + "#" + k + ":";
            sum += prefix.length() + v.length();
        }
        return sum;
    }

    static int runThreads(Walker w, int iters, boolean retargeted) throws InterruptedException {
        AtomicInteger mismatches = new AtomicInteger();
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int seed = t;
            ts[t] = new Thread(() -> {
                for (int n = 0; n < iters; n++) {
                    int i = (n * 7 + seed) % 1000;
                    boolean b = ((n + seed) & 1) == 0;
                    if (w.walk("x", i, b) != expectedWalk(i, b, retargeted)) {
                        mismatches.incrementAndGet();
                    }
                    String both = w.both("y", i, b);
                    String v = String.valueOf(b ? i : -i);
                    if (!both.equals("pair#80:" + v + "|pair#81:" + v)) {
                        mismatches.incrementAndGet();
                    }
                }
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        return mismatches.get();
    }

    public static void main(String[] args) throws Throwable {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        Class<?> gen = MethodHandles.lookup().defineClass(genBytes());
        Walker w = (Walker) gen.getConstructor().newInstance();

        // Link every instruction on this thread first: 80 walk sites, then the
        // two `both` instructions over one entry.
        int walk = w.walk("x", 3, true);
        String both = w.both("y", 3, true);
        System.out.println("linked bootstraps=" + bootstraps + " walk=" + walk + " both=" + both);

        System.out.println("phase1 threads=" + THREADS + " mismatches=" + runThreads(w, iters, false));

        // Move what can move: drop garbage around the call sites and collect.
        for (int r = 0; r < 20; r++) {
            Object[] junk = new Object[2000];
            for (int j = 0; j < junk.length; j++) {
                junk[j] = new int[16];
            }
            sink = junk;
        }
        sink = null;
        System.gc();
        System.out.println("after-gc walk=" + w.walk("x", 8, true) + " both=" + w.both("y", 8, true));

        MethodHandle tag = MethodHandles.lookup().findStatic(L2bW133GenericIndyThreadRow.class, "tag",
                MethodType.methodType(Object.class, String.class, Object.class, int.class, boolean.class));
        for (int k = 0; k < SITE_LIST.size(); k++) {
            String name = SITE_NAMES.get(k);
            if (name.startsWith("site")) {
                SITE_LIST.get(k).setTarget(MethodHandles.insertArguments(tag, 0, "r" + name.substring(4) + ":"));
            }
        }
        int mism2 = runThreads(w, iters, true);
        System.out.println("phase2 threads=" + THREADS + " mismatches=" + mism2 + " walk=" + w.walk("x", 1, true));
        System.out.println("bootstraps=" + bootstraps);

        // Bench: the two-site `both` (fits the thread table) and the 80-site walk.
        long[] bt = new long[5];
        long[] wt = new long[5];
        int reps = iters * 10;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            for (int n = 0; n < reps; n++) {
                sink = w.both("z", n & 255, true);
            }
            bt[rep] = System.nanoTime() - t0;
            t0 = System.nanoTime();
            for (int n = 0; n < iters; n++) {
                sink = w.walk("z", n & 255, true);
            }
            wt[rep] = System.nanoTime() - t0;
        }
        sink = null;
        Arrays.sort(bt);
        Arrays.sort(wt);
        System.out.println("TIME both med " + (bt[2] / (reps * 2L)) + " ns/site");
        System.out.println("TIME walk med " + (wt[2] / ((long) iters * SITES)) + " ns/site");
    }

    // ---------------------------------------------------------------------
    // Class assembly
    // ---------------------------------------------------------------------

    static byte[] genBytes() {
        ClassDesc self = ClassDesc.of("L2bW133GenericIndyThreadRow");
        ClassDesc walker = ClassDesc.of("L2bW133GenericIndyThreadRow$Walker");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        MethodTypeDesc siteType = MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object,
                ConstantDescs.CD_int, ConstantDescs.CD_boolean);
        MethodTypeDesc walkType = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object,
                ConstantDescs.CD_int, ConstantDescs.CD_boolean);
        MethodTypeDesc bothType = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_Object,
                ConstantDescs.CD_int, ConstantDescs.CD_boolean);
        MethodTypeDesc lengthType = MethodTypeDesc.of(ConstantDescs.CD_int);
        MethodTypeDesc concatType = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_String);
        return ClassFile.of().build(ClassDesc.of("GenL2bW133Row"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(ConstantDescs.CD_Object);
            cb.withInterfaceSymbols(walker);
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, code -> code
                    .aload(0)
                    .invokespecial(ConstantDescs.CD_Object, "<init>", ConstantDescs.MTD_void)
                    .return_());
            // int walk(Object o, int i, boolean b): the sum of the lengths of
            // SITES distinct sites' answers.
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
            // String both(Object o, int i, boolean b): two instructions over
            // ONE invokedynamic entry ("pair"), joined by "|".
            DynamicCallSiteDesc pair = DynamicCallSiteDesc.of(bsm, "pair", siteType);
            cb.withMethodBody("both", bothType, ClassFile.ACC_PUBLIC, code -> code
                    .aload(1).iload(2).iload(3).invokedynamic(pair)
                    .checkcast(ConstantDescs.CD_String)
                    .ldc("|")
                    .invokevirtual(ConstantDescs.CD_String, "concat", concatType)
                    .aload(1).iload(2).iload(3).invokedynamic(pair)
                    .checkcast(ConstantDescs.CD_String)
                    .invokevirtual(ConstantDescs.CD_String, "concat", concatType)
                    .areturn());
        });
    }
}
