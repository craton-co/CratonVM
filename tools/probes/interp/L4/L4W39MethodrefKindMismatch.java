// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L4 (review): JVMS 5.4.3.3 / 5.4.3.4. A
// `CONSTANT_Methodref` whose class is an INTERFACE, and a
// `CONSTANT_InterfaceMethodref` whose class is a CLASS, fail method
// resolution with `IncompatibleClassChangeError`, whatever the invoke
// instruction; the message depends on the instruction. Each row generates,
// with the java.lang.classfile API, an interface `LoI` (a static `s()`, a
// default `d()`), a class `LoC implements LoI` (a static `cs()`, an instance
// `ci()`), and a caller class whose static `run()` is one invoke of the row's
// shape; `run()` is called twice (a resolution error is raised again).
//
//   static-methodref-iface      invokestatic    Methodref          LoI.s()V
//   virtual-methodref-iface     invokevirtual   Methodref          LoI.d()V (receiver new LoC)
//   static-ifaceref-class       invokestatic    InterfaceMethodref LoC.cs()V
//   interface-ifaceref-class    invokeinterface InterfaceMethodref LoC.ci()V (receiver new LoC)
//   special-methodref-iface     invokespecial   Methodref          LoI.d()V (from `LoR8 implements LoI`)
//   control-static              invokestatic    InterfaceMethodref LoI.s()V
//
// (`invokevirtual` of an `InterfaceMethodref` and `invokeinterface` of a
// `Methodref` are `VerifyError`s on HotSpot: "Illegal type at constant pool
// entry"; not rows here.)
//
// `--compatible` links these references (by design, unchanged); its lines
// are not recorded.
//
// Run: javac -d out L4W39MethodrefKindMismatch.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W39MethodrefKindMismatch
//
// Expected HotSpot 25 output (default and -Xint):
//   static-methodref-iface: java.lang.IncompatibleClassChangeError: Method 'void LoI.s()' must be InterfaceMethodref constant | java.lang.IncompatibleClassChangeError: Method 'void LoI.s()' must be InterfaceMethodref constant
//   virtual-methodref-iface: java.lang.IncompatibleClassChangeError: Found interface LoI, but class was expected | java.lang.IncompatibleClassChangeError: Found interface LoI, but class was expected
//   static-ifaceref-class: java.lang.IncompatibleClassChangeError: Method 'void LoC.cs()' must be Methodref constant | java.lang.IncompatibleClassChangeError: Method 'void LoC.cs()' must be Methodref constant
//   interface-ifaceref-class: java.lang.IncompatibleClassChangeError: Found class LoC, but interface was expected | java.lang.IncompatibleClassChangeError: Found class LoC, but interface was expected
//   special-methodref-iface: java.lang.IncompatibleClassChangeError: Method 'void LoI.d()' must be InterfaceMethodref constant | java.lang.IncompatibleClassChangeError: Method 'void LoI.d()' must be InterfaceMethodref constant
//   control-static: ok | ok
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.function.Consumer;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Opcode;

public class L4W39MethodrefKindMismatch {
    static final ClassDesc I = ClassDesc.of("LoI");
    static final ClassDesc C = ClassDesc.of("LoC");
    static final MethodTypeDesc V = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W39MethodrefKindMismatch.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static void setup() {
        LOADER.define("LoI", ClassFile.of().build(I, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethodBody("s", V, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.return_());
            cb.withMethodBody("d", V, ClassFile.ACC_PUBLIC, code -> code.return_());
        }));
        LOADER.define("LoC", ClassFile.of().build(C, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(I);
            cb.withMethodBody(ConstantDescs.INIT_NAME, V, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, V).return_());
            cb.withMethodBody("cs", V, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.return_());
            cb.withMethodBody("ci", V, ClassFile.ACC_PUBLIC, code -> code.return_());
        }));
    }

    static String row(String name, Consumer<CodeBuilder> body) {
        ClassDesc self = ClassDesc.of(name);
        byte[] b = ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("run", V, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, body);
        });
        Method run;
        try {
            run = LOADER.define(name, b).getMethod("run");
        } catch (Throwable t) {
            return "setup: " + t;
        }
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                run.invoke(null);
                out.append("ok");
            } catch (InvocationTargetException e) {
                out.append(e.getCause());
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    /** `LoR8 implements LoI`: `run()` is `new LoR8; invokespecial Methodref LoI.d()V`. */
    static String special() {
        ClassDesc self = ClassDesc.of("LoR8");
        byte[] b = ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(I);
            cb.withMethodBody(ConstantDescs.INIT_NAME, V, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, V).return_());
            cb.withMethodBody("run", V, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.new_(self).dup()
                    .invokespecial(self, ConstantDescs.INIT_NAME, V)
                    .invoke(Opcode.INVOKESPECIAL, I, "d", V, false).return_());
        });
        Method run;
        try {
            run = LOADER.define("LoR8", b).getMethod("run");
        } catch (Throwable t) {
            return "setup: " + t;
        }
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                run.invoke(null);
                out.append("ok");
            } catch (InvocationTargetException e) {
                out.append(e.getCause());
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    static void newC(CodeBuilder code) {
        code.new_(C).dup().invokespecial(C, ConstantDescs.INIT_NAME, V);
    }

    public static void main(String[] args) {
        setup();
        System.out.println("static-methodref-iface: "
                + row("LoR1", code -> code.invokestatic(I, "s", V, false).return_()));
        System.out.println("virtual-methodref-iface: "
                + row("LoR2", code -> {
                    newC(code);
                    code.invokevirtual(I, "d", V).return_();
                }));
        System.out.println("static-ifaceref-class: "
                + row("LoR3", code -> code.invokestatic(C, "cs", V, true).return_()));
        System.out.println("interface-ifaceref-class: "
                + row("LoR4", code -> {
                    newC(code);
                    code.invokeinterface(C, "ci", V).return_();
                }));
        System.out.println("special-methodref-iface: " + special());
        System.out.println("control-static: "
                + row("LoR5", code -> code.invokestatic(I, "s", V, true).return_()));
    }
}
