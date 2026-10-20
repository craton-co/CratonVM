// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29 (orchestrator): an `invokespecial` from a class
// WITHOUT `ACC_SUPER` selects from the caller's superclass, as from any other
// class. "In Java SE 8 and above, the Java Virtual Machine considers the
// ACC_SUPER flag to be set in every class file" (JVMS 4.1); HotSpot 25 does
// (docs/internal/fixed-bugs/interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md,
// item 2).
//
// `Caller extends NsB extends NsA`; `NsB` overrides `m`. `Caller` is
// generated with the java.lang.classfile API, once without `ACC_SUPER` and
// once with it, and runs `invokespecial NsA.m()` (the constant pool names the
// GRANDPARENT). Selection starts at `Caller`'s direct superclass, so both
// run `NsB.m`. Before wave 29 CratonVM honoured the missing flag and ran
// `NsA.m` for the first row.
//
// Run: javac -d out L4W29NoAccSuper.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W29NoAccSuper
//
// Expected HotSpot 25 output (default and -Xint):
//   no ACC_SUPER: B
//   ACC_SUPER: B
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;

public class L4W29NoAccSuper {
    public static class NsA {
        public String m() {
            return "A";
        }
    }

    public static class NsB extends NsA {
        @Override
        public String m() {
            return "B";
        }
    }

    static byte[] caller(String name, int flags) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc nsa = ClassDesc.of(NsA.class.getName());
        ClassDesc nsb = ClassDesc.of(NsB.class.getName());
        MethodTypeDesc str = MethodTypeDesc.of(ConstantDescs.CD_String);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(flags);
            cb.withSuperclass(nsb);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(nsb, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).return_());
            cb.withMethodBody("call", str, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(nsa, "m", str).areturn());
        });
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W29NoAccSuper.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String run(String name, int flags) {
        try {
            Class<?> c = new Loader().define(name, caller(name, flags));
            Object o = c.getConstructor().newInstance();
            return (String) c.getMethod("call").invoke(o);
        } catch (Throwable t) {
            Throwable e = t instanceof java.lang.reflect.InvocationTargetException ? t.getCause() : t;
            return e.getClass().getName();
        }
    }

    public static void main(String[] args) {
        System.out.println("no ACC_SUPER: " + run("NoSuperCaller", ClassFile.ACC_PUBLIC));
        System.out.println("ACC_SUPER: " + run("SuperCaller", ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER));
    }
}
