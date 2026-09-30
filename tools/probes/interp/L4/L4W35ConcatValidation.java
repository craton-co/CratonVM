// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 35 (orchestrator): an `invokedynamic` of
// `StringConcatFactory.makeConcatWithConstants` is validated as the JDK's
// factory validates it
// (docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md,
// item 1). javac never emits these shapes; each caller is generated with the
// java.lang.classfile API.
//
//   args      recipe "\1\1" for a (int)String site
//   consts    recipe "\2" with no constant
//   return    a (int)Integer site
//   class     recipe "x=\2" with the constant `Runnable.class`
//   ok        recipe "<\1>" for a (int)String site (the control)
//
// Before wave 35 CratonVM linked every one of them itself: it skipped the
// surplus or missing tags, answered the Integer site with a String, and
// printed `class java.lang.Runnable`. `--compatible` keeps that.
//
// Run: javac -d out L4W35ConcatValidation.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W35ConcatValidation
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;

public class L4W35ConcatValidation {
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            ClassDesc.of("java.lang.invoke.StringConcatFactory"), "makeConcatWithConstants",
            MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.MethodType"), ConstantDescs.CD_String,
                    ConstantDescs.CD_Object.arrayType()));

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W35ConcatValidation.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String row(String name, ClassDesc ret, String recipe, ConstantDesc... constants) {
        ConstantDesc[] args = new ConstantDesc[constants.length + 1];
        args[0] = recipe;
        System.arraycopy(constants, 0, args, 1, constants.length);
        MethodTypeDesc type = MethodTypeDesc.of(ret, ConstantDescs.CD_int);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(BSM, "concat", type, args);
        byte[] b = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("run", MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.iload(0).invokedynamic(site).areturn());
        });
        try {
            Object r = LOADER.define(name, b).getMethod("run", int.class).invoke(null, 7);
            return String.valueOf(r);
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            Throwable c = t.getCause();
            return t.getClass().getName() + (c == null ? "" : " / " + c.getClass().getName() + ": " + c.getMessage());
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) {
        System.out.println("args: " + row("ConcatArgs", ConstantDescs.CD_String, "\u0001\u0001"));
        System.out.println("consts: " + row("ConcatConsts", ConstantDescs.CD_String, "\u0002\u0001"));
        System.out.println("return: " + row("ConcatReturn", ClassDesc.of("java.lang.Integer"), "\u0001"));
        System.out.println("class: " + row("ConcatClass", ConstantDescs.CD_String, "\u0001 x=\u0002",
                ClassDesc.of("java.lang.Runnable")));
        System.out.println("ok: " + row("ConcatOk", ConstantDescs.CD_String, "<\u0001>"));
    }
}
