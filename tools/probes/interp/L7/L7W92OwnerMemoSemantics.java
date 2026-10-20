// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L7: the static door's loader-split
// guard keeps the right owner across class-definition churn when it is
// stamped with the class-name generation
// (`CRATONVM_INTERP_OWNER_MEMO_NAME_GENERATION`, default off). Two loaders
// split the name `Q`: the application loader's copy (this file)
// answers "parent", a child-first loader's own copy answers "child". A
// caller class defined by each loader runs `Q.id()` through
// `invokestatic` before and after 200 unrelated definitions (which move the
// definition epoch but not the name generation), and after a THIRD loader
// defines its own `Q` (which moves the name generation). Every
// answer is the caller's own loader's view, as JVMS 5.3 requires.
//
// Run each with the switch off and on, --nojit and JIT, both modes; the
// loader-aware resolution path is exercised further with
// CRATONVM_LOADER_AWARE_RESOLUTION=1:
//   javac -d out L7W92OwnerMemoSemantics.java
//   java -cp out L7W92OwnerMemoSemantics                                    (HotSpot reference)
//   CRATONVM_INTERP_OWNER_MEMO_NAME_GENERATION=1 cratonvm --jdk-only --nojit -cp out L7W92OwnerMemoSemantics
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// app parent
// child-first before child
// delegating before parent
// churn 200
// child-first after-churn child child child
// delegating after-churn parent parent parent
// third own child
// child-first after-split child
// delegating after-split parent
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;

public class L7W92OwnerMemoSemantics {
    static final ClassDesc CD_STRING = ConstantDescs.CD_String;
    static final MethodTypeDesc RET_STRING = MethodTypeDesc.of(CD_STRING);
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    /** The split name: the application loader's copy is the nested class below. */
    static final String QNAME = "L7W92OwnerMemoSemantics$Q";

    /** The application loader's `Q`. Public, so a caller in another loader may call it. */
    public static class Q {
        public static String id() {
            return "parent";
        }
    }

    /** `Q` whose `id()` answers `answer`. */
    static byte[] ownQ(String answer) {
        return ClassFile.of().build(ClassDesc.of(QNAME), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("id", RET_STRING, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc(answer).areturn());
        });
    }

    /** `name` with `public static String run() { return Q.id(); }`. */
    static byte[] caller(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("run", RET_STRING, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokestatic(ClassDesc.of(QNAME), "id", RET_STRING)
                            .areturn());
        });
    }

    static byte[] empty(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    /**
     * Defines the caller `callerName` itself; defines its own `Q`
     * (answering `own`) when `own` is non-null, else delegates it.
     */
    static final class Split extends ClassLoader {
        final String callerName;
        final String own;

        Split(String callerName, String own) {
            super(L7W92OwnerMemoSemantics.class.getClassLoader());
            this.callerName = callerName;
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    if (name.equals(callerName)) {
                        byte[] b = caller(name);
                        c = defineClass(name, b, 0, b.length);
                    } else if (own != null && name.equals(QNAME)) {
                        byte[] b = ownQ(own);
                        c = defineClass(name, b, 0, b.length);
                    } else {
                        return super.loadClass(name, resolve);
                    }
                }
                return c;
            }
        }

        Class<?> defineNow(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String run(Method m) throws Exception {
        return (String) m.invoke(null);
    }

    static String thrice(Method m) throws Exception {
        return run(m) + " " + run(m) + " " + run(m);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("app " + Q.id());
        Split childFirst = new Split("L7W92OwnerCallerA", "child");
        Split delegating = new Split("L7W92OwnerCallerB", null);
        Method a = Class.forName("L7W92OwnerCallerA", true, childFirst).getMethod("run");
        Method b = Class.forName("L7W92OwnerCallerB", true, delegating).getMethod("run");
        System.out.println("child-first before " + run(a));
        System.out.println("delegating before " + run(b));
        Split churn = new Split("unused", null);
        int defined = 0;
        for (int i = 0; i < 200; i++) {
            String n = "L7W92OwnerChurn" + i;
            if (churn.defineNow(n, empty(n)).getName().equals(n)) {
                defined++;
            }
        }
        System.out.println("churn " + defined);
        System.out.println("child-first after-churn " + thrice(a));
        System.out.println("delegating after-churn " + thrice(b));
        // A third loader defines its own `Q`: the name now maps to
        // three classes, and neither existing caller's answer may change.
        Split third = new Split("L7W92OwnerCallerC", "child");
        Method c = Class.forName("L7W92OwnerCallerC", true, third).getMethod("run");
        System.out.println("third own " + run(c));
        System.out.println("child-first after-split " + run(a));
        System.out.println("delegating after-split " + run(b));
        }
}
