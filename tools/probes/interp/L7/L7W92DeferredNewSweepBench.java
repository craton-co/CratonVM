// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 92, lane L7: the class-manager write guard's
// release sweep (`ClassManagerWriteGuard::drop` ->
// `resweep_deferred_new_after_class_definition`) while a deferred-`new`
// retry is HELD. A hot method (`hot`) has a `new` of a class nothing ever
// loads (`L7W92NeverLoaded`) in a branch that never runs, so its optimizing
// compile bails on the deferred site and its retry stays armed for the life
// of the run. From then on, every write-guard release clones the held list
// and re-resolves the method's sites by name -- one define + link + init
// takes five or more releases. Row `define-init`: a fresh tiny class per op,
// `defineClass` + `Class.forName(name, true, loader)`.
//
// The A/B is `CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP` (default off): with
// it on, a release re-sweeps only when a definition, a resolution-epoch
// bump, a link / init completion or a new retry moved since the last
// release sweep (most releases of a define settle nothing). The argument
// `nohold` skips the warm-up of `hot`, so nothing is held: the floor both
// arms should then match.
//
// Census (count and total of the sweeps):
//   CRATONVM_DBG_DEFINE_CENSUS=1 cratonvm --jdk-only -cp out L7W92DeferredNewSweepBench 2>census.txt
//   grep write-guard-sweep census.txt
// Confirm the retry is held: CRATONVM_DBG_JITC=1 prints
// "deferred-new ARMED L7W92DeferredNewSweepBench.hot(I)Ljava/lang/Object;".
//
// Run (JIT on: the retry exists only when the optimizing tier runs):
//   javac -d out L7W92DeferredNewSweepBench.java
//   java -cp out L7W92DeferredNewSweepBench                                   (HotSpot reference)
//   cratonvm --jdk-only -cp out L7W92DeferredNewSweepBench
//   CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP=1 cratonvm --jdk-only -cp out L7W92DeferredNewSweepBench
//   ... and each with --compatible, and with the argument `nohold`.
// Interleave the two arms, several rounds, medians (in-JVM timings swing).
//
// Output: "checksum <row> <n>" on stdout (deterministic); the timing line
// "row <row> min <ns> med <ns> ns/op" on stderr.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, no argument)
// hot checksum 1999999000000
// checksum define-init 300
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.Arrays;

public class L7W92DeferredNewSweepBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final int N = 300;
    static final int WARM = 60;
    static int serial;

    /** Never loaded: only `hot`'s dead branch names it. */
    static final class L7W92NeverLoaded {
        final int v;

        L7W92NeverLoaded(int v) {
            this.v = v;
        }
    }

    /** A box `hot` allocates on its live path, so the method has a `new`. */
    static final class Box {
        final long v;

        Box(long v) {
            this.v = v;
        }
    }

    // `i == -1` never holds in this program. Returns Object so the verifier
    // needs no assignability check that would load the dead branch's class.
    static Object hot(int i) {
        if (i == -1) {
            return new L7W92NeverLoaded(i);
        }
        return new Box(i);
    }

    /** Defines what it is given; `forName` then links and initializes it. */
    static final class Definer extends ClassLoader {
        Definer() {
            super(L7W92DeferredNewSweepBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    /** One define + link + init of a fresh class; 1 when it came back initialized. */
    static int defineInit(Definer loader) throws Exception {
        String name = "L7W92Swept" + (serial++);
        byte[] b = build(name);
        loader.define(name, b);
        Class<?> c = Class.forName(name, true, loader);
        return c.getName().equals(name) ? 1 : 0;
    }

    public static void main(String[] args) throws Exception {
        boolean hold = !(args.length > 0 && args[0].equals("nohold"));
        long sum = 0;
        if (hold) {
            for (int i = 0; i < 2_000_000; i++) {
                sum += ((Box) hot(i)).v;
            }
        } else {
            sum = 1_999_999_000_000L;
        }
        System.out.println("hot checksum " + sum);
        Definer loader = new Definer();
        for (int i = 0; i < WARM; i++) {
            defineInit(loader);
        }
        long[] ns = new long[N];
        int checksum = 0;
        for (int i = 0; i < N; i++) {
            long t0 = System.nanoTime();
            checksum += defineInit(loader);
            ns[i] = System.nanoTime() - t0;
        }
        Arrays.sort(ns);
        System.out.println("checksum define-init " + checksum);
        System.err.println("row define-init min " + ns[0] + " med " + ns[N / 2] + " ns/op");
    }
}
