// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L4
// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`): an
// `ObjectMethods.bootstrap` `toString` site whose getters include accessor
// METHODS (`REF_invokeVirtual R.m()T`) renders what each accessor RETURNS,
// in getter order, and lets an accessor's exception through. Under
// `--jdk-only` CratonVM used to hand such a site to the JDK's own
// `ObjectMethods.makeToString`, whose chain needs `MethodHandle.copyWith`
// (LambdaForms): `BootstrapMethodError / RuntimeException: AbstractMethodError`.
//
// Each row generates (java.lang.classfile) a record-shaped class
// `Lo<Row>(int a, String b, double d)` whose accessors do NOT return the bare
// field (`a()` is `a * 10`, `b()` is `b + "!"`), so a row tells a called
// accessor from a field read; `boom()` throws `IllegalStateException`. Its
// static `make1(Object)` runs the site; each row runs it twice.
//
//   called        getters `a()`                -> the accessor's value
//   mixed         getters `b()`, `a` (field)   -> getter order, field + method
//   double        getters `d()`                -> a `double` return
//   throws        getters `a`, `boom()`        -> the accessor's exception
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 (default mode) prints, per row,
//   `[indy-all] object-methods toString cp#N: getter-driven (1 getters, 1 accessor method(s))`
// (`2 getters, 1 accessor method(s)` for `mixed` and `throws`). Since wave 44
// (`OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE` on, the JDK's own
// `ObjectMethods` links these sites) it prints
//   `[indy-all] object-methods toString cp#N: accessor-method getters take the jdk route`
// then `... jdk bootstrap (getters not modelled)`; the output is unchanged.
//
// Run: javac -d out L4W41ObjectMethodsAccessorToString.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W41ObjectMethodsAccessorToString
//
// Expected HotSpot 25 output (default and -Xint):
//   called: LoCalled[a=10] | LoCalled[a=10]
//   mixed: LoMixed[b=x!, a=1] | LoMixed[b=x!, a=1]
//   double: LoDouble[d=2.5] | LoDouble[d=2.5]
//   throws: java.lang.IllegalStateException: boom | java.lang.IllegalStateException: boom
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

public class L4W41ObjectMethodsAccessorToString {
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
    static final MethodTypeDesc CTOR = MethodTypeDesc.of(ConstantDescs.CD_void, INT, STR, DBL);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W41ObjectMethodsAccessorToString.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static ClassDesc typeOf(String component) {
        return switch (component) {
            case "a", "boom" -> INT;
            case "b" -> STR;
            default -> DBL;
        };
    }

    /** `x` is `REF_getField x`, `x()` the accessor method. */
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

    static byte[] record(String name, String names, String[] getterNames) {
        ClassDesc self = ClassDesc.of(name);
        ConstantDesc[] g = getters(self, getterNames);
        ConstantDesc[] args = new ConstantDesc[2 + g.length];
        args[0] = self;
        args[1] = names;
        System.arraycopy(g, 0, args, 2, g.length);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(OM_BSM, "toString", MethodTypeDesc.of(STR, self), args);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withSuperclass(ClassDesc.of("java.lang.Record"));
            cb.with(RecordAttribute.of(RecordComponentInfo.of("a", INT), RecordComponentInfo.of("b", STR),
                    RecordComponentInfo.of("d", DBL)));
            cb.withField("a", INT, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("b", STR, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withField("d", DBL, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.withMethodBody(ConstantDescs.INIT_NAME, CTOR, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ClassDesc.of("java.lang.Record"), ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                    .aload(0).iload(1).putfield(self, "a", INT)
                    .aload(0).aload(2).putfield(self, "b", STR)
                    .aload(0).dload(3).putfield(self, "d", DBL)
                    .return_());
            // a() = a * 10
            cb.withMethodBody("a", MethodTypeDesc.of(INT), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).getfield(self, "a", INT).bipush(10).imul().ireturn());
            // b() = b + "!"
            cb.withMethodBody("b", MethodTypeDesc.of(STR), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).getfield(self, "b", STR).ldc("!")
                            .invokevirtual(STR, "concat", MethodTypeDesc.of(STR, STR)).areturn());
            cb.withMethodBody("d", MethodTypeDesc.of(DBL), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).getfield(self, "d", DBL).dreturn());
            ClassDesc ise = ClassDesc.of("java.lang.IllegalStateException");
            cb.withMethodBody("boom", MethodTypeDesc.of(INT), ClassFile.ACC_PUBLIC,
                    code -> code.new_(ise).dup().ldc("boom")
                            .invokespecial(ise, ConstantDescs.INIT_NAME,
                                    MethodTypeDesc.of(ConstantDescs.CD_void, STR))
                            .athrow());
            cb.withMethodBody("make1", MethodTypeDesc.of(OBJ, OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).checkcast(self).invokedynamic(site).areturn());
        });
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
    }

    static String row(String name, String names, String... getterNames) {
        try {
            Class<?> c = LOADER.define(name, record(name, names, getterNames));
            Object x = c.getConstructor(int.class, String.class, double.class).newInstance(1, "x", 2.5);
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
        System.out.println("called: " + row("LoCalled", "a", "a()"));
        System.out.println("mixed: " + row("LoMixed", "b;a", "b()", "a"));
        System.out.println("double: " + row("LoDouble", "d", "d()"));
        System.out.println("throws: " + row("LoThrows", "a;boom", "a", "boom()"));
    }
}
