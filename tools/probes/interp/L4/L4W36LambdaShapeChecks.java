// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 36 (orchestrator): a `LambdaMetafactory` call site
// the JDK's factory refuses fails linkage with its `LambdaConversionException`
// (docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md,
// item 2). Each caller is generated with the java.lang.classfile API.
//
//   not-interface  the factory type returns `Object`, a class
//   arity          `Impl.two(int, int)` implements a no-argument
//                  `Supplier.get` with nothing captured
//   ok             `Impl.one()` implements `Supplier.get` (the control)
//
// Before wave 36 CratonVM linked the first two itself. `--compatible` keeps
// that.
//
// Run: javac -d out L4W36LambdaShapeChecks.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W36LambdaShapeChecks
//
// Expected HotSpot 25 output (default and -Xint):
//   not-interface: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: java.lang.Object is not an interface
//   arity: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Incorrect number of parameters for static method invokeStatic L4W36LambdaShapeChecks$Impl.two:(int,int)String; 0 captured parameters, 0 functional interface method parameters, 2 implementation parameters
//   ok: supplier one
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Supplier;

public class L4W36LambdaShapeChecks {
    public static class Impl {
        public static String one() {
            return "one";
        }

        public static String two(int a, int b) {
            return "two";
        }
    }

    static final DirectMethodHandleDesc LMF = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            ClassDesc.of("java.lang.invoke.LambdaMetafactory"), "metafactory",
            MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.MethodType"), ClassDesc.of("java.lang.invoke.MethodType"),
                    ClassDesc.of("java.lang.invoke.MethodHandle"), ClassDesc.of("java.lang.invoke.MethodType")));

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W36LambdaShapeChecks.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String row(String name, ClassDesc factoryReturn, String impl, MethodTypeDesc implType) {
        DirectMethodHandleDesc target = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ClassDesc.of(Impl.class.getName()), impl, implType);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(LMF, "get", MethodTypeDesc.of(factoryReturn),
                MethodTypeDesc.of(ConstantDescs.CD_Object), target, MethodTypeDesc.of(ConstantDescs.CD_String));
        byte[] b = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.invokedynamic(site).areturn());
        });
        try {
            Object r = LOADER.define(name, b).getMethod("make").invoke(null);
            return r instanceof Supplier<?> s ? "supplier " + s.get() : "made " + r.getClass().getSimpleName();
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            Throwable c = t.getCause();
            return t.getClass().getName() + (c == null ? "" : " / " + c.getClass().getName() + ": " + c.getMessage());
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) {
        MethodTypeDesc str = MethodTypeDesc.of(ConstantDescs.CD_String);
        MethodTypeDesc str2 = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_int, ConstantDescs.CD_int);
        ClassDesc supplier = ClassDesc.of("java.util.function.Supplier");
        System.out.println("not-interface: " + row("LsNotIface", ConstantDescs.CD_Object, "one", str));
        System.out.println("arity: " + row("LsArity", supplier, "two", str2));
        System.out.println("ok: " + row("LsOk", supplier, "one", str));
    }
}
