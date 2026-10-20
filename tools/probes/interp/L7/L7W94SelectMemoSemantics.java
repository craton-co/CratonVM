// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L7
// (i92-L7-selection-and-callback-memos-need-a-hierarchy-edge-epoch): JVMS
// 5.4.6 selection stays right across class definitions when the selection
// memo survives them (`CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH=1`; default
// off). Each answer below is asked before, between and after definitions:
//
//   inherit    an interface call on a receiver inheriting `f` from Base -> 1
//   override   a class defined later (child loader) overriding `f` -> 2,
//              while the earlier receiver keeps answering 1
//   runtime-pkg  a child-loader class Q, same package NAME as P but another
//              defining loader, declaring package-private `g()`: it does NOT
//              override P.g (JVMS 5.4.5 compares run-time packages), so
//              `((P) q).g()` runs P.g -> 10 every time
//
// Run in both modes, switch off and on, --nojit and with the JIT:
//   javac -d out L7W94SelectMemoSemantics.java
//   java -cp out L7W94SelectMemoSemantics
//   cratonvm --jdk-only --nojit -cp out L7W94SelectMemoSemantics
//   CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH=1 cratonvm --jdk-only --nojit -cp out L7W94SelectMemoSemantics
//   (and --compatible)
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// inherit 1
// override 2
// inherit-after 1
// runtime-pkg 10
// rounds 200 inherit 200 override 400 runtime-pkg 2000
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;

public class L7W94SelectMemoSemantics {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static int serial;

    public interface I {
        int f();
    }

    public static class Base implements I {
        public Base() {}

        public int f() {
            return 1;
        }
    }

    public static class S1 extends Base {
        public S1() {}
    }

    public static class P {
        public P() {}

        int g() {
            return 10;
        }
    }

    static final class Definer extends ClassLoader {
        Definer() {
            super(L7W94SelectMemoSemantics.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    // `public class <name> extends <superName> { public <init>; <method> }`,
    // the method returning `value`.
    static byte[] subclass(String name, Class<?> superclass, String method, int flags, int value) {
        ClassDesc sup = ClassDesc.of(superclass.getName());
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(sup);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(sup, ConstantDescs.INIT_NAME, VOID)
                            .return_());
            cb.withMethodBody(method, INT, flags,
                    code -> code.bipush(value).ireturn());
        });
    }

    static byte[] plain(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    static int callI(I x) {
        return x.f();
    }

    static int callP(P p) {
        return p.g();
    }

    static void defineUnrelated(Definer loader) {
        String name = "L7W94SelUnrelated" + (serial++);
        loader.define(name, plain(name));
    }

    public static void main(String[] args) throws Exception {
        Definer loader = new Definer();
        I s1 = new S1();
        System.out.println("inherit " + callI(s1));
        defineUnrelated(loader);
        Class<?> sub2 = loader.define("L7W94SelSub2", subclass("L7W94SelSub2", S1.class, "f",
                ClassFile.ACC_PUBLIC, 2));
        I o = (I) sub2.getConstructor().newInstance();
        System.out.println("override " + callI(o));
        System.out.println("inherit-after " + callI(s1));
        Definer other = new Definer();
        Class<?> q = other.define("L7W94SelQ", subclass("L7W94SelQ", P.class, "g", 0, 20));
        P qp = (P) q.getConstructor().newInstance();
        System.out.println("runtime-pkg " + callP(qp));
        int inherit = 0;
        int override = 0;
        int runtimePkg = 0;
        for (int round = 0; round < 200; round++) {
            inherit += callI(s1);
            override += callI(o);
            defineUnrelated(loader);
            runtimePkg += callP(qp);
        }
        System.out.println("rounds 200 inherit " + inherit + " override " + override
                + " runtime-pkg " + runtimePkg);
    }
}
