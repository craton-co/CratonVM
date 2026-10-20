// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: ONE MutableCallSite linked by SEVERAL
// invokedynamic instructions (`CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC`,
// `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`; per-instruction
// `IndyDirectState` -- `cells`, `mutable_cell`, `planned_cell`,
// `retarget_armed` -- against a per-`CallSite` target).
//
// Risk targeted: the guard state (cells, the planned cell, the retarget
// arming, the recent-target memos) is kept PER INSTRUCTION, while
// `setTarget` changes the target of every instruction linked to the call
// site at once. Three generated classes link the same MutableCallSite; each
// is warmed on a different subset of targets (so their planned / second cells
// differ), then the site is flipped through all targets and every
// instruction must call the CURRENT target. A fourth instruction links a
// second MutableCallSite whose target is a `dynamicInvoker()` of the first
// (a target that is itself a call-site invoker, not a direct leaf).
//
// Run:
//   javac -d out R21FzIndyShared1.java
//   java -Xmx512m -cp out R21FzIndyShared1                                # HotSpot 25
//   cratonvm -Xmx512m -cp out R21FzIndyShared1
//   CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC=1 cratonvm -Xmx512m -cp out R21FzIndyShared1
//   CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE=1 cratonvm ...; both together
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

public class R21FzIndyShared1 {
    static final MethodType II = MethodType.methodType(int.class, int.class);
    static final CallSite[] BY_NAME = new CallSite[2];
    static MethodHandle[] T = new MethodHandle[4];

    public static int t0(int x) {
        return x + 3;
    }

    public static int t1(int x) {
        return x * 7;
    }

    public static int t2(int x) {
        return x ^ 0x55;
    }

    public static int t3(int x) {
        return -x;
    }

    static int direct(int which, int x) {
        switch (which) {
            case 0: return t0(x);
            case 1: return t1(x);
            case 2: return t2(x);
            default: return t3(x);
        }
    }

    public static CallSite bsm(MethodHandles.Lookup l, String name, MethodType type) {
        return BY_NAME[name.equals("shared") ? 0 : 1];
    }

    static MethodHandle gen(int k, String siteName) throws Throwable {
        ClassDesc g = ClassDesc.of("R21FzIndyShGen" + k);
        ClassDesc host = ClassDesc.of("R21FzIndyShared1");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, host, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        MethodTypeDesc ii = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
        MethodTypeDesc ji = MethodTypeDesc.of(ConstantDescs.CD_long, ConstantDescs.CD_int);
        DynamicCallSiteDesc indy = DynamicCallSiteDesc.of(bsm, siteName, ii);
        byte[] bytes = ClassFile.of().build(g, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withMethodBody("loop", ji, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label top = code.newLabel();
                Label end = code.newLabel();
                code.lconst_0().lstore(1).iconst_0().istore(3);
                code.labelBinding(top);
                code.iload(3).iload(0).if_icmpge(end);
                code.lload(1).iload(3).invokedynamic(indy).i2l().ladd().lstore(1);
                code.iinc(3, 1).goto_(top);
                code.labelBinding(end);
                code.lload(1).lreturn();
            });
        });
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> c = l.defineClass(bytes);
        return l.findStatic(c, "loop", MethodType.methodType(long.class, int.class));
    }

    static long expect(int which, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += direct(which, i);
        }
        return s;
    }

    static long mismatches;

    static String run(MethodHandle loop, int which, int n) throws Throwable {
        long got = (long) loop.invokeExact(n);
        long want = expect(which, n);
        if (got != want) {
            mismatches++;
        }
        return got + (got == want ? "" : "!=" + want);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        for (int i = 0; i < 4; i++) {
            T[i] = l.findStatic(R21FzIndyShared1.class, "t" + i, II);
        }
        MutableCallSite shared = new MutableCallSite(T[0]);
        MutableCallSite outer = new MutableCallSite(shared.dynamicInvoker());
        BY_NAME[0] = shared;
        BY_NAME[1] = outer;
        MethodHandle a = gen(0, "shared");
        MethodHandle b = gen(1, "shared");
        MethodHandle c = gen(2, "shared");
        MethodHandle d = gen(3, "outer");
        // Warm each instruction on its own subset of targets.
        int[][] subsets = {{0, 1}, {1, 2}, {2, 3}, {0, 3}};
        MethodHandle[] loops = {a, b, c, d};
        for (int rep = 0; rep < 24; rep++) {
            for (int k = 0; k < 4; k++) {
                int which = subsets[k][rep & 1];
                shared.setTarget(T[which]);
                run(loops[k], which, 20_000);
            }
        }
        System.out.println("warm mismatches " + mismatches);
        for (int rep = 0; rep < 3; rep++) {
            for (int which = 0; which < 4; which++) {
                shared.setTarget(T[which]);
                StringBuilder sb = new StringBuilder("row r" + rep + " t" + which);
                for (int k = 0; k < 4; k++) {
                    sb.append(' ').append(run(loops[k], which, 30_000));
                }
                System.out.println(sb);
            }
        }
        // The outer site retargeted away from the invoker and back.
        outer.setTarget(T[2]);
        shared.setTarget(T[3]);
        System.out.println("row outer-direct " + run(d, 2, 10_000) + " shared " + run(a, 3, 10_000));
        outer.setTarget(shared.dynamicInvoker());
        System.out.println("row outer-back " + run(d, 3, 10_000));
        System.out.println("mismatches " + mismatches);
    }
}
