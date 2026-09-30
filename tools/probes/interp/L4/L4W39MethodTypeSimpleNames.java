// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L4 (item 2 of
// `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`): a
// `MethodType` text names each class by `Class.getSimpleName()`, which reads
// the class's own `InnerClasses` entry: a LOCAL class `Outer$1Local` is
// `Local` (CratonVM printed `1Local`), an ANONYMOUS class `Outer$1` is `""`
// (CratonVM printed `1`), and a TOP-LEVEL class whose name contains `$` keeps
// it (`L4W39Top$Name`; CratonVM printed `Name`). The texts are the messages
// of `MethodType.toString` (`--compatible` native) and a bootstrap's
// `CallSite` of the wrong type (`CallSite.makeSite`'s text, which the VM
// composes; `--jdk-only` only: `--compatible` does not check a bootstrap's
// `CallSite` type, by design since wave 32, and its line for that row is not
// recorded). The `invokeExact` rows this probe had (an `identity` and a
// `constant` handle called with a mismatched type) were removed after the
// wave-39 host run: CratonVM does not throw there at all, a separate gap
// (`docs/internal/fixed-bugs/interpreter-L4-invokeexact-of-identity-and-constant-handles-does-not-check-the-type-FIXED-20261004.md`; fixed in wave 40, rows in `L4W40IdentityConstantInvokeExact.java`).
//
// Run: javac -d out L4W39MethodTypeSimpleNames.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W39MethodTypeSimpleNames
//
// Expected HotSpot 25 output (default and -Xint):
//   tostring: (Local,,L4W39Top$Name)void
//   callsite-type: java.lang.BootstrapMethodError / java.lang.invoke.WrongMethodTypeException: MethodHandle()Local should be of type ()Object
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;

class L4W39Top$Name {
}

public class L4W39MethodTypeSimpleNames {
    static Class<?> localClass;
    static Object localInstance;

    /** A bootstrap whose call site's target is `()Local`. */
    public static CallSite bsm(MethodHandles.Lookup lookup, String name, MethodType type) {
        return new ConstantCallSite(MethodHandles.constant(localClass, localInstance));
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W39MethodTypeSimpleNames.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String callsiteRow() {
        ClassDesc self = ClassDesc.of("LoCallsiteType");
        ClassDesc probe = ClassDesc.of("L4W39MethodTypeSimpleNames");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, probe, "bsm",
                MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                        ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                        ClassDesc.of("java.lang.invoke.MethodType")));
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm, "x", MethodTypeDesc.of(ConstantDescs.CD_Object));
        byte[] b = ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.invokedynamic(site).areturn());
        });
        try {
            Class<?> c = new Loader().define("LoCallsiteType", b);
            return "returned " + c.getMethod("make").invoke(null).getClass().getName();
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            Throwable cause = t.getCause();
            return t.getClass().getName() + (cause == null ? ": " + t.getMessage() : " / " + cause);
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) throws Throwable {
        class Local {
        }
        Object anon = new Object() {
        };
        localClass = Local.class;
        localInstance = new Local();
        System.out.println("tostring: "
                + MethodType.methodType(void.class, Local.class, anon.getClass(), L4W39Top$Name.class));
        System.out.println("callsite-type: " + callsiteRow());
    }
}
