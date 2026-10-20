// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L4
// (`i38-L4-proposal-native-bootstraps-link-only-javac-shapes-natively`, and
// row 4 of `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`):
// a `LambdaMetafactory` site whose shape javac never emits (here, a dynamic
// constant as the implementation handle) links through the JDK's own
// `LambdaMetafactory` when `CRATONVM_INDY_OTHER_SHAPES_TO_JDK` is on (the
// default since wave 48), which builds the JDK's proxy or raises the JDK's
// error.
//
// Each row generates (java.lang.classfile) a class `Lo<Row> extends
// L4W48LambdaSiteJdkRoute$Base` that overrides `who()` ("sub") and has a
// private `priv()` ("priv"); its static `make()` is `new Lo<Row>()` then one
// `invokedynamic` over `LambdaMetafactory.metafactory` for `Supplier.get`
// capturing it, whose static argument 1 is a dynamic constant answered by
// `condy`:
//
//   private  `findSpecial(Lo, "priv", ()String, Lo)`
//   super    `findSpecial(Base, "who", ()String, Lo)` (links; `get()` throws)
//   virtual  `findVirtual(Lo, "who", ()String)`
//   bound    `findVirtual(Lo, "who", ()String).bindTo(null)` (not direct)
//   null     `null`
//
// Each `make()` runs twice and calls `get()` on what it answers.
//
// Run: javac -d out L4W48LambdaSiteJdkRoute.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W48LambdaSiteJdkRoute
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   private: priv | priv
//   super: setup: java.lang.invoke.WrongMethodTypeException: handle's method type (Losuper)String but found (Base)String
//   virtual: sub | sub
//   bound: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: MethodHandle()String is not direct or cannot be cracked | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   null: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.NullPointerException: null | java.lang.BootstrapMethodError: bootstrap method initialization exception
//
// With the switch off (`CRATONVM_INDY_OTHER_SHAPES_TO_JDK=0`) row `super`
// ends the VM with the uncatchable `internal error: invokedynamic: cp#N is not
// a MethodHandle`. Positive control: `CRATONVM_DBG_INDY_ALL=1` prints
// `[indy-all] other shape cp#N: jdk bootstrap
// java/lang/invoke/LambdaMetafactory.metafactory` once per row execution.
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

public class L4W48LambdaSiteJdkRoute {
    public static class Base {
        public String who() {
            return "base";
        }
    }

    static final ClassDesc SELF = ClassDesc.of("L4W48LambdaSiteJdkRoute");
    static final ClassDesc BASE = ClassDesc.of("L4W48LambdaSiteJdkRoute$Base");
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
        Class<?> self = lookup.lookupClass();
        MethodType ms = MethodType.methodType(String.class);
        return switch (name) {
            case "private" -> lookup.findSpecial(self, "priv", ms, self);
            case "super" -> lookup.findSpecial(Base.class, "who", ms, self);
            case "virtual" -> lookup.findVirtual(self, "who", ms);
            case "bound" -> lookup.findVirtual(self, "who", ms).bindTo(null);
            case "null" -> null;
            default -> null;
        };
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W48LambdaSiteJdkRoute.class.getClassLoader());
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

    @SuppressWarnings("unchecked")
    static String row(String rowName) {
        String cls = "Lo" + rowName;
        ClassDesc lo = ClassDesc.of(cls);
        ConstantDesc[] args = {
            MethodTypeDesc.of(ConstantDescs.CD_Object),
            DynamicConstantDesc.ofNamed(CONDY, rowName, MH),
            MethodTypeDesc.of(STR),
        };
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(BSM, "get", MethodTypeDesc.of(SUPPLIER, lo), args);
        byte[] b = ClassFile.of().build(lo, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(BASE);
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    c -> c.aload(0).invokespecial(BASE, "<init>", ConstantDescs.MTD_void).return_());
            cb.withMethodBody("who", MethodTypeDesc.of(STR), ClassFile.ACC_PUBLIC,
                    c -> c.ldc("sub").areturn());
            cb.withMethodBody("priv", MethodTypeDesc.of(STR), ClassFile.ACC_PRIVATE,
                    c -> c.ldc("priv").areturn());
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.new_(lo).dup()
                            .invokespecial(lo, "<init>", ConstantDescs.MTD_void).invokedynamic(site).areturn());
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
        for (String name : new String[] {"private", "super", "virtual", "bound", "null"}) {
            System.out.println(name + ": " + row(name));
        }
    }
}
