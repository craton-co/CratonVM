// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 35 (orchestrator): the compiled half of item 1 of
// docs/internal/fixed-bugs/interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md.
// `L4W32SpecialSelection`'s rows, each `invokespecial` site called 30 000
// times so that its caller is compiled: the answer must not change when the
// site runs in compiled code.
//
//   abstract-super  `invokespecial SaB.m` where SaB re-declares `m` abstract
//                   over SaA.m -> AbstractMethodError every time
//   static-between  `invokespecial SsA.m` from below a static SsB.m -> "A"
//                   every time
//
// Run: javac -d out L4W35SpecialSelectionHot.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W35SpecialSelectionHot
//
// Expected HotSpot 25 output (default and -Xint):
//   abstract-super: AbstractMethodError=30000 A=0 other=0
//   static-between: AbstractMethodError=0 A=30000 other=0
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Consumer;

public class L4W35SpecialSelectionHot {
    public static class SaA {
        public String m() {
            return "A";
        }
    }

    public abstract static class SaB extends SaA {
        @Override
        public abstract String m();
    }

    public static class SsA {
        public String m() {
            return "A";
        }
    }

    static final MethodTypeDesc STR = MethodTypeDesc.of(ConstantDescs.CD_String);

    static byte[] build(String name, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            body.accept(cb);
        });
    }

    static void ctor(ClassBuilder cb, ClassDesc sup) {
        cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                code -> code.aload(0).invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).return_());
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W35SpecialSelectionHot.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String hot(String label, Class<?> c) throws Throwable {
        Object o = c.getConstructor().newInstance();
        MethodHandle call = MethodHandles.lookup().findVirtual(c, "call", MethodType.methodType(String.class))
                .asType(MethodType.methodType(String.class, Object.class));
        int ame = 0;
        int a = 0;
        int other = 0;
        for (int i = 0; i < 30_000; i++) {
            try {
                String r = (String) call.invokeExact(o);
                if ("A".equals(r)) {
                    a++;
                } else {
                    other++;
                }
            } catch (AbstractMethodError e) {
                ame++;
            }
        }
        return label + ": AbstractMethodError=" + ame + " A=" + a + " other=" + other;
    }

    public static void main(String[] args) throws Throwable {
        ClassDesc sab = ClassDesc.of(SaB.class.getName());
        Class<?> sa = LOADER.define("HotSaCaller", build("HotSaCaller", cb -> {
            cb.withSuperclass(sab);
            ctor(cb, sab);
            cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC, code -> code.ldc("C").areturn());
            cb.withMethodBody("call", STR, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(sab, "m", STR).areturn());
        }));
        System.out.println(hot("abstract-super", sa));

        ClassDesc ssa = ClassDesc.of(SsA.class.getName());
        LOADER.define("HotSsB", build("HotSsB", cb -> {
            cb.withSuperclass(ssa);
            ctor(cb, ssa);
            cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.ldc("B").areturn());
        }));
        ClassDesc ssb = ClassDesc.of("HotSsB");
        Class<?> ss = LOADER.define("HotSsCaller", build("HotSsCaller", cb -> {
            cb.withSuperclass(ssb);
            ctor(cb, ssb);
            cb.withMethodBody("call", STR, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(ssa, "m", STR).areturn());
        }));
        System.out.println(hot("static-between", ss));
    }
}
