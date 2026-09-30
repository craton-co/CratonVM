// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L4 (row 4 of
// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`, "Still
// open in row 4" of wave 43): a dynamic constant that answers the
// IMPLEMENTATION handle of a `LambdaMetafactory` site (static argument 1) with
// a `findSpecial` handle. HotSpot cracks it (`Lookup.revealDirect`,
// `REF_invokeSpecial`) and links: a private method of the lambda's own class
// is invoked as `REF_invokeVirtual` (`AbstractValidatingLambdaMetafactory`).
// (A superclass method's special handle, `findSpecial(Base, "who", ()String,
// Lo)`, links on HotSpot 25 too, but its `get()` throws
// `WrongMethodTypeException: handle's method type (Lo)String but found
// (Base)String`; not a row: CratonVM does not model that.)
//
// Each row generates (java.lang.classfile) a class `Lo<Row> extends
// L4W44LambdaSiteSpecialHandle$Base` that overrides `who()` ("sub") and has a
// private `priv()` ("priv"); its static `make()` is `new Lo<Row>()` then one
// `invokedynamic` over `LambdaMetafactory.metafactory` for `Supplier.get`
// capturing it, whose static argument 1 is a dynamic constant answered by
// `condy` with `lookup.findSpecial(..., lookup.lookupClass())`:
//
//   private   `findSpecial(Lo, "priv", ()String, Lo)`    -> "priv"
//
// Each `make()` runs twice and calls `get()` on what it answers.
//
// Run: javac -d out L4W44LambdaSiteSpecialHandle.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W44LambdaSiteSpecialHandle
//
// Expected HotSpot 25 output (default and -Xint, measured locally), also
// `--compatible`'s (the read-back is in every mode):
//   private: priv | priv
//
// On the base `5248262b7` the special handle value is not read back
// (`method_handle_value_member` answered `None` for `MH_KIND_SPECIAL`), so
// the site does not link as HotSpot's does. Positive control:
// `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints
//   `[DBG_LAMBDA] link-check get(LLoprivate;)Ljava/util/function/Supplier;: dynamic static argument 1 is a MethodHandle (REF_invokeSpecial Loprivate.priv()Ljava/lang/String;)`
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

public class L4W44LambdaSiteSpecialHandle {
    public static class Base {
        public String who() {
            return "base";
        }
    }

    static final ClassDesc SELF = ClassDesc.of("L4W44LambdaSiteSpecialHandle");
    static final ClassDesc BASE = ClassDesc.of("L4W44LambdaSiteSpecialHandle$Base");
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
            default -> null;
        };
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W44LambdaSiteSpecialHandle.class.getClassLoader());
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
        for (String name : new String[] {"private"}) {
            System.out.println(name + ": " + row(name));
        }
    }
}
