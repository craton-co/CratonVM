// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 35 (orchestrator): item 6 of
// docs/internal/fixed-bugs/interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md.
// `final C extends abstract B extends A`: A declares a concrete `m`, B
// re-declares it abstract, C declares none. `invokevirtual C.m` on a C
// selects B's abstract `m` (JVMS §5.4.6), so every call is an
// AbstractMethodError, however hot the call site gets.
//
// C and the caller are generated with the java.lang.classfile API (javac
// refuses a concrete C without `m`). `call(C)` is invoked 30 000 times, far
// past the JIT's thresholds, and the probe counts the outcomes.
//
// Run: javac -d out L4W35FinalDevirtAbstract.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W35FinalDevirtAbstract
//
// Expected HotSpot 25 output (default and -Xint):
//   AbstractMethodError: 30000, other: 0
//   first: java.lang.AbstractMethodError: Receiver class FdC does not define or inherit an implementation of the resolved method 'abstract java.lang.String m()' of abstract class L4W35FinalDevirtAbstract$FdB.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W35FinalDevirtAbstract {
    public static class FdA {
        public String m() {
            return "A";
        }
    }

    public abstract static class FdB extends FdA {
        @Override
        public abstract String m();
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W35FinalDevirtAbstract.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Throwable {
        ClassDesc fdb = ClassDesc.of(FdB.class.getName());
        ClassDesc fdc = ClassDesc.of("FdC");
        MethodTypeDesc str = MethodTypeDesc.of(ConstantDescs.CD_String);
        Loader loader = new Loader();
        Class<?> c = loader.define("FdC", ClassFile.of().build(fdc, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER | ClassFile.ACC_FINAL);
            cb.withSuperclass(fdb);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(fdb, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).return_());
        }));
        Class<?> caller = loader.define("FdCaller", ClassFile.of().build(ClassDesc.of("FdCaller"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("call", MethodTypeDesc.of(ConstantDescs.CD_String, fdc),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).invokevirtual(fdc, "m", str).areturn());
        }));
        Object receiver = c.getConstructor().newInstance();
        MethodHandle call = MethodHandles.lookup().findStatic(caller, "call", MethodType.methodType(String.class, c))
                .asType(MethodType.methodType(String.class, Object.class));
        int ame = 0;
        int other = 0;
        String first = null;
        for (int i = 0; i < 30_000; i++) {
            try {
                String r = (String) call.invokeExact(receiver);
                other++;
                if (first == null) {
                    first = "returned " + r;
                }
            } catch (AbstractMethodError e) {
                ame++;
                if (first == null) {
                    first = e.toString();
                }
            }
        }
        System.out.println("AbstractMethodError: " + ame + ", other: " + other);
        System.out.println("first: " + first);
    }
}
