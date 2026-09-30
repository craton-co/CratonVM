// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 33 (orchestrator): a `LambdaMetafactory` call
// site whose implementation method is missing fails at the `invokedynamic`
// itself, with the resolution's own error, unwrapped
// (docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md,
// item 2). This is the shape of a library upgrade that removed a method a
// separately compiled lambda or method reference points at.
//
// Each caller is generated with the java.lang.classfile API: `make()` is one
// `invokedynamic` of `LambdaMetafactory.metafactory` for a `Supplier` whose
// implementation is `Other.<name>()` (static, returning `String`).
//
//   missing  `Other.gone()` does not exist -> NoSuchMethodError at link
//   present  `Other.here()` exists -> the supplier answers "here"
//
// Before wave 33 CratonVM built the lambda and failed only in `get()`.
// `--compatible` keeps that.
//
// Run: javac -d out L4W33LambdaImplMissing.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W33LambdaImplMissing
//
// Expected HotSpot 25 output (default and -Xint):
//   missing: link: java.lang.NoSuchMethodError: 'java.lang.String L4W33LambdaImplMissing$Other.gone()'
//   present: here
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Supplier;

public class L4W33LambdaImplMissing {
    public static class Other {
        public static String here() {
            return "here";
        }
    }

    static byte[] caller(String name, String impl) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc supplier = ClassDesc.of("java.util.function.Supplier");
        DirectMethodHandleDesc boot = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ClassDesc.of("java.lang.invoke.LambdaMetafactory"), "metafactory",
                MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                        ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                        ClassDesc.of("java.lang.invoke.MethodType"), ClassDesc.of("java.lang.invoke.MethodType"),
                        ClassDesc.of("java.lang.invoke.MethodHandle"), ClassDesc.of("java.lang.invoke.MethodType")));
        DirectMethodHandleDesc target = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ClassDesc.of(Other.class.getName()), impl, MethodTypeDesc.of(ConstantDescs.CD_String));
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(boot, "get", MethodTypeDesc.of(supplier),
                MethodTypeDesc.of(ConstantDescs.CD_Object), target, MethodTypeDesc.of(ConstantDescs.CD_String));
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(supplier), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(site).areturn());
        });
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W33LambdaImplMissing.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String describe(Throwable t) {
        return t.getMessage() == null ? t.getClass().getName() : t.getClass().getName() + ": " + t.getMessage();
    }

    static String run(Loader loader, String name, String impl) {
        Supplier<?> s;
        try {
            s = (Supplier<?>) loader.define(name, caller(name, impl)).getMethod("make").invoke(null);
        } catch (InvocationTargetException e) {
            return "link: " + describe(e.getCause());
        } catch (Throwable t) {
            return "setup: " + describe(t);
        }
        try {
            return String.valueOf(s.get());
        } catch (Throwable t) {
            return "call: " + describe(t);
        }
    }

    public static void main(String[] args) {
        Loader loader = new Loader();
        System.out.println("missing: " + run(loader, "LambdaMissing", "gone"));
        System.out.println("present: " + run(loader, "LambdaPresent", "here"));
    }
}
