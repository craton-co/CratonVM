// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4 (review): an interface's PRIVATE
// instance method, STATIC method and DEFAULT method reached through every
// invoke instruction, from the interface itself (private access) and from an
// implementing class (no private access). JVMS 6.5: `invokeinterface` /
// `invokespecial` of a static method and `invokestatic` of an instance
// method are `IncompatibleClassChangeError`; a private member from outside
// its nest is `IllegalAccessError`.
//
// With the java.lang.classfile API the probe generates `LoI` (an interface:
// private `p()I` = 1, static `s()I` = 2, default `d()I` = 3, and static
// callers `rN(LoI)I`, each one invoke of the row's shape on its argument),
// `LoC implements LoI` (callers `cN(LoC)I`) and `LoX` (no relation). Each
// row runs twice (a resolution error is raised again).
//
//   i-iface-private     in LoI: invokeinterface LoI.p           -> 1
//   i-special-private   in LoI: invokespecial   LoI.p           -> 1
//   i-static-private    in LoI: invokestatic    LoI.p (instance) -> ICCE
//   i-iface-static      in LoI: invokeinterface LoI.s (static)   -> ICCE
//   i-special-static    in LoI: invokespecial   LoI.s (static)   -> ICCE
//   i-static-static     in LoI: invokestatic    LoI.s           -> 2
//   i-iface-default     in LoI: invokeinterface LoI.d           -> 3
//   i-special-default   in LoI: invokespecial   LoI.d           -> 3
//   i-iface-private-x   in LoI: invokeinterface LoI.p on a LoX  -> ICCE (receiver)
//   c-iface-private     in LoC: invokeinterface LoI.p           -> IAE
//   c-special-private   in LoC: invokespecial   LoI.p           -> IAE
//   c-special-default   in LoC: invokespecial   LoI.d           -> 3
//   c-static-static     in LoC: invokestatic    LoI.s           -> 2
//   c-iface-static      in LoC: invokeinterface LoI.s           -> ICCE
//   k-*                 the same kinds on a class `LoK` (static `ks()I`,
//                       instance `ki()I`), the class-reference wording
//
// Run: javac -d out L4W43InterfaceMemberKinds.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W43InterfaceMemberKinds
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   i-iface-private: 1 | 1
//   i-special-private: 1 | 1
//   i-static-private: java.lang.IncompatibleClassChangeError: Expected static method 'int LoI.p()' | java.lang.IncompatibleClassChangeError: Expected static method 'int LoI.p()'
//   i-iface-static: java.lang.IncompatibleClassChangeError: Expected instance not static method 'int LoI.s()' | java.lang.IncompatibleClassChangeError: Expected instance not static method 'int LoI.s()'
//   i-special-static: java.lang.IncompatibleClassChangeError: Expected instance not static method 'int LoI.s()' | java.lang.IncompatibleClassChangeError: Expected instance not static method 'int LoI.s()'
//   i-static-static: 2 | 2
//   i-iface-default: 3 | 3
//   i-special-default: 3 | 3
//   i-iface-private-x: java.lang.IncompatibleClassChangeError: Class LoX does not implement the requested interface LoI | java.lang.IncompatibleClassChangeError: Class LoX does not implement the requested interface LoI
//   c-iface-private: java.lang.IllegalAccessError: class LoC tried to access private method 'int LoI.p()' | java.lang.IllegalAccessError: class LoC tried to access private method 'int LoI.p()'
//   c-special-private: java.lang.IllegalAccessError: class LoC tried to access private method 'int LoI.p()' | java.lang.IllegalAccessError: class LoC tried to access private method 'int LoI.p()'
//   c-special-default: 3 | 3
//   c-static-static: 2 | 2
//   c-iface-static: java.lang.IncompatibleClassChangeError: Expected instance not static method 'int LoI.s()' | java.lang.IncompatibleClassChangeError: Expected instance not static method 'int LoI.s()'
//   k-virtual-static: java.lang.IncompatibleClassChangeError: Expecting non-static method 'int LoK.ks()' | java.lang.IncompatibleClassChangeError: Expecting non-static method 'int LoK.ks()'
//   k-special-static: java.lang.IncompatibleClassChangeError: Expecting non-static method 'int LoK.ks()' | java.lang.IncompatibleClassChangeError: Expecting non-static method 'int LoK.ks()'
//   k-static-instance: java.lang.IncompatibleClassChangeError: Expected static method 'int LoK.ki()' | java.lang.IncompatibleClassChangeError: Expected static method 'int LoK.ki()'
//   k-virtual-instance: 5 | 5
//
// (An IllegalAccessError's text is cut before HotSpot's " (LoC and LoI are in
// unnamed module of loader ... @<hash>)", which is not deterministic.)
//
// Read from the code, CratonVM before wave 43: `i-iface-static`,
// `i-special-static` and `c-iface-static` said `Expecting non-static method`
// (the class-reference wording of `selection::static_flag_mismatch`). The
// `k-*` rows are its controls. The other rows were not traced.
import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Opcode;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;

public class L4W43InterfaceMemberKinds {
    static final ClassDesc I = ClassDesc.of("LoI");
    static final ClassDesc C = ClassDesc.of("LoC");
    static final ClassDesc X = ClassDesc.of("LoX");
    static final ClassDesc K = ClassDesc.of("LoK");
    static final MethodTypeDesc V = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);

    record Shape(String row, Opcode op, String member, boolean onX) {
    }

    static final List<Shape> I_ROWS = List.of(
            new Shape("i-iface-private", Opcode.INVOKEINTERFACE, "p", false),
            new Shape("i-special-private", Opcode.INVOKESPECIAL, "p", false),
            new Shape("i-static-private", Opcode.INVOKESTATIC, "p", false),
            new Shape("i-iface-static", Opcode.INVOKEINTERFACE, "s", false),
            new Shape("i-special-static", Opcode.INVOKESPECIAL, "s", false),
            new Shape("i-static-static", Opcode.INVOKESTATIC, "s", false),
            new Shape("i-iface-default", Opcode.INVOKEINTERFACE, "d", false),
            new Shape("i-special-default", Opcode.INVOKESPECIAL, "d", false),
            new Shape("i-iface-private-x", Opcode.INVOKEINTERFACE, "p", true));

    static final List<Shape> C_ROWS = List.of(
            new Shape("c-iface-private", Opcode.INVOKEINTERFACE, "p", false),
            new Shape("c-special-private", Opcode.INVOKESPECIAL, "p", false),
            new Shape("c-special-default", Opcode.INVOKESPECIAL, "d", false),
            new Shape("c-static-static", Opcode.INVOKESTATIC, "s", false),
            new Shape("c-iface-static", Opcode.INVOKEINTERFACE, "s", false));

    /** Class rows: `LoK` (static `ks()I` = 4, instance `ki()I` = 5), callers in `LoK`. */
    static final List<Shape> K_ROWS = List.of(
            new Shape("k-virtual-static", Opcode.INVOKEVIRTUAL, "ks", false),
            new Shape("k-special-static", Opcode.INVOKESPECIAL, "ks", false),
            new Shape("k-static-instance", Opcode.INVOKESTATIC, "ki", false),
            new Shape("k-virtual-instance", Opcode.INVOKEVIRTUAL, "ki", false));

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W43InterfaceMemberKinds.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** One invoke of `shape` on local 0 (its receiver, unless the op is static). */
    static void emit(CodeBuilder code, Shape shape) {
        if (shape.op() != Opcode.INVOKESTATIC) {
            code.aload(0);
            if (shape.onX()) {
                code.checkcast(ConstantDescs.CD_Object);
            }
        }
        code.invoke(shape.op(), I, shape.member(), INT, true).ireturn();
    }

    static byte[] iface() {
        return ClassFile.of().build(I, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethodBody("p", INT, ClassFile.ACC_PRIVATE, code -> code.iconst_1().ireturn());
            cb.withMethodBody("s", INT, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.iconst_2().ireturn());
            cb.withMethodBody("d", INT, ClassFile.ACC_PUBLIC, code -> code.iconst_3().ireturn());
            for (int i = 0; i < I_ROWS.size(); i++) {
                Shape shape = I_ROWS.get(i);
                cb.withMethodBody("r" + i, MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                            if (shape.op() != Opcode.INVOKESTATIC && !shape.onX()) {
                                code.aload(0).checkcast(I).astore(0);
                            }
                            emit(code, shape);
                        });
            }
        });
    }

    static byte[] impl() {
        return ClassFile.of().build(C, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(I);
            cb.withMethodBody(ConstantDescs.INIT_NAME, V, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, V).return_());
            for (int i = 0; i < C_ROWS.size(); i++) {
                Shape shape = C_ROWS.get(i);
                // `invokespecial` needs a receiver of the current class.
                cb.withMethodBody("c" + i, MethodTypeDesc.of(ConstantDescs.CD_int, C),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> emit(code, shape));
            }
        });
    }

    static byte[] klass() {
        return ClassFile.of().build(K, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, V, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, V).return_());
            cb.withMethodBody("ks", INT, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.iconst_4().ireturn());
            cb.withMethodBody("ki", INT, ClassFile.ACC_PUBLIC, code -> code.iconst_5().ireturn());
            for (int i = 0; i < K_ROWS.size(); i++) {
                Shape shape = K_ROWS.get(i);
                cb.withMethodBody("k" + i, MethodTypeDesc.of(ConstantDescs.CD_int, K),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                            if (shape.op() != Opcode.INVOKESTATIC) {
                                code.aload(0);
                            }
                            code.invoke(shape.op(), K, shape.member(), INT, false).ireturn();
                        });
            }
        });
    }

    static byte[] other() {
        return ClassFile.of().build(X, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, V, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, V).return_());
        });
    }

    static String twice(Method m, Object arg) {
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                out.append(m.invoke(null, arg));
            } catch (InvocationTargetException e) {
                // The loader's identity hash in an IllegalAccessError's
                // module text is not deterministic: cut it.
                String text = String.valueOf(e.getCause());
                int cut = text.indexOf(" (LoC and LoI are in");
                out.append(cut < 0 ? text : text.substring(0, cut));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    public static void main(String[] args) throws Throwable {
        Loader loader = new Loader();
        Class<?> ic;
        Class<?> cc;
        Class<?> xc;
        Class<?> kc;
        try {
            ic = loader.define("LoI", iface());
            cc = loader.define("LoC", impl());
            xc = loader.define("LoX", other());
            kc = loader.define("LoK", klass());
        } catch (Throwable t) {
            System.out.println("setup: " + t);
            return;
        }
        Object c = cc.getConstructor().newInstance();
        Object x = xc.getConstructor().newInstance();
        for (int i = 0; i < I_ROWS.size(); i++) {
            Shape shape = I_ROWS.get(i);
            Method m = ic.getMethod("r" + i, Object.class);
            System.out.println(shape.row() + ": " + twice(m, shape.onX() ? x : c));
        }
        for (int i = 0; i < C_ROWS.size(); i++) {
            Shape shape = C_ROWS.get(i);
            Method m = cc.getMethod("c" + i, cc);
            System.out.println(shape.row() + ": " + twice(m, c));
        }
        Object k = kc.getConstructor().newInstance();
        for (int i = 0; i < K_ROWS.size(); i++) {
            Shape shape = K_ROWS.get(i);
            Method m = kc.getMethod("k" + i, kc);
            System.out.println(shape.row() + ": " + twice(m, k));
        }
    }
}
