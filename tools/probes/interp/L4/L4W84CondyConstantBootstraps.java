// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L4: `ldc` of dynamic constants
// bootstrapped by `java.lang.invoke.ConstantBootstraps` (several of which the
// interpreter answers itself, `constants.rs::condy_fast_answer`), plus the
// caching of a dynamic constant per constant-pool entry. Complements
// `L4W43CondyResultConversion` (an own bootstrap's answer conversions) and
// `L4W29CondyStaticReads`.
//
// Each row generates (java.lang.classfile API) a class whose static `get()` is
// one `ldc` of the row's constant, runs it twice and prints both answers
// ("value (SimpleClassName)") or the thrown chain of exception CLASSES
// (outermost first; messages are left out on purpose -- the message texts of
// the JDK-internal causes are not what these rows test). A recorded
// resolution error must come back with the same chain on the second run.
// `shared-entry` / `two-entries` run a method with two `ldc`s: one entry twice
// (one bootstrap call, one object) or two entries (two calls, two objects).
//
// Run: javac -d . L4W84CondyConstantBootstraps.java
//      cratonvm --jdk-only [--nojit] -cp . L4W84CondyConstantBootstraps
//      cratonvm --compatible [--nojit] -cp . L4W84CondyConstantBootstraps
//
// Expected output derived from ConstantBootstraps.java, BootstrapMethodInvoker.java,
// MethodHandleNatives.mapLookupExceptionToError / initCauseFrom and
// ValueConversions.primitiveConversion (JDK 25.0.3 src.zip). A recorded condy
// failure keeps its cause class on the second run (ConstantPool's condy path
// records the cause; L4W43CondyResultConversion measured that).
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4)
// own-null-object: null (null) | null (null) | calls=1
// nullConstant-String: null (null) | null (null)
// nullConstant-int: java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException | java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException
// primitiveClass-I: int (Class) | int (Class)
// primitiveClass-V: void (Class) | void (Class)
// primitiveClass-X: java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException | java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException
// primitiveClass-II: java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException | java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException
// primitiveClass-as-Object: java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException | java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException
// enumConstant-RUNNABLE: RUNNABLE (State) | RUNNABLE (State) | same=true
// enumConstant-NOPE: java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException | java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException
// getStaticFinal-MAX_VALUE: 2147483647 (Integer) | 2147483647 (Integer)
// getStaticFinal-TRUE: true (Boolean) | true (Boolean)
// getStaticFinal-nonfinal: java.lang.IncompatibleClassChangeError | java.lang.IncompatibleClassChangeError
// getStaticFinal-missing: java.lang.NoSuchFieldError | java.lang.NoSuchFieldError
// explicitCast-long-to-int: 5 (Integer) | 5 (Integer)
// explicitCast-int-to-boolean: true (Boolean) | true (Boolean)
// explicitCast-null-to-int: 0 (Integer) | 0 (Integer)
// explicitCast-string-to-int: java.lang.BootstrapMethodError <- java.lang.ClassCastException | java.lang.BootstrapMethodError <- java.lang.ClassCastException
// explicitCast-to-Object: s (String) | s (String)
// invoke-valueOf: 42 (String) | 42 (String)
// invoke-parseInt-as-long: 7 (Long) | 7 (Long)
// arrayVarHandle: VarHandle varType=int coords=[class [I, int] | VarHandle varType=int coords=[class [I, int]
// fieldVarHandle: VarHandle varType=int coords=[class L4W84CondyConstantBootstraps$Holder] | VarHandle varType=int coords=[class L4W84CondyConstantBootstraps$Holder]
// staticFieldVarHandle: VarHandle varType=long coords=[] | VarHandle varType=long coords=[]
// fieldVarHandle-as-Object: java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException | java.lang.BootstrapMethodError <- java.lang.IllegalArgumentException
// shared-entry: true | true | calls=1
// two-entries: false | false | calls=2
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicConstantDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W84CondyConstantBootstraps {
    static final ClassDesc SELF = ClassDesc.of("L4W84CondyConstantBootstraps");
    static final ClassDesc HOLDER = ClassDesc.of("L4W84CondyConstantBootstraps$Holder");
    static final DirectMethodHandleDesc OWN = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "bsm", MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_MethodHandles_Lookup,
                    ConstantDescs.CD_String, ConstantDescs.CD_Class));

    public static class Holder {
        public int inst;
        public static long stat;
    }

    public static Object NONFINAL = "nf";

    static int calls;

    public static Object bsm(MethodHandles.Lookup lookup, String name, Class<?> type) {
        calls++;
        if (name.startsWith("fresh")) {
            return new int[1];
        }
        return null;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W84CondyConstantBootstraps.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();
    static int classes;

    static String chain(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (sb.length() > 0) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName());
        }
        return sb.toString();
    }

    static String show(Object v) {
        if (v instanceof VarHandle vh) {
            return "VarHandle varType=" + vh.varType().getName() + " coords=" + vh.coordinateTypes();
        }
        return v + " (" + (v == null ? "null" : v.getClass().getSimpleName()) + ")";
    }

    static Method build(ClassDesc type, ConstantDesc constant) throws Exception {
        String cls = "Cb" + (classes++);
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("get", MethodTypeDesc.of(type), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                code.ldc(constant);
                if (!type.isPrimitive()) {
                    code.areturn();
                } else if (type.equals(ConstantDescs.CD_long)) {
                    code.lreturn();
                } else if (type.equals(ConstantDescs.CD_double)) {
                    code.dreturn();
                } else if (type.equals(ConstantDescs.CD_float)) {
                    code.freturn();
                } else {
                    code.ireturn();
                }
            });
        });
        return LOADER.define(cls, b).getMethod("get");
    }

    static String twice(Method get, boolean same) {
        StringBuilder out = new StringBuilder();
        Object first = null;
        Object second = null;
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                Object v = get.invoke(null);
                if (i == 0) first = v; else second = v;
                out.append(show(v));
            } catch (InvocationTargetException e) {
                out.append(chain(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        if (same) {
            out.append(" | same=").append(first == second);
        }
        return out.toString();
    }

    static void row(String name, ClassDesc type, ConstantDesc constant) {
        row(name, type, constant, false);
    }

    static void row(String name, ClassDesc type, ConstantDesc constant, boolean same) {
        String out;
        try {
            out = twice(build(type, constant), same);
        } catch (Throwable t) {
            out = "setup: " + t;
        }
        System.out.println(name + ": " + out);
    }

    /** A method `same()` comparing two `ldc`s by identity. */
    static Method pair(ConstantDesc a, ConstantDesc b) throws Exception {
        String cls = "Cb" + (classes++);
        byte[] bytes = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("same", MethodTypeDesc.of(ConstantDescs.CD_boolean),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                        Label ne = code.newLabel();
                        code.ldc(a);
                        code.ldc(b);
                        code.if_acmpne(ne);
                        code.iconst_1();
                        code.ireturn();
                        code.labelBinding(ne);
                        code.iconst_0();
                        code.ireturn();
                    });
        });
        return LOADER.define(cls, bytes).getMethod("same");
    }

    static void pairRow(String name, ConstantDesc a, ConstantDesc b) {
        String out;
        try {
            Method m = pair(a, b);
            int before = calls;
            Object x = m.invoke(null);
            Object y = m.invoke(null);
            out = x + " | " + y + " | calls=" + (calls - before);
        } catch (InvocationTargetException e) {
            out = chain(e.getCause());
        } catch (Throwable t) {
            out = "setup: " + t;
        }
        System.out.println(name + ": " + out);
    }

    static DynamicConstantDesc<Object> cb(DirectMethodHandleDesc bsm, String name, ClassDesc type,
            ConstantDesc... args) {
        return DynamicConstantDesc.ofNamed(bsm, name, type, args);
    }

    public static void main(String[] args) {
        ClassDesc cdClass = ConstantDescs.CD_Class;
        ClassDesc cdVh = ConstantDescs.CD_VarHandle;

        int before = calls;
        String own = "own-null-object: ";
        try {
            own += twice(build(ConstantDescs.CD_Object, cb(OWN, "null", ConstantDescs.CD_Object)), false);
        } catch (Throwable t) {
            own += "setup: " + t;
        }
        System.out.println(own + " | calls=" + (calls - before));

        row("nullConstant-String", ConstantDescs.CD_String,
                cb(ConstantDescs.BSM_NULL_CONSTANT, "_", ConstantDescs.CD_String));
        row("nullConstant-int", ConstantDescs.CD_int,
                cb(ConstantDescs.BSM_NULL_CONSTANT, "_", ConstantDescs.CD_int));
        row("primitiveClass-I", cdClass, cb(ConstantDescs.BSM_PRIMITIVE_CLASS, "I", cdClass));
        row("primitiveClass-V", cdClass, cb(ConstantDescs.BSM_PRIMITIVE_CLASS, "V", cdClass));
        row("primitiveClass-X", cdClass, cb(ConstantDescs.BSM_PRIMITIVE_CLASS, "X", cdClass));
        row("primitiveClass-II", cdClass, cb(ConstantDescs.BSM_PRIMITIVE_CLASS, "II", cdClass));
        row("primitiveClass-as-Object", ConstantDescs.CD_Object,
                cb(ConstantDescs.BSM_PRIMITIVE_CLASS, "I", ConstantDescs.CD_Object));

        ClassDesc state = ClassDesc.of("java.lang.Thread$State");
        row("enumConstant-RUNNABLE", state, cb(ConstantDescs.BSM_ENUM_CONSTANT, "RUNNABLE", state), true);
        row("enumConstant-NOPE", state, cb(ConstantDescs.BSM_ENUM_CONSTANT, "NOPE", state));

        row("getStaticFinal-MAX_VALUE", ConstantDescs.CD_int,
                cb(ConstantDescs.BSM_GET_STATIC_FINAL, "MAX_VALUE", ConstantDescs.CD_int, ConstantDescs.CD_Integer));
        ClassDesc cdBoolean = ConstantDescs.CD_Boolean;
        // The 3-argument form (no declaring class: the field type is it);
        // `ConstantDescs.BSM_GET_STATIC_FINAL` names the 4-argument one.
        DirectMethodHandleDesc getStaticFinal3 = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ConstantDescs.CD_ConstantBootstraps, "getStaticFinal",
                MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_Class));
        row("getStaticFinal-TRUE", cdBoolean, cb(getStaticFinal3, "TRUE", cdBoolean));
        row("getStaticFinal-nonfinal", ConstantDescs.CD_Object,
                cb(ConstantDescs.BSM_GET_STATIC_FINAL, "NONFINAL", ConstantDescs.CD_Object, SELF));
        row("getStaticFinal-missing", ConstantDescs.CD_int,
                cb(ConstantDescs.BSM_GET_STATIC_FINAL, "NOPE", ConstantDescs.CD_int, ConstantDescs.CD_Integer));

        row("explicitCast-long-to-int", ConstantDescs.CD_int,
                cb(ConstantDescs.BSM_EXPLICIT_CAST, "_", ConstantDescs.CD_int, 5L));
        row("explicitCast-int-to-boolean", ConstantDescs.CD_boolean,
                cb(ConstantDescs.BSM_EXPLICIT_CAST, "_", ConstantDescs.CD_boolean, 3));
        row("explicitCast-null-to-int", ConstantDescs.CD_int,
                cb(ConstantDescs.BSM_EXPLICIT_CAST, "_", ConstantDescs.CD_int, ConstantDescs.NULL));
        row("explicitCast-string-to-int", ConstantDescs.CD_int,
                cb(ConstantDescs.BSM_EXPLICIT_CAST, "_", ConstantDescs.CD_int, "s"));
        row("explicitCast-to-Object", ConstantDescs.CD_Object,
                cb(ConstantDescs.BSM_EXPLICIT_CAST, "_", ConstantDescs.CD_Object, "s"));

        DirectMethodHandleDesc valueOf = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ConstantDescs.CD_String, "valueOf", MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_int));
        row("invoke-valueOf", ConstantDescs.CD_String,
                cb(ConstantDescs.BSM_INVOKE, "_", ConstantDescs.CD_String, valueOf, 42));
        DirectMethodHandleDesc parseInt = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ConstantDescs.CD_Integer, "parseInt", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_String));
        row("invoke-parseInt-as-long", ConstantDescs.CD_long,
                cb(ConstantDescs.BSM_INVOKE, "_", ConstantDescs.CD_long, parseInt, "7"));

        row("arrayVarHandle", cdVh,
                cb(ConstantDescs.BSM_VARHANDLE_ARRAY, "_", cdVh, ConstantDescs.CD_int.arrayType()));
        row("fieldVarHandle", cdVh,
                cb(ConstantDescs.BSM_VARHANDLE_FIELD, "inst", cdVh, HOLDER, ConstantDescs.CD_int));
        row("staticFieldVarHandle", cdVh,
                cb(ConstantDescs.BSM_VARHANDLE_STATIC_FIELD, "stat", cdVh, HOLDER, ConstantDescs.CD_long));
        row("fieldVarHandle-as-Object", ConstantDescs.CD_Object,
                cb(ConstantDescs.BSM_VARHANDLE_FIELD, "inst", ConstantDescs.CD_Object, HOLDER, ConstantDescs.CD_int));

        DynamicConstantDesc<Object> fresh = cb(OWN, "fresh", ConstantDescs.CD_Object);
        pairRow("shared-entry", fresh, fresh);
        pairRow("two-entries", cb(OWN, "fresh1", ConstantDescs.CD_Object), cb(OWN, "fresh2", ConstantDescs.CD_Object));
    }
}
