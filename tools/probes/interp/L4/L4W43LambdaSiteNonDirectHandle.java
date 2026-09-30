// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4 (row 4 of
// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`, "What
// remains of row 4 (wave 42)"): a dynamic constant (`CONSTANT_Dynamic`) that
// answers the IMPLEMENTATION handle of a `LambdaMetafactory` site (static
// argument 1) with a handle that is NOT a direct `findStatic` /
// `findVirtual` one. HotSpot's `AbstractValidatingLambdaMetafactory` cracks
// it (`Lookup.revealDirect`): a bound, adapted or combinator handle is
// `LambdaConversionException: <handle> is not direct or cannot be cracked`;
// a `findConstructor` handle is direct and links. CratonVM read only the
// direct static / virtual shapes back, so every row failed with an
// uncatchable internal error (`invokedynamic: cp#N is not a MethodHandle`).
//
// Each row generates (java.lang.classfile) a class `Lo<Row>` whose static
// `make()` is one `invokedynamic` over `LambdaMetafactory.metafactory` for
// `Supplier.get` (instantiated `()Object`) whose static argument 1 is a
// dynamic constant (declared `MethodHandle`) answered by
// `L4W43LambdaSiteNonDirectHandle.condy`:
//
//   bound      `findVirtual(String, "trim", ()String).bindTo(" hi ")`
//   adapted    `findStatic(Probe, "hello", ()String).asType(()Object)`
//   inserted   `insertArguments(findStatic(String, "valueOf", (Object)String), 0, "x")`
//   constant   `MethodHandles.constant(String, "k")`
//   ctor       `findConstructor(ArrayList, ()void)`           -> "[]"
//
// Each `make()` runs twice and calls `get()` on what it answers.
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints, for row `bound`,
//   `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;: dynamic static argument 1 is a MethodHandle (not direct: MethodHandle()String)`
// then `... refused java/lang/invoke/LambdaConversionException: MethodHandle()String is not direct or cannot be cracked`;
// for row `ctor`, `... is a MethodHandle (REF_newInvokeSpecial java/util/ArrayList.<init>()V)`.
//
// Run: javac -d out L4W43LambdaSiteNonDirectHandle.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W43LambdaSiteNonDirectHandle
//
// Expected HotSpot 25 output (default and -Xint, measured locally), also
// `--compatible`'s (the fix is in every mode: the site was an internal error
// in both):
//   bound: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: MethodHandle()String is not direct or cannot be cracked | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   adapted: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: MethodHandle()Object is not direct or cannot be cracked | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   inserted: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: MethodHandle()String is not direct or cannot be cracked | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   constant: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: MethodHandle()String is not direct or cannot be cracked | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   ctor: [] | []
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.DynamicConstantDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Supplier;

public class L4W43LambdaSiteNonDirectHandle {
    static final ClassDesc SELF = ClassDesc.of("L4W43LambdaSiteNonDirectHandle");
    static final ClassDesc LMF = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc SUPPLIER = ClassDesc.of("java.util.function.Supplier");
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "metafactory", MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), STR, MT, MT, MH, MT));
    static final DirectMethodHandleDesc CONDY = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "condy", MethodTypeDesc.of(ConstantDescs.CD_Object,
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), STR, ConstantDescs.CD_Class));

    /** The dynamic constants' bootstrap: answers by the constant's name. */
    public static Object condy(MethodHandles.Lookup lookup, String name, Class<?> type) throws Exception {
        return switch (name) {
            case "bound" -> lookup.findVirtual(String.class, "trim", MethodType.methodType(String.class))
                    .bindTo(" hi ");
            case "adapted" -> lookup.findStatic(L4W43LambdaSiteNonDirectHandle.class, "hello",
                    MethodType.methodType(String.class)).asType(MethodType.methodType(Object.class));
            case "inserted" -> MethodHandles.insertArguments(lookup.findStatic(String.class, "valueOf",
                    MethodType.methodType(String.class, Object.class)), 0, "x");
            case "constant" -> MethodHandles.constant(String.class, "k");
            case "ctor" -> lookup.findConstructor(java.util.ArrayList.class, MethodType.methodType(void.class));
            default -> null;
        };
    }

    public static String hello() {
        return "hi";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W43LambdaSiteNonDirectHandle.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage()
                + (c == null ? "" : " / " + c.getClass().getName() + ": " + c.getMessage());
    }

    /** `captured` is `null` for no captured argument, else its declared type and value. */
    @SuppressWarnings("unchecked")
    static String row(String rowName, ClassDesc capturedType, String captured, ClassDesc instantiatedReturn) {
        ConstantDesc[] args = {
            MethodTypeDesc.of(ConstantDescs.CD_Object),
            DynamicConstantDesc.ofNamed(CONDY, rowName, MH),
            MethodTypeDesc.of(instantiatedReturn),
        };
        MethodTypeDesc siteType = capturedType == null
                ? MethodTypeDesc.of(SUPPLIER)
                : MethodTypeDesc.of(SUPPLIER, capturedType);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(BSM, "get", siteType, args);
        String cls = "Lo" + rowName;
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                        if (captured != null) {
                            code.ldc(captured);
                        }
                        code.invokedynamic(site).areturn();
                    });
        });
        try {
            var make = LOADER.define(cls, b).getMethod("make");
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 2; i++) {
                if (i > 0) {
                    out.append(" | ");
                }
                try {
                    out.append(((Supplier<Object>) make.invoke(null)).get());
                } catch (InvocationTargetException e) {
                    out.append(describe(e.getCause()));
                }
            }
            return out.toString();
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) {
        for (String name : new String[] {"bound", "adapted", "inserted", "constant", "ctor"}) {
            System.out.println(name + ": " + row(name, null, null, ConstantDescs.CD_Object));
        }
    }
}
