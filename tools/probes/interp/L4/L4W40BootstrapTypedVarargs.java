// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L4
// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`, the
// `accessor-getter` row): a bootstrap method whose varargs parameter is an
// array of a type other than `Object` (`String...`, and
// `ObjectMethods.bootstrap`'s `MethodHandle... getters`) receives its
// trailing static arguments COLLECTED into that array, as
// `invokeWithArguments` does. CratonVM collected only into `Object[]` and
// passed the bare first argument otherwise, so the JDK's own `ObjectMethods`
// route (`--jdk-only`, a getter the native linkage does not model: an
// accessor METHOD) failed with `BootstrapMethodError /
// ClassCastException`.
//
//   varargs-*      a user bootstrap `(Lookup, String, MethodType, String...)`
//                  answering `<joined parts>:<array class simple name>`;
//                  zero, one and two static arguments (every mode)
//   accessor-*     a generated record `Lo<Row>(int a, String b, boolean c)`
//                  whose site's getters include the accessor METHOD `b()`;
//                  each site runs twice
//
// The two `toString` rows were removed by the wave-40 orchestrator (on the
// host the JDK's `ObjectMethods.makeToString` chain failed with
// `BootstrapMethodError / RuntimeException: AbstractMethodError:
// MethodHandle.copyWith(MethodType, LambdaForm) has no Code attribute`) and
// restored in wave 41, lane L4: under `--jdk-only` a `toString` site whose
// getters are the record's fields and accessor METHODS now links natively,
// calling each accessor (`i38-L4-...`, Progress (wave 41)). More rows:
// `L4W41ObjectMethodsAccessorToString`.
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 prints
//   `[indy-all] bootstrap L4W40BootstrapTypedVarargs.bsm: collects 2 trailing argument(s) into java/lang/String[]`
// (varargs-two) and, under `--jdk-only`,
//   `[indy-all] bootstrap java/lang/runtime/ObjectMethods.bootstrap: collects 1 trailing argument(s) into java/lang/invoke/MethodHandle[]`
//
// Run: javac -d out L4W40BootstrapTypedVarargs.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W40BootstrapTypedVarargs
//
// Expected HotSpot 25 output (default and -Xint):
//   varargs-none: :String[]
//   varargs-one: p:String[]
//   varargs-two: p+q:String[]
//   accessor-tostring: LoAccessor[b=x] | LoAccessor[b=x]
//   accessor-mixed-tostring: LoAccessorMixed[a=1, b=x] | LoAccessorMixed[a=1, b=x]
//   accessor-hashcode: 120 | 120
//   accessor-equals: false | false
//
// `--compatible` (by design: a getter that is not a field of the record keeps
// the positional reading, `i38-L4-...` Progress wave 39) is not recorded for
// the accessor-* rows.
import java.lang.classfile.ClassBuilder;
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
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W40BootstrapTypedVarargs {
    static final ClassDesc OM = ClassDesc.of("java.lang.runtime.ObjectMethods");
    static final DirectMethodHandleDesc OM_BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, OM,
            "bootstrap", MethodTypeDesc.of(ConstantDescs.CD_Object,
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.TypeDescriptor"), ConstantDescs.CD_Class,
                    ConstantDescs.CD_String, ClassDesc.of("java.lang.invoke.MethodHandle").arrayType()));
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc INT = ConstantDescs.CD_int;
    static final ClassDesc BOOL = ConstantDescs.CD_boolean;
    static final MethodTypeDesc CTOR = MethodTypeDesc.of(ConstantDescs.CD_void, INT, STR, BOOL);

    /** The user bootstrap: a `String...` varargs parameter. */
    public static CallSite bsm(MethodHandles.Lookup lookup, String name, MethodType type, String... parts) {
        String text = String.join("+", parts) + ":" + parts.getClass().getSimpleName();
        return new ConstantCallSite(MethodHandles.constant(String.class, text));
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W40BootstrapTypedVarargs.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String varargs(String name, String... parts) {
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ClassDesc.of("L4W40BootstrapTypedVarargs"), "bsm",
                MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                        ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), STR,
                        ClassDesc.of("java.lang.invoke.MethodType"), STR.arrayType()));
        ConstantDesc[] args = parts;
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm, "x", MethodTypeDesc.of(STR), args);
        byte[] b = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(STR), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(site).areturn());
        });
        try {
            return String.valueOf(LOADER.define(name, b).getMethod("make").invoke(null));
        } catch (InvocationTargetException e) {
            return describe(e.getCause());
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    static ClassDesc typeOf(String component) {
        return switch (component) {
            case "a" -> INT;
            case "b" -> STR;
            default -> BOOL;
        };
    }

    /** `a` is `REF_getField a`, `b()` the accessor method. */
    static ConstantDesc[] getters(ClassDesc owner, String... names) {
        ConstantDesc[] out = new ConstantDesc[names.length];
        for (int i = 0; i < names.length; i++) {
            String n = names[i];
            if (n.endsWith("()")) {
                String m = n.substring(0, n.length() - 2);
                out[i] = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, owner, m,
                        MethodTypeDesc.of(typeOf(m)));
            } else {
                out[i] = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, owner, n, typeOf(n));
            }
        }
        return out;
    }

    static DynamicCallSiteDesc site(ClassDesc record, String method, String names, ConstantDesc[] getters) {
        MethodTypeDesc type = switch (method) {
            case "equals" -> MethodTypeDesc.of(BOOL, record, OBJ);
            case "hashCode" -> MethodTypeDesc.of(INT, record);
            default -> MethodTypeDesc.of(STR, record);
        };
        ConstantDesc[] args = new ConstantDesc[2 + getters.length];
        args[0] = record;
        args[1] = names;
        System.arraycopy(getters, 0, args, 2, getters.length);
        return DynamicCallSiteDesc.of(OM_BSM, method, type, args);
    }

    static void addMake(ClassBuilder cb, ClassDesc record, String method, DynamicCallSiteDesc site) {
        boolean two = method.equals("equals");
        MethodTypeDesc mt = two ? MethodTypeDesc.of(OBJ, OBJ, OBJ) : MethodTypeDesc.of(OBJ, OBJ);
        cb.withMethodBody(two ? "make2" : "make1", mt, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
            code.aload(0).checkcast(record);
            if (two) {
                code.aload(1);
            }
            code.invokedynamic(site);
            if (method.equals("hashCode")) {
                code.invokestatic(ConstantDescs.CD_Integer, "valueOf",
                        MethodTypeDesc.of(ConstantDescs.CD_Integer, INT));
            } else if (two) {
                code.invokestatic(ConstantDescs.CD_Boolean, "valueOf",
                        MethodTypeDesc.of(ConstantDescs.CD_Boolean, BOOL));
            }
            code.areturn();
        });
    }

    static byte[] record(String name, String method, String names, String[] getterNames) {
        ClassDesc self = ClassDesc.of(name);
        DynamicCallSiteDesc site = site(self, method, names, getters(self, getterNames));
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withSuperclass(ClassDesc.of("java.lang.Record"));
            cb.with(RecordAttribute.of(RecordComponentInfo.of("a", INT), RecordComponentInfo.of("b", STR),
                    RecordComponentInfo.of("c", BOOL)));
            cb.withField("a", INT, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("b", STR, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("c", BOOL, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withMethodBody(ConstantDescs.INIT_NAME, CTOR, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ClassDesc.of("java.lang.Record"), ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                    .aload(0).iload(1).putfield(self, "a", INT)
                    .aload(0).aload(2).putfield(self, "b", STR)
                    .aload(0).iload(3).putfield(self, "c", BOOL)
                    .return_());
            for (String c : new String[] {"a", "b", "c"}) {
                ClassDesc t = typeOf(c);
                cb.withMethodBody(c, MethodTypeDesc.of(t), ClassFile.ACC_PUBLIC, code -> {
                    code.aload(0).getfield(self, c, t);
                    if (t.equals(STR)) {
                        code.areturn();
                    } else {
                        code.ireturn();
                    }
                });
            }
            addMake(cb, self, method, site);
        });
    }

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + (c == null ? "" : " / " + c.getClass().getName() + ": " + c.getMessage());
    }

    static String row(String name, String method, String names, String... getterNames) {
        try {
            Class<?> c = LOADER.define(name, record(name, method, names, getterNames));
            var ctor = c.getConstructor(int.class, String.class, boolean.class);
            Object x = ctor.newInstance(1, "x", true);
            Object y = ctor.newInstance(1, "y", true);
            Method make = method.equals("equals") ? c.getMethod("make2", Object.class, Object.class)
                    : c.getMethod("make1", Object.class);
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 2; i++) {
                if (i > 0) {
                    out.append(" | ");
                }
                try {
                    out.append(method.equals("equals") ? make.invoke(null, x, y) : make.invoke(null, x));
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
        System.out.println("varargs-none: " + varargs("LoVarargsNone"));
        System.out.println("varargs-one: " + varargs("LoVarargsOne", "p"));
        System.out.println("varargs-two: " + varargs("LoVarargsTwo", "p", "q"));
        System.out.println("accessor-tostring: " + row("LoAccessor", "toString", "b", "b()"));
        System.out.println("accessor-mixed-tostring: " + row("LoAccessorMixed", "toString", "a;b", "a", "b()"));
        System.out.println("accessor-hashcode: " + row("LoAccessorHash", "hashCode", "b", "b()"));
        System.out.println("accessor-equals: " + row("LoAccessorEq", "equals", "b", "b()"));
    }
}
