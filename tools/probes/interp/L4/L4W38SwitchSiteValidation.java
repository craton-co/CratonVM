// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L4 (review): a `SwitchBootstraps`
// `typeSwitch` / `enumSwitch` call site the JDK refuses fails linkage as
// HotSpot's does: `BootstrapMethodError` caused by the bootstrap's own
// `IllegalArgumentException`. Each caller is generated with the
// java.lang.classfile API; `make(Object)` is one `invokedynamic` (its
// restart index `0`), boxed. Each site is run twice: the second failure is
// the recorded one (JVMS 5.4.3), a new error without a cause.
//
//   ts-* / es-*   typeSwitch / enumSwitch; `*-ok` are the controls.
//
// Before wave 38 CratonVM's native switches validated nothing: a site whose
// invocation type is not `(T,int)int` (the `*-arity` / `*-ret` rows) popped
// two operands and pushed an `int` whatever it declared, and every refused
// label linked. The invocation-type rows are refused in every mode; the
// selector and label rows under `--jdk-only` (`--compatible` links them, as
// before; its lines for them are not recorded).
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints
// `[DBG_LAMBDA] link-check recorded java/lang/BootstrapMethodError for cp#N`
// once per refused row.
//
// Run: javac -d out L4W38SwitchSiteValidation.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W38SwitchSiteValidation
//
// Expected HotSpot 25 output (default and -Xint):
//   ts-ok: 1 | 1
//   ts-zero: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0 | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   es-zero: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Illegal invocation type ()int | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   ts-arity: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Illegal invocation type (Object)int | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   ts-ret: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Illegal invocation type (Object,int)long | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   ts-long-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: label with illegal type found: class java.lang.Long | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   ts-float-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: label with illegal type found: class java.lang.Float | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   ts-methodtype-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: label with illegal type found: class java.lang.invoke.MethodType | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   es-ok: 1 | 1
//   es-not-enum: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Illegal invocation type (Object,int)int | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   es-int-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: label with illegal type found: class java.lang.Integer, expected label of type either String or Class | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   es-class-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: the Class label: class java.lang.String, expected the provided enum class: class L4W38SwitchSiteValidation$E | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   es-own-class-label: 0 | 0
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W38SwitchSiteValidation {
    public enum E {
        A, B
    }

    static final ClassDesc SB = ClassDesc.of("java.lang.runtime.SwitchBootstraps");
    static final MethodTypeDesc BSM_TYPE = MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
            ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
            ClassDesc.of("java.lang.invoke.MethodType"), ConstantDescs.CD_Object.arrayType());
    static final ClassDesc ENUM = ClassDesc.of(E.class.getName());

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W38SwitchSiteValidation.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage() + (c == null ? "" : " / " + c);
    }

    static String row(String name, String bsm, MethodTypeDesc type, Object selector, ConstantDesc... labels) {
        DirectMethodHandleDesc handle = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SB, bsm,
                BSM_TYPE);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(handle, "sw", type, labels);
        MethodTypeDesc makeType = MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object);
        byte[] b = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", makeType, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                if (type.parameterCount() > 0) {
                    code.aload(0).checkcast(type.parameterType(0));
                }
                for (int i = 1; i < type.parameterCount(); i++) {
                    code.iconst_0();
                }
                code.invokedynamic(site);
                if (type.returnType().equals(ConstantDescs.CD_long)) {
                    code.invokestatic(ConstantDescs.CD_Long, "valueOf",
                            MethodTypeDesc.of(ConstantDescs.CD_Long, ConstantDescs.CD_long));
                } else {
                    code.invokestatic(ConstantDescs.CD_Integer, "valueOf",
                            MethodTypeDesc.of(ConstantDescs.CD_Integer, ConstantDescs.CD_int));
                }
                code.areturn();
            });
        });
        Method make;
        try {
            make = LOADER.define(name, b).getMethod("make", Object.class);
        } catch (Throwable t) {
            return "setup: " + t;
        }
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                out.append(make.invoke(null, selector));
            } catch (InvocationTargetException e) {
                out.append(describe(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    public static void main(String[] args) {
        if (E.B.ordinal() != 1) {
            throw new AssertionError();
        }
        ClassDesc obj = ConstantDescs.CD_Object;
        ClassDesc i = ConstantDescs.CD_int;
        MethodTypeDesc ts = MethodTypeDesc.of(i, obj, i);
        MethodTypeDesc es = MethodTypeDesc.of(i, ENUM, i);
        System.out.println("ts-ok: " + row("LsTsOk", "typeSwitch", ts, "s", ConstantDescs.CD_Integer,
                ConstantDescs.CD_String));
        System.out.println("ts-zero: " + row("LsTsZero", "typeSwitch", MethodTypeDesc.of(i), "s",
                ConstantDescs.CD_String));
        System.out.println("es-zero: " + row("LsEsZero", "enumSwitch", MethodTypeDesc.of(i), E.B, "A"));
        System.out.println("ts-arity: " + row("LsTsArity", "typeSwitch", MethodTypeDesc.of(i, obj), "s",
                ConstantDescs.CD_String));
        System.out.println("ts-ret: " + row("LsTsRet", "typeSwitch",
                MethodTypeDesc.of(ConstantDescs.CD_long, obj, i), "s", ConstantDescs.CD_String));
        System.out.println("ts-long-label: " + row("LsTsLong", "typeSwitch", ts, 5L, 5L));
        System.out.println("ts-float-label: " + row("LsTsFloat", "typeSwitch", ts, 5f, 5f));
        System.out.println("ts-methodtype-label: " + row("LsTsMt", "typeSwitch", ts, "s", MethodTypeDesc.of(i)));
        System.out.println("es-ok: " + row("LsEsOk", "enumSwitch", es, E.B, "A", "B"));
        System.out.println("es-not-enum: " + row("LsEsNotEnum", "enumSwitch", ts, E.B, "A"));
        System.out.println("es-int-label: " + row("LsEsInt", "enumSwitch", es, E.B, 1));
        System.out.println("es-class-label: " + row("LsEsClass", "enumSwitch", es, E.B, ConstantDescs.CD_String));
        System.out.println("es-own-class-label: " + row("LsEsOwnClass", "enumSwitch", es, E.B, ENUM));
    }
}
