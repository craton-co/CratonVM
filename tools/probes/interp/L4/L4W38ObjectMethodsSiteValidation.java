// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L4 (review): an `ObjectMethods.bootstrap`
// call site the JDK refuses fails linkage as HotSpot's does:
// `BootstrapMethodError` caused by the bootstrap's `IllegalArgumentException`.
// Each row generates, with the java.lang.classfile API, a record
// `Lo<Row>(int a, String b)` whose static `make(Object)` casts its argument to
// the record and is one `invokedynamic` over
// `ObjectMethods.bootstrap(Lo<Row>.class, "a;b", getField a, getField b)`
// (javac's shape; `null` for an `equals` site's second operand), boxed.
// Each site is run twice: the second failure is the recorded one (JVMS
// 5.4.3), a new error without a cause.
//
//   bad-name          method name `size`: the JDK's `IllegalArgumentException(methodName)`.
//                     Before wave 38 CratonVM raised an internal error Java
//                     cannot catch (every mode).
//   *-bad-type        a method type other than the one the name needs
//   tostring-names    a name list whose length is not the accessor count
//   *-ok              the controls
//
// The name row and the arity/return rows (`hashcode-bad-type`,
// `equals-bad-type`) are refused in every mode: the native record methods pop
// and push by the method NAME, whatever the site declares. The class-only
// type row (`tostring-bad-type`) and the name-list rows under `--jdk-only`
// (`--compatible` links them, as before; its lines for them are not
// recorded).
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints
// `[DBG_LAMBDA] link-check recorded java/lang/BootstrapMethodError for cp#N`
// once per refused row.
//
// Run: javac -d out L4W38ObjectMethodsSiteValidation.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W38ObjectMethodsSiteValidation
//
// Expected HotSpot 25 output (default and -Xint):
//   tostring-ok: LoToStringOk[a=1, b=x] | LoToStringOk[a=1, b=x]
//   equals-ok: false | false
//   bad-name: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: size | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   hashcode-bad-type: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Bad method type: (LoHashLong)long | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   equals-bad-type: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Bad method type: (LoEqualsArity)boolean | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   tostring-bad-type: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Bad method type: (Object)String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   tostring-names: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Name list and accessor list do not match | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   tostring-trailing-names: LoToStringTrail[a=1, b=x] | LoToStringTrail[a=1, b=x]
//   tostring-empty-names: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: Name list and accessor list do not match | java.lang.BootstrapMethodError: bootstrap method initialization exception
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.RecordAttribute;
import java.lang.classfile.attribute.RecordComponentInfo;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W38ObjectMethodsSiteValidation {
    static final ClassDesc OM = ClassDesc.of("java.lang.runtime.ObjectMethods");
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, OM,
            "bootstrap", MethodTypeDesc.of(ConstantDescs.CD_Object,
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.TypeDescriptor"), ConstantDescs.CD_Class,
                    ConstantDescs.CD_String, ClassDesc.of("java.lang.invoke.MethodHandle").arrayType()));
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc INT = ConstantDescs.CD_int;
    static final MethodTypeDesc CTOR = MethodTypeDesc.of(ConstantDescs.CD_void, INT, STR);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W38ObjectMethodsSiteValidation.class.getClassLoader());
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

    /** `type` names the record as `null` (the record itself) or any other descriptor. */
    static String row(String name, String method, ClassDesc ret, ClassDesc[] params, String names) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc[] ps = params.clone();
        for (int i = 0; i < ps.length; i++) {
            if (ps[i] == null) {
                ps[i] = self;
            }
        }
        MethodTypeDesc type = MethodTypeDesc.of(ret, ps);
        ConstantDesc[] args = {self, names,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, self, "a", INT),
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, self, "b", STR)};
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(BSM, method, type, args);
        byte[] b = ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withSuperclass(ClassDesc.of("java.lang.Record"));
            cb.with(RecordAttribute.of(RecordComponentInfo.of("a", INT), RecordComponentInfo.of("b", STR)));
            cb.withField("a", INT, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("b", STR, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withMethodBody(ConstantDescs.INIT_NAME, CTOR, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ClassDesc.of("java.lang.Record"), ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                    .aload(0).iload(1).putfield(self, "a", INT)
                    .aload(0).aload(2).putfield(self, "b", STR)
                    .return_());
            cb.withMethodBody("make", MethodTypeDesc.of(OBJ, OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> {
                        for (int i = 0; i < type.parameterCount(); i++) {
                            if (i == 0) {
                                code.aload(0).checkcast(type.parameterType(0));
                            } else {
                                code.aconst_null();
                            }
                        }
                        code.invokedynamic(site);
                        if (ret.equals(INT)) {
                            code.invokestatic(ConstantDescs.CD_Integer, "valueOf",
                                    MethodTypeDesc.of(ConstantDescs.CD_Integer, INT));
                        } else if (ret.equals(ConstantDescs.CD_long)) {
                            code.invokestatic(ConstantDescs.CD_Long, "valueOf",
                                    MethodTypeDesc.of(ConstantDescs.CD_Long, ConstantDescs.CD_long));
                        } else if (ret.equals(ConstantDescs.CD_boolean)) {
                            code.invokestatic(ConstantDescs.CD_Boolean, "valueOf",
                                    MethodTypeDesc.of(ConstantDescs.CD_Boolean, ConstantDescs.CD_boolean));
                        }
                        code.areturn();
                    });
        });
        Method make;
        Object instance;
        try {
            Class<?> c = LOADER.define(name, b);
            make = c.getMethod("make", Object.class);
            instance = c.getConstructor(int.class, String.class).newInstance(1, "x");
        } catch (Throwable t) {
            return "setup: " + t;
        }
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                out.append(make.invoke(null, instance));
            } catch (InvocationTargetException e) {
                out.append(describe(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    public static void main(String[] args) {
        ClassDesc[] self = {null};
        ClassDesc[] selfObj = {null, OBJ};
        System.out.println("tostring-ok: " + row("LoToStringOk", "toString", STR, self, "a;b"));
        System.out.println("equals-ok: " + row("LoEqualsOk", "equals", ConstantDescs.CD_boolean, selfObj, "a;b"));
        System.out.println("bad-name: " + row("LoBadName", "size", INT, self, "a;b"));
        System.out.println("hashcode-bad-type: " + row("LoHashLong", "hashCode", ConstantDescs.CD_long, self,
                "a;b"));
        System.out.println("equals-bad-type: " + row("LoEqualsArity", "equals", ConstantDescs.CD_boolean, self,
                "a;b"));
        System.out.println("tostring-bad-type: " + row("LoToStringObj", "toString", STR, new ClassDesc[] {OBJ},
                "a;b"));
        System.out.println("tostring-names: " + row("LoToStringNames", "toString", STR, self, "a"));
        System.out.println("tostring-trailing-names: " + row("LoToStringTrail", "toString", STR, self, "a;b;"));
        System.out.println("tostring-empty-names: " + row("LoToStringEmpty", "toString", STR, self, ""));
    }
}
