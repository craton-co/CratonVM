// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L2: a call site whose class is missing
// at run time (separate compilation deleted it), warmed until the JIT compiles
// the caller. HotSpot: NoClassDefFoundError naming the class (internal form),
// caused by the application loader's ClassNotFoundException (dotted form), on
// every call (the resolution error is recorded with its cause).
//
// The compiled dispatch sink (`handle_jit_dispatch_error`, the
// `ClassFileError::ClassNotFound` arm) built a bare NoClassDefFoundError with
// no cause, where the interpreter's conversion (`convert_class_not_found_for`)
// attaches it. Wave 100 routes the arm through that conversion with the site's
// class as the initiating one.
//
//   UMD  the callers (UMissing / UMissingV are never defined):
//     s1  invokestatic  UMissing.m()I
//     v1  invokevirtual UMissingV.m()I  on a null receiver
//         (resolution comes first: NoClassDefFoundError, not NPE)
//
// Each row prints the distinct answers of 20000 calls.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L2W100CompiledSiteMissingClass
// Expected (HotSpot 25):
//   s1: [java.lang.NoClassDefFoundError: UMissing <- java.lang.ClassNotFoundException: UMissing]
//   v1: [java.lang.NoClassDefFoundError: UMissingV <- java.lang.ClassNotFoundException: UMissingV]

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Set;
import java.util.TreeSet;

public class L2W100CompiledSiteMissingClass {
    static String describe(Throwable t) {
        String s = t.getClass().getName() + ": " + t.getMessage();
        Throwable c = t.getCause();
        return c == null ? s + " <- (no cause)" : s + " <- " + c.getClass().getName() + ": " + c.getMessage();
    }

    static String run(Method m) {
        try {
            return String.valueOf(m.invoke(null));
        } catch (InvocationTargetException e) {
            return describe(e.getCause());
        } catch (Throwable t) {
            return "probe failure " + t;
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodTypeDesc intDesc = MethodTypeDesc.of(ConstantDescs.CD_int);
        ClassDesc missing = ClassDesc.of("UMissing");
        ClassDesc missingV = ClassDesc.of("UMissingV");
        byte[] bytes = ClassFile.of().build(ClassDesc.of("UMD"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(ConstantDescs.CD_Object);
            cb.withMethodBody("s1", intDesc, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                code -> code.invokestatic(missing, "m", intDesc).ireturn());
            cb.withMethodBody("v1", intDesc, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                code -> code.aconst_null().invokevirtual(missingV, "m", intDesc).ireturn());
        });
        Class<?> umd = MethodHandles.lookup().defineClass(bytes);
        for (String row : new String[] {"s1", "v1"}) {
            Method m = umd.getMethod(row);
            Set<String> seen = new TreeSet<>();
            for (int k = 0; k < 20000; k++) seen.add(run(m));
            System.out.println(row + ": " + seen);
        }
    }
}
