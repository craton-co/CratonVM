// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L4
// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`): an
// `ObjectMethods.bootstrap` site folds over the GETTERS it is given, as the
// JDK's `makeEquals` / `makeHashCode` / `makeToString` do, not over the
// record's own components; the getters resolve from the calling class first.
//
// Each row generates, with the java.lang.classfile API, a record
// `Lo<Row>(int a, String b, boolean c)` (fields in that order, accessors
// `a()`, `b()`, `c()`) holding a static `make1(Object)` / `make2(Object,
// Object)` that is one `invokedynamic` over `ObjectMethods.bootstrap(Lo<Row>,
// names, getters...)`, and, for the `own-*` rows, the record's own
// `hashCode`/`equals` with javac's body shape (`aload_0 [aload_1]
// invokedynamic ireturn`) over the same odd getters, called through
// `Object.hashCode`/`Object.equals` (the record intrinsic's door). Instances:
// `x = (1, "x", true)`, `y = (1, "y", true)`. Each `make` site runs twice (a
// linkage failure is recorded; the second is the recorded error, no cause).
//
//   *-reorder / *-subset   getters in another order / a subset: folded over the
//                          getters (CratonVM walked the record's components;
//                          `toString` read positional slots)
//   hash-boolean           a lone `boolean` getter hashes `Boolean.hashCode`
//   own-*                  the intrinsic door: the class-level "javac body"
//                          recogniser now also requires javac's getters
//   foreign-private        a non-nest class linking over another record's
//                          private fields: `IllegalAccessError` (cause
//                          `IllegalAccessException`) resolving the getter,
//                          before the bootstrap runs (`--jdk-only`)
//   null-component         javac record `P(Object o)`: `Objects.equals(o, null)`
//                          calls `o.equals(null)` (every mode)
//   null-this-*            javac's getters, a NULL receiver (make1(null),
//                          make2(null, null | x)): the getters dereference it
//                          (`NullPointerException`); `equals(null, null)` is
//                          the same object. CratonVM answered 0, "null", false.
//   javac-shape            control
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 prints
//   `[indy-all] object-methods hashCode cp#N: getter-driven (2 getters)` (hash-reorder)
//
// A row with a getter that is the accessor METHOD `b()` (HotSpot:
// `LoAccessor[b=x] | LoAccessor[b=x]`) was removed after the wave-39 host
// run: CratonVM's route to the JDK's own `ObjectMethods` fails there (see
// `i38-L4-objectmethods-native-linkage-ignores-its-getters`, What remains).
//
// Run: javac -d out L4W39ObjectMethodsGetters.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W39ObjectMethodsGetters
//
// Expected HotSpot 25 output (default and -Xint):
//   javac-shape: LoJavac[a=1, b=x, c=true] | LoJavac[a=1, b=x, c=true]
//   hash-reorder: 3721 | 3721
//   hash-subset: 120 | 120
//   hash-boolean: 1231 | 1231
//   equals-subset: true | true
//   tostring-subset: LoStrSubset[b=x] | LoStrSubset[b=x]
//   tostring-reorder: LoStrReorder[c=true, b=x, a=1] | LoStrReorder[c=true, b=x, a=1]
//   own-hashcode: 120
//   own-equals: true
//   foreign-private: java.lang.IllegalAccessError / java.lang.IllegalAccessException | java.lang.IllegalAccessError
//   null-component: true calls=1
//   null-this-hashcode: java.lang.NullPointerException | java.lang.NullPointerException
//   null-this-tostring: java.lang.NullPointerException | java.lang.NullPointerException
//   null-this-equals-null: true | true
//   null-this-equals-x: java.lang.NullPointerException | java.lang.NullPointerException
//
// `--compatible` (by design: no getter access check, and a getter that is
// not a field of the record keeps the positional reading) differs on
//   foreign-private: 120 | 120
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
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W39ObjectMethodsGetters {
    static final ClassDesc OM = ClassDesc.of("java.lang.runtime.ObjectMethods");
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, OM,
            "bootstrap", MethodTypeDesc.of(ConstantDescs.CD_Object,
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.TypeDescriptor"), ConstantDescs.CD_Class,
                    ConstantDescs.CD_String, ClassDesc.of("java.lang.invoke.MethodHandle").arrayType()));
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc INT = ConstantDescs.CD_int;
    static final ClassDesc BOOL = ConstantDescs.CD_boolean;
    static final MethodTypeDesc CTOR = MethodTypeDesc.of(ConstantDescs.CD_void, INT, STR, BOOL);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W39ObjectMethodsGetters.class.getClassLoader());
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
            default -> BOOL;
        };
    }

    /** `name:kind` getters: `a` is `REF_getField a`, `b()` the accessor method. */
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
        return DynamicCallSiteDesc.of(BSM, method, type, args);
    }

    /** `make1` / `make2`: the site, its result boxed. */
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

    /** A record `(int a, String b, boolean c)`; `own` is `null` or the method whose body is the site. */
    static byte[] record(String name, String method, String names, String[] getterNames, boolean own) {
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
            if (own) {
                if (method.equals("equals")) {
                    cb.withMethodBody("equals", MethodTypeDesc.of(BOOL, OBJ), ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL,
                            code -> code.aload(0).aload(1).invokedynamic(site).ireturn());
                } else {
                    cb.withMethodBody("hashCode", MethodTypeDesc.of(INT), ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL,
                            code -> code.aload(0).invokedynamic(site).ireturn());
                }
            } else {
                addMake(cb, self, method, site);
            }
        });
    }

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + (c == null ? "" : " / " + c.getClass().getName());
    }

    static String twice(Class<?> c, String method, Object x, Object y) {
        StringBuilder out = new StringBuilder();
        Method make;
        try {
            make = method.equals("equals") ? c.getMethod("make2", Object.class, Object.class)
                    : c.getMethod("make1", Object.class);
        } catch (Throwable t) {
            return "setup: " + t;
        }
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                out.append(method.equals("equals") ? make.invoke(null, x, y) : make.invoke(null, x));
            } catch (InvocationTargetException e) {
                out.append(describe(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    static Object[] instances(Class<?> c) throws Exception {
        var ctor = c.getConstructor(int.class, String.class, boolean.class);
        return new Object[] {ctor.newInstance(1, "x", true), ctor.newInstance(1, "y", true)};
    }

    static String row(String name, String method, String names, String... getterNames) {
        try {
            Class<?> c = LOADER.define(name, record(name, method, names, getterNames, false));
            Object[] xy = instances(c);
            return twice(c, method, xy[0], xy[1]);
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    /** javac's getters; a NULL receiver, and for `equals` the other operand null or `x`. */
    static String nullRow(String name, String method, boolean otherNull) {
        try {
            Class<?> c = LOADER.define(name, record(name, method, "a;b;c", new String[] {"a", "b", "c"}, false));
            Object[] xy = instances(c);
            return twice(c, method, null, otherNull ? null : xy[0]);
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    static String own(String name, String method, String... getterNames) {
        try {
            Class<?> c = LOADER.define(name, record(name, method, "a;b;c", getterNames, true));
            Object[] xy = instances(c);
            Object x = xy[0];
            return method.equals("equals") ? String.valueOf(x.equals(xy[1])) : String.valueOf(x.hashCode());
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    /** A plain class linking `hashCode` over `LoTarget`'s private fields. */
    static String foreign() {
        try {
            Class<?> target = LOADER.define("LoTarget", record("LoTarget", "hashCode", "a;b;c",
                    new String[] {"a", "b", "c"}, false));
            ClassDesc tdesc = ClassDesc.of("LoTarget");
            ClassDesc self = ClassDesc.of("LoForeign");
            DynamicCallSiteDesc site = site(tdesc, "hashCode", "b", getters(tdesc, "b"));
            byte[] b = ClassFile.of().build(self, cb -> {
                cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
                addMake(cb, tdesc, "hashCode", site);
            });
            Class<?> c = LOADER.define("LoForeign", b);
            Object[] xy = instances(target);
            return twice(c, "hashCode", xy[0], null);
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    static final class Weird {
        int calls;

        @Override
        public boolean equals(Object o) {
            calls++;
            return true;
        }

        @Override
        public int hashCode() {
            return 7;
        }
    }

    record P(Object o) {
    }

    public static void main(String[] args) {
        System.out.println("javac-shape: " + row("LoJavac", "toString", "a;b;c", "a", "b", "c"));
        System.out.println("hash-reorder: " + row("LoHashReorder", "hashCode", "b;a", "b", "a"));
        System.out.println("hash-subset: " + row("LoHashSubset", "hashCode", "b", "b"));
        System.out.println("hash-boolean: " + row("LoHashBool", "hashCode", "c", "c"));
        System.out.println("equals-subset: " + row("LoEqSubset", "equals", "a", "a"));
        System.out.println("tostring-subset: " + row("LoStrSubset", "toString", "b", "b"));
        System.out.println("tostring-reorder: " + row("LoStrReorder", "toString", "c;b;a", "c", "b", "a"));
        System.out.println("own-hashcode: " + own("LoOwnHash", "hashCode", "b"));
        System.out.println("own-equals: " + own("LoOwnEq", "equals", "a"));
        System.out.println("foreign-private: " + foreign());
        Weird w = new Weird();
        boolean eq = new P(w).equals(new P(null));
        System.out.println("null-component: " + eq + " calls=" + w.calls);
        System.out.println("null-this-hashcode: " + nullRow("LoNullHash", "hashCode", false));
        System.out.println("null-this-tostring: " + nullRow("LoNullStr", "toString", false));
        System.out.println("null-this-equals-null: " + nullRow("LoNullEqNull", "equals", true));
        System.out.println("null-this-equals-x: " + nullRow("LoNullEqX", "equals", false));
    }
}
