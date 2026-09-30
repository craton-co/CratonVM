// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L4
// (`i42-L4-reflective-switch-and-record-bootstraps-have-null-answering-bridges`):
// the `typeSwitch-*` / `enumSwitch-direct` rows of
// `L4W42ReflectiveSwitchBootstraps` differ on the host (waves 42 and 43) and
// no run printed which step fails. A direct call of `SwitchBootstraps.typeSwitch`
// / `enumSwitch` runs the JDK's own bytecode (the `Bridge`s of
// `register_p69_switch_bootstraps` exist only in `synthetic-jdk` builds), and
// this probe takes that bytecode apart, one row per step, in the order the
// JDK runs them (JDK 25 `java/lang/runtime/SwitchBootstraps.java`):
//
//   enum-constants      `Class.getEnumConstants` (`mappedEnumSwitch` reads
//                       `getEnumConstantsShared`, the same `values()` call)
//   classdesc-resolve   `ClassDesc.resolveConstantDesc(lookup)`
//                       (`ResolvedEnumLabels.test`)
//   enumdesc-resolve    `EnumDesc.resolveConstantDesc(lookup)` (same)
//   insert-at-2         `insertArguments(h, 2, 4 values)` then `asType` to a
//                       narrower leading parameter, then `invokeExact`
//                       (`enumSwitch`'s `MAPPED_ENUM_SWITCH` route)
//   hidden-switch       a hidden NESTMATE class built with `java.lang.classfile`
//                       whose static `(Object,int)int` does `Objects.checkIndex`,
//                       `instanceof` and `tableswitch`, found with `findStatic`
//                       on the hidden lookup (`generateTypeSwitch`)
//   hidden-switch-extra the same with two trailing parameters bound by
//                       `insertArguments(h, 2, ...)` (`generateTypeSwitch`'s
//                       extra-info shape)
//   typeSwitch-classes  `SwitchBootstraps.typeSwitch(.., Integer.class, String.class)`
//   typeSwitch-enumdesc `typeSwitch` with an `EnumDesc` label (extra-info route)
//   enumSwitch-mapped   `enumSwitch` with constant labels, restart 0
//                       (`mappedEnumSwitch`'s ordinal map)
//   enumSwitch-restart  the same site, restart 1 (`generateTypeSwitch` under
//                       `mappedEnumSwitch`)
//
// A failing row prints the exception chain and the innermost cause's top
// three frames, so a host diff names the step.
//
// Run: javac -d out L4W44SwitchBootstrapSteps.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W44SwitchBootstrapSteps
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible:
//   enum-constants: [A, B]
//   classdesc-resolve: true
//   enumdesc-resolve: true
//   insert-at-2: (E,int)int 1115 -1
//   hidden-switch: 1 0 2 -1
//   hidden-switch-extra: (Object,int)int 1 0 13 -1
//   typeSwitch-classes: 1 0 2 -1
//   typeSwitch-enumdesc: 2 0 1 -1
//   enumSwitch-mapped: (E,int)int 1 0 -1
//   enumSwitch-restart: 2 1 2
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.classfile.instruction.SwitchCase;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.runtime.SwitchBootstraps;
import java.util.Arrays;
import java.util.List;
import java.util.function.BiPredicate;

public class L4W44SwitchBootstrapSteps {
    enum E {
        A,
        B
    }

    interface Row {
        Object run() throws Throwable;
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        Throwable last = t;
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (c != t) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName()).append(": ").append(c.getMessage());
            last = c;
        }
        StackTraceElement[] st = last.getStackTrace();
        for (int i = 0; i < Math.min(3, st.length); i++) {
            sb.append(" @ ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
        }
        return sb.toString();
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = describe(t);
        }
        System.out.println(name + ": " + out);
    }

    static int mapped(Enum<?> e, int restart, MethodHandles.Lookup l, Class<?> c, Object[] labels, Object cache) {
        return e == null ? -1 : e.ordinal() * 10 + restart + labels.length + (l.lookupClass() == c ? 100 : 0)
                + (cache != null ? 1000 : 0);
    }

    static byte[] switchClass(boolean extra) {
        ClassDesc self = ClassDesc.of("L4W44SwitchBootstrapSteps$$Steps");
        ClassDesc objs = ClassDesc.of("java.util.Objects");
        MethodTypeDesc mtd = extra
                ? MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object, ConstantDescs.CD_int,
                        ClassDesc.of("java.util.function.BiPredicate"), ConstantDescs.CD_List)
                : MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object, ConstantDescs.CD_int);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_FINAL | ClassFile.ACC_SUPER | ClassFile.ACC_SYNTHETIC);
            cb.withMethodBody("typeSwitch", mtd, ClassFile.ACC_FINAL | ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> {
                        Label nonNull = c.newLabel();
                        Label dflt = c.newLabel();
                        Label case0 = c.newLabel();
                        Label case1 = c.newLabel();
                        c.iload(1).loadConstant(3)
                                .invokestatic(objs, "checkIndex",
                                        MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int,
                                                ConstantDescs.CD_int))
                                .pop().aload(0).ifnonnull(nonNull).iconst_m1().ireturn().labelBinding(nonNull);
                        c.iload(1).tableswitch(0, 1, dflt, List.of(SwitchCase.of(0, case0), SwitchCase.of(1, case1)));
                        c.labelBinding(case0).aload(0).instanceOf(ConstantDescs.CD_Integer).ifeq(case1).iconst_0()
                                .ireturn();
                        c.labelBinding(case1).aload(0).instanceOf(ConstantDescs.CD_String).ifeq(dflt).iconst_1()
                                .ireturn();
                        c.labelBinding(dflt);
                        if (extra) {
                            // the bound tail must arrive: size() of the List
                            c.aload(3).invokeinterface(ConstantDescs.CD_List, "size",
                                    MethodTypeDesc.of(ConstantDescs.CD_int)).bipush(10).iadd().ireturn();
                        } else {
                            c.iconst_2().ireturn();
                        }
                    });
        });
    }

    static String run4(MethodHandle h) throws Throwable {
        return (int) h.invokeExact((Object) "s", 0) + " " + (int) h.invokeExact((Object) 7, 0) + " "
                + (int) h.invokeExact((Object) 1.5, 0) + " " + (int) h.invokeExact((Object) null, 0);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        row("enum-constants", () -> Arrays.toString(E.class.getEnumConstants()));
        ClassDesc eDesc = ClassDesc.of(E.class.getName());
        row("classdesc-resolve", () -> eDesc.resolveConstantDesc(lookup) == E.class);
        row("enumdesc-resolve", () -> java.lang.Enum.EnumDesc.of(eDesc, "B").resolveConstantDesc(lookup) == E.B);
        row("insert-at-2", () -> {
            MethodHandle m = lookup.findStatic(L4W44SwitchBootstrapSteps.class, "mapped",
                    MethodType.methodType(int.class, Enum.class, int.class, MethodHandles.Lookup.class, Class.class,
                            Object[].class, Object.class));
            MethodHandle b = MethodHandles.insertArguments(m, 2, lookup, L4W44SwitchBootstrapSteps.class,
                    new Object[] {"x", "y"}, new Object());
            MethodHandle t = b.asType(MethodType.methodType(int.class, E.class, int.class));
            return t.type() + " " + (int) t.invokeExact(E.B, 3) + " " + (int) t.invokeExact((E) null, 0);
        });
        row("hidden-switch", () -> {
            MethodHandles.Lookup h = lookup.defineHiddenClass(switchClass(false), true,
                    MethodHandles.Lookup.ClassOption.NESTMATE, MethodHandles.Lookup.ClassOption.STRONG);
            MethodHandle s = h.findStatic(h.lookupClass(), "typeSwitch",
                    MethodType.methodType(int.class, Object.class, int.class));
            return run4(s.asType(MethodType.methodType(int.class, Object.class, int.class)));
        });
        row("hidden-switch-extra", () -> {
            MethodHandles.Lookup h = lookup.defineHiddenClass(switchClass(true), true,
                    MethodHandles.Lookup.ClassOption.NESTMATE, MethodHandles.Lookup.ClassOption.STRONG);
            MethodHandle s = h.findStatic(h.lookupClass(), "typeSwitch",
                    MethodType.methodType(int.class, Object.class, int.class, BiPredicate.class, List.class));
            BiPredicate<Integer, Object> p = (i, o) -> false;
            MethodHandle b = MethodHandles.insertArguments(s, 2, p, List.of(1, 2, 3));
            return b.type() + " " + run4(b);
        });
        MethodType ts = MethodType.methodType(int.class, Object.class, int.class);
        row("typeSwitch-classes", () -> {
            CallSite cs = SwitchBootstraps.typeSwitch(lookup, "typeSwitch", ts, Integer.class, String.class);
            return run4(cs.getTarget());
        });
        row("typeSwitch-enumdesc", () -> {
            CallSite cs = SwitchBootstraps.typeSwitch(lookup, "typeSwitch", ts,
                    java.lang.Enum.EnumDesc.of(eDesc, "B"), String.class);
            MethodHandle h = cs.getTarget();
            return (int) h.invokeExact((Object) E.A, 0) + " " + (int) h.invokeExact((Object) E.B, 0) + " "
                    + (int) h.invokeExact((Object) "s", 0) + " " + (int) h.invokeExact((Object) null, 0);
        });
        MethodType es = MethodType.methodType(int.class, E.class, int.class);
        CallSite[] site = new CallSite[1];
        row("enumSwitch-mapped", () -> {
            site[0] = SwitchBootstraps.enumSwitch(lookup, "enumSwitch", es, "B", "A");
            MethodHandle h = site[0].getTarget();
            return h.type() + " " + (int) h.invokeExact(E.A, 0) + " " + (int) h.invokeExact(E.B, 0) + " "
                    + (int) h.invokeExact((E) null, 0);
        });
        row("enumSwitch-restart", () -> {
            MethodHandle h = site[0] != null ? site[0].getTarget()
                    : SwitchBootstraps.enumSwitch(lookup, "enumSwitch", es, "B", "A").getTarget();
            return (int) h.invokeExact(E.B, 1) + " " + (int) h.invokeExact(E.A, 1) + " "
                    + (int) h.invokeExact(E.A, 2);
        });
    }
}
