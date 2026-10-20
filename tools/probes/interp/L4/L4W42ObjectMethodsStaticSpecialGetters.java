// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L4
// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`, "What remains
// (wave 41)"): an `ObjectMethods.bootstrap` `toString` site whose getters are
// `REF_invokeStatic X.m(R)T` (of the record or of another class) or
// `REF_invokeSpecial R.m()T`, and getters whose handle type is not `(R)T`
// (which the JDK's `makeToString` refuses in `filterArguments` /
// `permuteArguments`). Under `--jdk-only` CratonVM handed every such site to
// the JDK's own `makeToString`, whose chain needs `MethodHandle.copyWith`
// (LambdaForms): `BootstrapMethodError / RuntimeException:
// AbstractMethodError`.
//
// Each row generates (java.lang.classfile) a record-shaped class
// `Lo<Row>(int a, String b, double d)` whose accessor `a()` is `a * 10`, a
// static `sa(R)I` is `a * 100`, a static `boom(R)I` throws
// `IllegalStateException("boom")`, and a helper class `Lo<Row>H` with a static
// `hb(R)String` answering `b + "?"`. `open` rows make the record non-final
// with a subclass `Lo<Row>Sub` overriding `a()` to answer -1, and run the
// site on a subclass instance (so `REF_invokeSpecial` tells itself from a
// virtual call). Its static `make1(Object)` runs the site; each row runs it
// twice. An error prints its cause chain (`class: message <- cause ...`).
//
// Getter notation: `a` field, `a()` virtual, `sa(R)` / `boom(R)` / `so(O)` /
// `sv(R)` / `s2(RR)` static of the record, `H.hb(R)` static of the helper,
// `sp:a()` special, `gs:K` static field, `O.getClass()` Object's method.
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 (default mode) prints, for row
// `static-own`, `[indy-all] object-methods toString cp#N: getter-driven (1
// getters, 1 accessor method(s))` (`5 getters, 4 accessor method(s)` for
// `mixed`), and for row `wrong-param` `[indy-all] object-methods toString
// cp#N: getter refused: parameter types do not match after reorder:
// (Object)String, (LoWrongParam)String`. Before wave 42 the rows took the
// JDK's route: `[indy-all] object-methods toString cp#N: jdk bootstrap
// (getters not modelled)`.
//
// Run: javac -d out L4W42ObjectMethodsStaticSpecialGetters.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W42ObjectMethodsStaticSpecialGetters
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   static-own: LoStaticOwn[a=100] | LoStaticOwn[a=100]
//   static-other: LoStaticOther[b=x?] | LoStaticOther[b=x?]
//   special: LoSpecial[a=10] | LoSpecial[a=10]
//   special-open: LoSpecialOpen[a=10] | LoSpecialOpen[a=10]
//   virtual-open: LoVirtualOpen[a=-1] | LoVirtualOpen[a=-1]
//   mixed: LoMixed[s=100, f=x, v=2.5, p=10, h=x?] | LoMixed[s=100, f=x, v=2.5, p=10, h=x?]
//   static-throws: java.lang.IllegalStateException: boom | java.lang.IllegalStateException: boom
//   wrong-param: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.RuntimeException: java.lang.IllegalArgumentException: parameter types do not match after reorder: (Object)String, (LoWrongParam)String <- java.lang.IllegalArgumentException: parameter types do not match after reorder: (Object)String, (LoWrongParam)String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   void-return: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.IllegalArgumentException: parameter type cannot be void | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   two-params: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.RuntimeException: java.lang.IllegalArgumentException: target and filter types do not match: (int)String, (LoTwoParams,LoTwoParams)int <- java.lang.IllegalArgumentException: target and filter types do not match: (int)String, (LoTwoParams,LoTwoParams)int | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   static-field: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.RuntimeException: java.lang.IllegalArgumentException: target and filter types do not match: (int)String, ()int <- java.lang.IllegalArgumentException: target and filter types do not match: (int)String, ()int | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   object-method: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.RuntimeException: java.lang.IllegalArgumentException: parameter types do not match after reorder: (Object)String, (LoObjectMethod)String <- java.lang.IllegalArgumentException: parameter types do not match after reorder: (Object)String, (LoObjectMethod)String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   wrong-after-good: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.RuntimeException: java.lang.IllegalArgumentException: parameter types do not match after reorder: (LoWrongAfterGood,Object)String, (LoWrongAfterGood)String <- java.lang.IllegalArgumentException: parameter types do not match after reorder: (LoWrongAfterGood,Object)String, (LoWrongAfterGood)String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//
// `--compatible` is not recorded (by design it keeps the positional reading
// for a getter that is not a field of the record).
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

public class L4W42ObjectMethodsStaticSpecialGetters {
    static final ClassDesc OM = ClassDesc.of("java.lang.runtime.ObjectMethods");
    static final DirectMethodHandleDesc OM_BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, OM,
            "bootstrap", MethodTypeDesc.of(ConstantDescs.CD_Object,
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.TypeDescriptor"), ConstantDescs.CD_Class,
                    ConstantDescs.CD_String, ClassDesc.of("java.lang.invoke.MethodHandle").arrayType()));
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc INT = ConstantDescs.CD_int;
    static final ClassDesc DBL = ConstantDescs.CD_double;
    static final ClassDesc VOID = ConstantDescs.CD_void;
    static final MethodTypeDesc CTOR = MethodTypeDesc.of(VOID, INT, STR, DBL);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W42ObjectMethodsStaticSpecialGetters.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static ClassDesc typeOf(String component) {
        return switch (component) {
            case "a" -> INT;
            case "b" -> STR;
            default -> DBL;
        };
    }

    static ConstantDesc getter(ClassDesc self, ClassDesc helper, String n) {
        return switch (n) {
            case "sa(R)" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "sa",
                    MethodTypeDesc.of(INT, self));
            case "boom(R)" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "boom",
                    MethodTypeDesc.of(INT, self));
            case "so(O)" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "so",
                    MethodTypeDesc.of(INT, OBJ));
            case "sv(R)" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "sv",
                    MethodTypeDesc.of(VOID, self));
            case "s2(RR)" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "s2",
                    MethodTypeDesc.of(INT, self, self));
            case "H.hb(R)" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, helper, "hb",
                    MethodTypeDesc.of(STR, self));
            case "sp:a()" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.SPECIAL, self, "a",
                    MethodTypeDesc.of(INT));
            case "gs:K" -> MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_GETTER, self, "K", INT);
            case "O.getClass()" -> MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, OBJ, "getClass",
                    MethodTypeDesc.of(ConstantDescs.CD_Class));
            default -> n.endsWith("()")
                    ? MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, self,
                            n.substring(0, n.length() - 2), MethodTypeDesc.of(typeOf(n.substring(0, 1))))
                    : MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, self, n, typeOf(n));
        };
    }

    static byte[] record(String name, boolean open, String names, String[] getterNames) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc helper = ClassDesc.of(name + "H");
        ConstantDesc[] args = new ConstantDesc[2 + getterNames.length];
        args[0] = self;
        args[1] = names;
        for (int i = 0; i < getterNames.length; i++) {
            args[2 + i] = getter(self, helper, getterNames[i]);
        }
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(OM_BSM, "toString", MethodTypeDesc.of(STR, self), args);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | (open ? 0 : ClassFile.ACC_FINAL) | ClassFile.ACC_SUPER);
            cb.withSuperclass(ClassDesc.of("java.lang.Record"));
            cb.with(RecordAttribute.of(RecordComponentInfo.of("a", INT), RecordComponentInfo.of("b", STR),
                    RecordComponentInfo.of("d", DBL)));
            cb.withField("a", INT, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("b", STR, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("d", DBL, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("K", INT, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC | ClassFile.ACC_FINAL);
            cb.withMethodBody(ConstantDescs.INIT_NAME, CTOR, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ClassDesc.of("java.lang.Record"), ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                    .aload(0).iload(1).putfield(self, "a", INT)
                    .aload(0).aload(2).putfield(self, "b", STR)
                    .aload(0).dload(3).putfield(self, "d", DBL)
                    .return_());
            // a() = a * 10
            cb.withMethodBody("a", MethodTypeDesc.of(INT), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).getfield(self, "a", INT).bipush(10).imul().ireturn());
            cb.withMethodBody("b", MethodTypeDesc.of(STR), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).getfield(self, "b", STR).areturn());
            cb.withMethodBody("d", MethodTypeDesc.of(DBL), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).getfield(self, "d", DBL).dreturn());
            // static sa(R) = r.a * 100
            cb.withMethodBody("sa", MethodTypeDesc.of(INT, self), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).getfield(self, "a", INT).bipush(100).imul().ireturn());
            cb.withMethodBody("so", MethodTypeDesc.of(INT, OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.bipush(7).ireturn());
            cb.withMethodBody("sv", MethodTypeDesc.of(VOID, self), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.return_());
            cb.withMethodBody("s2", MethodTypeDesc.of(INT, self, self), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.bipush(2).ireturn());
            ClassDesc ise = ClassDesc.of("java.lang.IllegalStateException");
            cb.withMethodBody("boom", MethodTypeDesc.of(INT, self), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.new_(ise).dup().ldc("boom")
                            .invokespecial(ise, ConstantDescs.INIT_NAME, MethodTypeDesc.of(VOID, STR))
                            .athrow());
            cb.withMethodBody("make1", MethodTypeDesc.of(OBJ, OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).checkcast(self).invokedynamic(site).areturn());
        });
    }

    /** `Lo<Row>H.hb(R)` = `r.b() + "?"`. */
    static byte[] helper(String name) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc h = ClassDesc.of(name + "H");
        return ClassFile.of().build(h, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("hb", MethodTypeDesc.of(STR, self), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).invokevirtual(self, "b", MethodTypeDesc.of(STR)).ldc("?")
                            .invokevirtual(STR, "concat", MethodTypeDesc.of(STR, STR)).areturn());
        });
    }

    /** `Lo<Row>Sub extends Lo<Row>`, `a()` = -1. */
    static byte[] subclass(String name) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc sub = ClassDesc.of(name + "Sub");
        return ClassFile.of().build(sub, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(self);
            cb.withMethodBody(ConstantDescs.INIT_NAME, CTOR, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .iload(1).aload(2).dload(3).invokespecial(self, ConstantDescs.INIT_NAME, CTOR).return_());
            cb.withMethodBody("a", MethodTypeDesc.of(INT), ClassFile.ACC_PUBLIC,
                    code -> code.iconst_m1().ireturn());
        });
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (c != t) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName());
            if (c.getMessage() != null) {
                sb.append(": ").append(c.getMessage());
            }
        }
        return sb.toString();
    }

    static String row(String name, boolean open, String names, String... getterNames) {
        try {
            Class<?> c = LOADER.define(name, record(name, open, names, getterNames));
            LOADER.define(name + "H", helper(name));
            Class<?> target = open ? LOADER.define(name + "Sub", subclass(name)) : c;
            Object x = target.getConstructor(int.class, String.class, double.class).newInstance(1, "x", 2.5);
            Method make = c.getMethod("make1", Object.class);
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 2; i++) {
                if (i > 0) {
                    out.append(" | ");
                }
                try {
                    out.append(make.invoke(null, x));
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
        System.out.println("static-own: " + row("LoStaticOwn", false, "a", "sa(R)"));
        System.out.println("static-other: " + row("LoStaticOther", false, "b", "H.hb(R)"));
        System.out.println("special: " + row("LoSpecial", false, "a", "sp:a()"));
        System.out.println("special-open: " + row("LoSpecialOpen", true, "a", "sp:a()"));
        System.out.println("virtual-open: " + row("LoVirtualOpen", true, "a", "a()"));
        System.out.println("mixed: " + row("LoMixed", false, "s;f;v;p;h", "sa(R)", "b", "d()", "sp:a()", "H.hb(R)"));
        System.out.println("static-throws: " + row("LoStaticThrows", false, "a;x", "a", "boom(R)"));
        System.out.println("wrong-param: " + row("LoWrongParam", false, "a", "so(O)"));
        System.out.println("void-return: " + row("LoVoidReturn", false, "a", "sv(R)"));
        System.out.println("two-params: " + row("LoTwoParams", false, "a", "s2(RR)"));
        System.out.println("static-field: " + row("LoStaticField", false, "a", "gs:K"));
        System.out.println("object-method: " + row("LoObjectMethod", false, "a", "O.getClass()"));
        System.out.println("wrong-after-good: " + row("LoWrongAfterGood", false, "a;b", "sa(R)", "so(O)"));
    }
}
