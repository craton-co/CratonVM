// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L4 (row 1 of
// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`): HotSpot
// resolves a lambda site's `MethodType` static arguments before
// `LambdaMetafactory` runs, which loads every class they name. So a class
// that is not loaded YET is known to the checks, and a missing one is the
// resolution's `NoClassDefFoundError`. CratonVM's checks read only loaded
// classes and left such a site linked (`--jdk-only`).
//
// A loader serves the generated classes lazily from `findClass`:
// `LoIface` (an interface), `LoImpl` (a class that does not implement it)
// and one site class per row, whose static `make()` is one `invokedynamic`
// over `LambdaMetafactory.metafactory` for `Consumer.accept`, with the SAM
// type `(Object)void`, implementation `static impl(<implParam>)void` of the
// site class, and the instantiated type `(<instParam>)void`. Neither `LoIface`
// nor `LoImpl` is loaded before the site links. Each `make()` runs twice.
//
//   not-convertible   instantiated `(LoIface)void`, impl `(LoImpl)void`:
//                     `LambdaConversionException` (wrapped)
//   missing           instantiated `(LoMissing)void`: `NoClassDefFoundError`
//                     (unwrapped, caused by the loader's `ClassNotFoundException`;
//                     the second is the recorded error, no cause)
//   ok                instantiated `(LoImpl)void`, impl `(LoImpl)void`: links
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints
// `[DBG_LAMBDA] link-check accept()Ljava/util/function/Consumer;: loaded N type(s), checking again`
// for the rows `not-convertible` and `ok`.
//
// Run: javac -d out L4W39LambdaSiteLoadsItsTypes.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W39LambdaSiteLoadsItsTypes
//
// Expected HotSpot 25 output (default and -Xint):
//   not-convertible: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: Type mismatch for lambda argument 0: interface LoIface is not convertible to class LoImpl | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   missing: java.lang.NoClassDefFoundError: LoMissing / java.lang.ClassNotFoundException: LoMissing | java.lang.NoClassDefFoundError: LoMissing
//   ok: linked | linked
//
// `--compatible` checks none of this (by design) and loads nothing; its
// lines are not recorded.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.Map;

public class L4W39LambdaSiteLoadsItsTypes {
    static final ClassDesc LMF = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "metafactory", MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String, MT, MT, MH,
                    MT));
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc VOID = ConstantDescs.CD_void;

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> classes = new HashMap<>();

        Loader() {
            super(L4W39LambdaSiteLoadsItsTypes.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = classes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] site(String name, ClassDesc implParam, ClassDesc instParam) {
        ClassDesc self = ClassDesc.of(name);
        MethodTypeDesc implType = MethodTypeDesc.of(VOID, implParam);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(BSM, "accept",
                MethodTypeDesc.of(ClassDesc.of("java.util.function.Consumer")),
                MethodTypeDesc.of(VOID, OBJ),
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "impl", implType),
                MethodTypeDesc.of(VOID, instParam));
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("impl", implType, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.return_());
            cb.withMethodBody("make", MethodTypeDesc.of(OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(site).areturn());
        });
    }

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage() + (c == null ? "" : " / " + c);
    }

    static String row(String name, String implParam, String instParam) {
        Loader loader = new Loader();
        ClassDesc iface = ClassDesc.of("LoIface");
        ClassDesc impl = ClassDesc.of("LoImpl");
        loader.classes.put("LoIface", ClassFile.of().build(iface, cb -> cb.withFlags(
                ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT)));
        loader.classes.put("LoImpl", ClassFile.of().build(impl, cb -> cb.withFlags(ClassFile.ACC_PUBLIC)));
        loader.classes.put(name, site(name, ClassDesc.of(implParam), ClassDesc.of(instParam)));
        Method make;
        try {
            make = loader.loadClass(name).getMethod("make");
        } catch (Throwable t) {
            return "setup: " + t;
        }
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                out.append(make.invoke(null) != null ? "linked" : "null");
            } catch (InvocationTargetException e) {
                out.append(describe(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    public static void main(String[] args) {
        System.out.println("not-convertible: " + row("LoNotConvertible", "LoImpl", "LoIface"));
        System.out.println("missing: " + row("LoMissingRow", "LoImpl", "LoMissing"));
        System.out.println("ok: " + row("LoOk", "LoImpl", "LoImpl"));
    }
}
