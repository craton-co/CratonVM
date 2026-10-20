// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: INDY21-1 with VIRTUAL leaves
// (`CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC`; `jit_indy_guarded_leaf` admits a
// virtual leaf only with a non-null receiver and dispatches it with
// `jit_invoke_dispatch_body(leaf)`; `jit_indy_mutable_second_target` takes a
// second cell only of the planned leaf's kind).
//
// Risk targeted: a MutableCallSite of type (Base,int)int whose targets are
// `findVirtual(Base, "m")`, `findVirtual(Base, "n")`, `findVirtual(Iface, "k")`
// (an interface method), `findStatic(host, "s")` (another kind: the second
// arm must not take it), and a bound handle (`insertArguments`, not a direct
// leaf). Receivers are Base, Sub (overrides m and n), Sub2 (overrides only
// n), and null. The second arm must dispatch VIRTUALLY on the receiver
// (Sub.m for a Sub), must raise the NPE of a virtual call on null (and the
// static target must accept null), and the bound target must reach the
// owner bridge. Flips inside the loop (every 37 elements) and between
// batches. Every batch prints its sum and the direct-call reference.
//
// Run:
//   javac -d out R21FzIndyBimorph2.java
//   java -Xmx512m -cp out R21FzIndyBimorph2                               # HotSpot 25
//   cratonvm -Xmx512m -cp out R21FzIndyBimorph2
//   CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC=1 cratonvm -Xmx512m -cp out R21FzIndyBimorph2
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

public class R21FzIndyBimorph2 {
    public interface Iface {
        int k(int x);
    }

    public static class Base implements Iface {
        public int m(int x) {
            return x + 100;
        }

        public int n(int x) {
            return x * 3;
        }

        public int k(int x) {
            return x - 1;
        }
    }

    public static class Sub extends Base {
        @Override
        public int m(int x) {
            return x + 200;
        }

        @Override
        public int n(int x) {
            return x * 5;
        }
    }

    public static final class Sub2 extends Base {
        @Override
        public int n(int x) {
            return -x;
        }

        @Override
        public int k(int x) {
            return x + 9;
        }
    }

    public static int s(Base b, int x) {
        return (b == null ? 7 : 11) + x;
    }

    public static int bound(int extra, Base b, int x) {
        return extra + (b == null ? 0 : 1) + x;
    }

    static final MethodType BI = MethodType.methodType(int.class, Base.class, int.class);
    static MethodHandle M, N, K, S, BOUND;

    static int direct(MethodHandle t, Base b, int x) {
        if (t == M) {
            return b.m(x);
        }
        if (t == N) {
            return b.n(x);
        }
        if (t == K) {
            return b.k(x);
        }
        if (t == S) {
            return s(b, x);
        }
        return bound(1000, b, x);
    }

    static CallSite SITE;

    public static CallSite bsm(MethodHandles.Lookup l, String name, MethodType type) {
        return SITE;
    }

    static CallSite cur;
    static MethodHandle[] rot;
    static int rotIdx;

    public static void flip() {
        rotIdx = (rotIdx + 1) % rot.length;
        cur.setTarget(rot[rotIdx]);
    }

    static MethodHandle loop;

    static void gen(CallSite site) throws Throwable {
        SITE = site;
        ClassDesc g = ClassDesc.of("R21FzIndyBiVGen");
        ClassDesc host = ClassDesc.of("R21FzIndyBimorph2");
        ClassDesc base = ClassDesc.of("R21FzIndyBimorph2$Base");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, host, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        MethodTypeDesc bi = MethodTypeDesc.of(ConstantDescs.CD_int, base, ConstantDescs.CD_int);
        // loop(Base[] rs, int n): sum of indy(rs[i % len], i), flip() before i where i % 37 == 36
        MethodTypeDesc loopT = MethodTypeDesc.of(ConstantDescs.CD_long, base.arrayType(), ConstantDescs.CD_int);
        DynamicCallSiteDesc indy = DynamicCallSiteDesc.of(bsm, "vop", bi);
        int st = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        byte[] bytes = ClassFile.of().build(g, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withMethodBody("loop", loopT, st, code -> {
                Label top = code.newLabel();
                Label end = code.newLabel();
                Label skip = code.newLabel();
                code.lconst_0().lstore(2).iconst_0().istore(4);
                code.labelBinding(top);
                code.iload(4).iload(1).if_icmpge(end);
                code.iload(4).bipush(37).irem().bipush(36).if_icmpne(skip);
                code.invokestatic(host, "flip", ConstantDescs.MTD_void);
                code.labelBinding(skip);
                code.lload(2);
                code.aload(0).iload(4).aload(0).arraylength().irem().aaload();
                code.iload(4).invokedynamic(indy).i2l().ladd().lstore(2);
                code.iinc(4, 1).goto_(top);
                code.labelBinding(end);
                code.lload(2).lreturn();
            });
        });
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> c = l.defineClass(bytes);
        loop = l.findStatic(c, "loop", MethodType.methodType(long.class, Base[].class, int.class));
    }

    static long mismatches;

    static String run(MethodHandle[] r, Base[] rs, int n) throws Throwable {
        cur = SITE;
        rot = r;
        int idx0 = rotIdx % r.length;
        rotIdx = idx0;
        SITE.setTarget(r[idx0]);
        String want;
        try {
            long s = 0;
            int idx = idx0;
            for (int i = 0; i < n; i++) {
                if (i % 37 == 36) {
                    idx = (idx + 1) % r.length;
                }
                s += direct(r[idx], rs[i % rs.length], i);
            }
            want = Long.toString(s);
        } catch (NullPointerException e) {
            want = "NPE";
        }
        String got;
        try {
            got = Long.toString((long) loop.invokeExact(rs, n));
        } catch (NullPointerException e) {
            got = "NPE";
        }
        if (!got.equals(want)) {
            mismatches++;
        }
        return got + (got.equals(want) ? "" : "!=" + want) + " rot=" + rotIdx;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        M = l.findVirtual(Base.class, "m", MethodType.methodType(int.class, int.class));
        N = l.findVirtual(Base.class, "n", MethodType.methodType(int.class, int.class));
        K = l.findVirtual(Iface.class, "k", MethodType.methodType(int.class, int.class)).asType(BI);
        S = l.findStatic(R21FzIndyBimorph2.class, "s", BI);
        BOUND = MethodHandles.insertArguments(
                l.findStatic(R21FzIndyBimorph2.class, "bound",
                        MethodType.methodType(int.class, int.class, Base.class, int.class)), 0, 1000);
        MutableCallSite site = new MutableCallSite(M);
        gen(site);
        Base[] mono = {new Base()};
        Base[] poly = {new Base(), new Sub(), new Sub2(), new Sub()};
        Base[] withNull = {new Base(), new Sub(), null};
        MethodHandle[] mn = {M, N};
        for (int rep = 0; rep < 30; rep++) {
            run(new MethodHandle[] {(rep & 1) == 0 ? M : N}, mono, 30_000);
            run(new MethodHandle[] {(rep & 1) == 0 ? M : N}, poly, 30_000);
        }
        System.out.println("warm mismatches " + mismatches);
        for (int rep = 0; rep < 3; rep++) {
            System.out.println("row r" + rep + " mn-mono " + run(mn, mono, 20_000));
            System.out.println("row r" + rep + " mn-poly " + run(mn, poly, 20_000));
            System.out.println("row r" + rep + " mnk-poly " + run(new MethodHandle[] {M, N, K}, poly, 20_000));
            System.out.println("row r" + rep + " ms-poly " + run(new MethodHandle[] {M, S}, poly, 20_000));
            System.out.println("row r" + rep + " mbound-poly " + run(new MethodHandle[] {M, BOUND}, poly, 20_000));
            System.out.println("row r" + rep + " s-null " + run(new MethodHandle[] {S}, withNull, 3_000));
            System.out.println("row r" + rep + " n-null " + run(new MethodHandle[] {N}, withNull, 3_000));
            System.out.println("row r" + rep + " mn-null " + run(mn, withNull, 3_000));
            System.out.println("row r" + rep + " k-null " + run(new MethodHandle[] {K, M}, withNull, 3_000));
        }
        System.out.println("mismatches " + mismatches);
    }
}
