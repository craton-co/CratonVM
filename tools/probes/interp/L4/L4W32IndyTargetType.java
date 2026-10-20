// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 32 (orchestrator): an `invokedynamic` whose
// bootstrap returns a `CallSite` with a target of a different type fails
// linkage, as `CallSite.makeSite` does
// (docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md,
// item 4).
//
// The caller is generated with the java.lang.classfile API: `run()` is one
// `invokedynamic` typed `()String` whose bootstrap answers a
// `ConstantCallSite` over `MethodHandles.constant(Object.class, "x")`, typed
// `()Object`. The control's bootstrap answers a `()String` target. Each site
// is executed twice: the second execution rethrows the recorded error
// (JVMS §5.4.3), which HotSpot rethrows without its cause.
//
// Before wave 32 CratonVM compared nothing and ran the `asType`-compatible
// target. `--compatible` keeps that.
//
// Run: javac -d out L4W32IndyTargetType.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W32IndyTargetType
//
// Expected HotSpot 25 output (default and -Xint):
//   mismatch#1: java.lang.BootstrapMethodError: CallSite bootstrap method initialization exception / java.lang.invoke.WrongMethodTypeException: MethodHandle()Object should be of type ()String
//   mismatch#2: java.lang.BootstrapMethodError: CallSite bootstrap method initialization exception
//   match#1: ok
//   match#2: ok
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
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

public class L4W32IndyTargetType {
    public static CallSite wrongType(MethodHandles.Lookup l, String name, MethodType type) {
        return new ConstantCallSite(MethodHandles.constant(Object.class, "x"));
    }

    public static CallSite rightType(MethodHandles.Lookup l, String name, MethodType type) {
        return new ConstantCallSite(MethodHandles.constant(String.class, "ok"));
    }

    static byte[] caller(String name, String bsm) {
        ClassDesc self = ClassDesc.of(name);
        ClassDesc owner = ClassDesc.of(L4W32IndyTargetType.class.getName());
        DirectMethodHandleDesc boot = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, owner, bsm,
                MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                        ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                        ClassDesc.of("java.lang.invoke.MethodType")));
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(boot, "site", MethodTypeDesc.of(ConstantDescs.CD_String));
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("run", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.invokedynamic(site).areturn());
        });
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W32IndyTargetType.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String call(Method m) {
        try {
            return String.valueOf(m.invoke(null));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            Throwable c = t.getCause();
            return t.getClass().getName() + ": " + t.getMessage()
                    + (c == null ? "" : " / " + c.getClass().getName() + ": " + c.getMessage());
        } catch (Throwable t) {
            return t.toString();
        }
    }

    public static void main(String[] args) throws Exception {
        Loader loader = new Loader();
        Method mismatch = loader.define("IndyMismatch", caller("IndyMismatch", "wrongType")).getMethod("run");
        Method match = loader.define("IndyMatch", caller("IndyMatch", "rightType")).getMethod("run");
        System.out.println("mismatch#1: " + call(mismatch));
        System.out.println("mismatch#2: " + call(mismatch));
        System.out.println("match#1: " + call(match));
        System.out.println("match#2: " + call(match));
    }
}
