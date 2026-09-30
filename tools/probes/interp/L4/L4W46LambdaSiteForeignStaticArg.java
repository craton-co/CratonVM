// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L4 (row 4 of
// `docs/known-issues/interpreter/i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked-20261001.md`):
// a dynamic constant among a `LambdaMetafactory` site's static arguments
// that answers an object of a class OUTSIDE the few `java.lang` classes the
// checks named (a `java.util`, `java.sql` or user class, a `Boolean`) is the
// invoker's `ClassCastException`, whose message names each class's module
// and loader (`SharedRuntime::generate_class_cast_message`), in a
// `BootstrapMethodError`; `altMetafactory`'s `extractArg` refuses it as
// `argument has wrong type`. CratonVM left such a value unclassified, the
// checks stopped undecided, and `bootstrap_lambda` failed with its
// uncatchable internal error (the site did not link and threw no Java
// exception the probe could catch).
//
// Each row generates (java.lang.classfile) a class `Lo<Row>` whose static
// `make()` is one `invokedynamic` over `LambdaMetafactory.metafactory` (or
// `altMetafactory` with flags 0) for `Supplier.get` with the static arguments
// `(()Object, hello()String, ()String)`, one of them replaced by a dynamic
// constant declared `Object` whose bootstrap answers by its name. Each
// `make()` runs twice (the second is the recorded linkage error).
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints
//   `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;: dynamic static argument 0 is an object of another class`
// for `list-0`, then `... refused java/lang/ClassCastException: ...`. On the
// base (55834015b) the same row printed `... is not classified` and
// `... undecided (a dynamic static argument)`.
//
// Known difference (host run of `9106a67a0`, both modes): row `sql-0` names
// `java.sql.Date`'s loader `'bootstrap'` where HotSpot says `'platform'`, a
// defect of the shared message funnel, not of this refusal:
// `docs/known-issues/interpreter/i46-L4-platform-module-classes-are-named-loader-bootstrap-in-hotspot-worded-messages-20261010.md`.
//
// Run: javac -d out L4W46LambdaSiteForeignStaticArg.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W46LambdaSiteForeignStaticArg
//
// Expected HotSpot 25 output (default and -Xint, identical), also
// `--compatible`'s:
//   list-0: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.util.ArrayList cannot be cast to class java.lang.invoke.MethodType (java.util.ArrayList and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   list-1: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.util.ArrayList cannot be cast to class java.lang.invoke.MethodHandle (java.util.ArrayList and java.lang.invoke.MethodHandle are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   bool-2: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.Boolean cannot be cast to class java.lang.invoke.MethodType (java.lang.Boolean and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   sql-0: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.sql.Date cannot be cast to class java.lang.invoke.MethodType (java.sql.Date is in module java.sql of loader 'platform'; java.lang.invoke.MethodType is in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   user-1: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class L4W46LambdaSiteForeignStaticArg cannot be cast to class java.lang.invoke.MethodHandle (L4W46LambdaSiteForeignStaticArg is in unnamed module of loader 'app'; java.lang.invoke.MethodHandle is in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   alt-list-0: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: argument has wrong type | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   alt-user-1: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: argument has wrong type | java.lang.BootstrapMethodError: bootstrap method initialization exception
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
import java.lang.reflect.InvocationTargetException;
import java.util.function.Supplier;

public class L4W46LambdaSiteForeignStaticArg {
    static final ClassDesc SELF = ClassDesc.of("L4W46LambdaSiteForeignStaticArg");
    static final ClassDesc LMF = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc CALL_SITE = ClassDesc.of("java.lang.invoke.CallSite");
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "metafactory", MethodTypeDesc.of(CALL_SITE, LOOKUP, STR, MT, MT, MH, MT));
    static final DirectMethodHandleDesc ALT = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "altMetafactory", MethodTypeDesc.of(CALL_SITE, LOOKUP, STR, MT, ConstantDescs.CD_Object.arrayType()));
    static final DirectMethodHandleDesc CONDY = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "condy", MethodTypeDesc.of(ConstantDescs.CD_Object, LOOKUP, STR, ConstantDescs.CD_Class));

    /** The dynamic constants' bootstrap: answers by the constant's name. */
    public static Object condy(MethodHandles.Lookup lookup, String name, Class<?> type) {
        return switch (name) {
            case "list" -> new java.util.ArrayList<String>();
            case "bool" -> Boolean.TRUE;
            case "sql" -> new java.sql.Date(0L);
            case "user" -> new L4W46LambdaSiteForeignStaticArg();
            default -> null;
        };
    }

    public static String hello() {
        return "hi";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W46LambdaSiteForeignStaticArg.class.getClassLoader());
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

    /** `position` of the three static arguments is the dynamic constant `name`. */
    @SuppressWarnings("unchecked")
    static String row(String rowName, boolean alt, int position, String name) {
        ConstantDesc[] three = {
            MethodTypeDesc.of(ConstantDescs.CD_Object),
            MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, "hello", MethodTypeDesc.of(STR)),
            MethodTypeDesc.of(STR),
        };
        three[position] = DynamicConstantDesc.ofNamed(CONDY, name, ConstantDescs.CD_Object);
        ConstantDesc[] args = three;
        if (alt) {
            args = new ConstantDesc[] {three[0], three[1], three[2], 0};
        }
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(alt ? ALT : BSM, "get",
                MethodTypeDesc.of(ClassDesc.of("java.util.function.Supplier")), args);
        String cls = "Lo" + rowName.replace("-", "");
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.invokedynamic(site).areturn());
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
        System.out.println("list-0: " + row("list-0", false, 0, "list"));
        System.out.println("list-1: " + row("list-1", false, 1, "list"));
        System.out.println("bool-2: " + row("bool-2", false, 2, "bool"));
        System.out.println("sql-0: " + row("sql-0", false, 0, "sql"));
        System.out.println("user-1: " + row("user-1", false, 1, "user"));
        System.out.println("alt-list-0: " + row("alt-list-0", true, 0, "list"));
        System.out.println("alt-user-1: " + row("alt-user-1", true, 1, "user"));
    }
}
