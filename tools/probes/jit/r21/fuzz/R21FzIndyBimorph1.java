// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: INDY21-1, the bimorphic MutableCallSite
// guard (`CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC`: the second arm is an
// `Op::IndyBridge` of the second target's `IndyMutableGuardCell`, dispatched
// by `jit_indy_guarded_leaf`), and the retarget recompile
// (`CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`).
//
// Risk targeted: `t == A ? callA : t == B ? bridge(cellB) : bridge(site)`.
//   * a THIRD target after the two planned ones (must reach the owner
//     bridge, not cell B's leaf);
//   * a flip INSIDE the running loop (each call re-reads the target), at a
//     fixed iteration, through A -> B -> C -> A rotations;
//   * a target that throws at one argument (the exception must leave
//     through the second arm with the right type and message);
//   * a target whose arity matches but whose behaviour differs only by a
//     constant (a wrong-arm call shows as a wrong sum);
//   * a VolatileCallSite twin, and a flip done by ANOTHER thread between
//     batches (joined, so deterministic);
//   * a flip storm (every 7 calls).
// Every batch prints its sum next to the sum computed by direct calls; the
// mismatch count must be 0.
//
// The call sites are generated with java.lang.classfile (JDK 24+), one class
// per site.
//
// Run:
//   javac -d out R21FzIndyBimorph1.java
//   java -Xmx512m -cp out R21FzIndyBimorph1                               # HotSpot 25
//   cratonvm -Xmx512m -cp out R21FzIndyBimorph1
//   CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC=1 cratonvm -Xmx512m -cp out R21FzIndyBimorph1
//   CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE=1 cratonvm ...; both switches together
//   CRATONVM_JIT_INDY_MUTABLE_GUARD=0 cratonvm ... (control: all bridge)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
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
import java.lang.invoke.VolatileCallSite;

public class R21FzIndyBimorph1 {
    static final MethodType II = MethodType.methodType(int.class, int.class);
    static final CallSite[] SITES = new CallSite[8];
    static MethodHandle A, B, C, T;

    public static int tA(int x) {
        return x + 1;
    }

    public static int tB(int x) {
        return x * 2;
    }

    public static int tC(int x) {
        return x - 7;
    }

    public static int tT(int x) {
        if (x == 13) {
            throw new IllegalStateException("tT saw " + x);
        }
        return x ^ 5;
    }

    static int direct(MethodHandle t, int x) {
        if (t == A) {
            return tA(x);
        }
        if (t == B) {
            return tB(x);
        }
        if (t == C) {
            return tC(x);
        }
        return tT(x);
    }

    public static CallSite bsm(MethodHandles.Lookup l, String name, MethodType type) {
        return SITES[Integer.parseInt(name.substring(2))];
    }

    // In-loop flips: flip() rotates CUR through ROT.
    static CallSite cur;
    static MethodHandle[] rot;
    static int rotIdx;

    public static void flip() {
        rotIdx = (rotIdx + 1) % rot.length;
        cur.setTarget(rot[rotIdx]);
    }

    static final class Gen {
        final MethodHandle call;
        final MethodHandle loop;

        Gen(MethodHandle call, MethodHandle loop) {
            this.call = call;
            this.loop = loop;
        }
    }

    static Gen gen(int k, CallSite site) throws Throwable {
        SITES[k] = site;
        ClassDesc g = ClassDesc.of("R21FzIndyBiGen" + k);
        ClassDesc host = ClassDesc.of("R21FzIndyBimorph1");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, host, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        MethodTypeDesc ii = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
        MethodTypeDesc jii = MethodTypeDesc.of(ConstantDescs.CD_long, ConstantDescs.CD_int, ConstantDescs.CD_int);
        DynamicCallSiteDesc indy = DynamicCallSiteDesc.of(bsm, "op" + k, ii);
        int st = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        byte[] bytes = ClassFile.of().build(g, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withMethodBody("call", ii, st, code -> code.iload(0).invokedynamic(indy).ireturn());
            // loop(n, flipAt): sum of indy(i), calling flip() before the call at i == flipAt (mod 64)
            cb.withMethodBody("loop", jii, st, code -> {
                Label top = code.newLabel();
                Label end = code.newLabel();
                Label skip = code.newLabel();
                code.lconst_0().lstore(2).iconst_0().istore(4);
                code.labelBinding(top);
                code.iload(4).iload(0).if_icmpge(end);
                code.iload(4).bipush(63).iand().iload(1).if_icmpne(skip);
                code.invokestatic(host, "flip", ConstantDescs.MTD_void);
                code.labelBinding(skip);
                code.lload(2).iload(4).invokedynamic(indy).i2l().ladd().lstore(2);
                code.iinc(4, 1).goto_(top);
                code.labelBinding(end);
                code.lload(2).lreturn();
            });
        });
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> c = l.defineClass(bytes);
        return new Gen(l.findStatic(c, "call", II),
                l.findStatic(c, "loop", MethodType.methodType(long.class, int.class, int.class)));
    }

    static long mismatches;

    /** The reference sum for loop(n, flipAt) starting from rot[rotIdx0]. */
    static long expectLoop(MethodHandle[] r, int idx0, int n, int flipAt) {
        long s = 0;
        int idx = idx0;
        for (int i = 0; i < n; i++) {
            if ((i & 63) == flipAt) {
                idx = (idx + 1) % r.length;
            }
            s += direct(r[idx], i);
        }
        return s;
    }

    static String runLoop(Gen g, CallSite site, MethodHandle[] r, int n, int flipAt) throws Throwable {
        cur = site;
        rot = r;
        int idx0 = rotIdx % r.length;
        rotIdx = idx0;
        site.setTarget(r[idx0]);
        String want;
        try {
            want = Long.toString(expectLoop(r, idx0, n, flipAt));
        } catch (IllegalStateException e) {
            want = "ISE(" + e.getMessage() + ")";
        }
        String got;
        try {
            got = Long.toString((long) g.loop.invokeExact(n, flipAt));
        } catch (IllegalStateException e) {
            got = "ISE(" + e.getMessage() + ")";
        }
        if (!got.equals(want)) {
            mismatches++;
        }
        return got + (got.equals(want) ? "" : "!=" + want) + " rot=" + rotIdx;
    }

    static String runCalls(Gen g, CallSite site, MethodHandle[] seq, int every, int n) throws Throwable {
        long s = 0;
        long want = 0;
        int idx = 0;
        site.setTarget(seq[0]);
        for (int i = 0; i < n; i++) {
            if (every > 0 && i % every == 0) {
                idx = (idx + 1) % seq.length;
                site.setTarget(seq[idx]);
            }
            int x = i & 1023;
            if (x == 13 && seq[idx] == T) {
                x = 14;
            }
            s += (int) g.call.invokeExact(x);
            want += direct(seq[idx], x);
        }
        if (s != want) {
            mismatches++;
        }
        return s + (s == want ? "" : "!=" + want);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        A = l.findStatic(R21FzIndyBimorph1.class, "tA", II);
        B = l.findStatic(R21FzIndyBimorph1.class, "tB", II);
        C = l.findStatic(R21FzIndyBimorph1.class, "tC", II);
        T = l.findStatic(R21FzIndyBimorph1.class, "tT", II);
        MutableCallSite m0 = new MutableCallSite(A);
        MutableCallSite m1 = new MutableCallSite(A);
        VolatileCallSite v2 = new VolatileCallSite(A);
        MutableCallSite m3 = new MutableCallSite(A);
        Gen g0 = gen(0, m0);
        Gen g1 = gen(1, m1);
        Gen g2 = gen(2, v2);
        Gen g3 = gen(3, m3);
        MethodHandle[] ab = {A, B};
        MethodHandle[] abc = {A, B, C};
        MethodHandle[] at = {A, T};
        // Warm both shapes on the two planned targets, then rows.
        for (int rep = 0; rep < 40; rep++) {
            runLoop(g0, m0, ab, 20_000, 64);           // flipAt 64 never matches: stable within a batch
            rotIdx++;
            runCalls(g1, m1, ab, 5_000, 20_000);
            runLoop(g2, v2, ab, 20_000, 64);
            rotIdx++;
        }
        System.out.println("warm mismatches " + mismatches);
        for (int rep = 0; rep < 3; rep++) {
            System.out.println("row r" + rep + " loop-ab-inloop " + runLoop(g0, m0, ab, 50_000, 17));
            System.out.println("row r" + rep + " loop-abc-inloop " + runLoop(g0, m0, abc, 50_000, 5));
            System.out.println("row r" + rep + " loop-third " + runLoop(g0, m0, new MethodHandle[] {C}, 30_000, 64));
            System.out.println("row r" + rep + " calls-ab " + runCalls(g1, m1, ab, 1000, 20_000));
            System.out.println("row r" + rep + " calls-abc " + runCalls(g1, m1, abc, 1000, 20_000));
            System.out.println("row r" + rep + " calls-storm " + runCalls(g1, m1, abc, 7, 20_000));
            System.out.println("row r" + rep + " vcs-inloop " + runLoop(g2, v2, abc, 50_000, 40));
            System.out.println("row r" + rep + " throw-inloop " + runLoop(g3, m3, at, 200, 3));
            System.out.println("row r" + rep + " throw-calls " + runCalls(g3, m3, at, 3, 3000));
        }
        // A flip by another thread between batches.
        for (int rep = 0; rep < 6; rep++) {
            final MethodHandle next = (rep & 1) == 0 ? C : B;
            Thread t = new Thread(() -> m1.setTarget(next));
            t.start();
            t.join();
            long s = 0;
            long want = 0;
            for (int i = 0; i < 10_000; i++) {
                s += (int) g1.call.invokeExact(i);
                want += direct(next, i);
            }
            if (s != want) {
                mismatches++;
            }
            System.out.println("row thread" + rep + " " + s + (s == want ? "" : "!=" + want));
        }
        // An exception through the second arm when T is the second planned target.
        m3.setTarget(T);
        try {
            int x = (int) g3.call.invokeExact(13);
            System.out.println("row throw-direct no throw " + x);
        } catch (IllegalStateException e) {
            System.out.println("row throw-direct ISE(" + e.getMessage() + ")");
        }
        System.out.println("mismatches " + mismatches);
    }
}
