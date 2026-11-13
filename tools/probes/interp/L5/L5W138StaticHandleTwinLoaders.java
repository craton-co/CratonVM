// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 138, lane L5 (page
// docs/feature-designs/real-jdk-mode.md):
// a `findStatic` handle on a class whose binary name several user loaders
// define, called for the first time. `--compatible`'s
// `Unsafe.ensureClassInitialized0` re-resolved the mirror's NAME for a class
// that was already initialized, and a name two user loaders define resolves
// to nothing there, so the `DirectMethodHandle` init barrier threw
// `NoClassDefFoundError: Twin`. The `init-first` rows initialize the class
// (Class.forName) after the handle is minted with its barrier and before the
// first call, which is exactly that shape; the `cold` rows let the barrier
// initialize it. Switch: CRATONVM_UNSAFE_ENSURE_INIT_BY_MIRROR_ID (default
// ON, `=0` restores the by-name route and the DIFF under --compatible).
//
// Run: javac -d out L5W138StaticHandleTwinLoaders.java
//      java -cp out L5W138StaticHandleTwinLoaders                       (HotSpot)
//      cratonvm --real-jdk|--compatible [--nojit] -cp out L5W138StaticHandleTwinLoaders
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// a-cold A clinits=1
// b-cold B clinits=1
// c-init-first C clinits=1
// d-init-first D clinits=1
// a-again A clinits=1
// forname-c C
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L5W138StaticHandleTwinLoaders {
    static int clinits;

    public static void clinitRan() {
        clinits++;
    }

    static final class Loader extends ClassLoader {
        final byte[] bytes;

        Loader(byte[] bytes) {
            super(L5W138StaticHandleTwinLoaders.class.getClassLoader());
            this.bytes = bytes;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals("Twin")) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static byte[] twin(String tag) {
        ClassDesc self = ClassDesc.of("Twin");
        ClassDesc probe = ClassDesc.of("L5W138StaticHandleTwinLoaders");
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("TAG", ConstantDescs.CD_String, ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    code -> code.invokestatic(probe, "clinitRan", ConstantDescs.MTD_void)
                            .ldc(tag).putstatic(self, "TAG", ConstantDescs.CD_String).return_());
            cb.withMethodBody("id", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.getstatic(self, "TAG", ConstantDescs.CD_String).areturn());
        });
    }

    static String call(MethodHandle mh) {
        try {
            return (String) mh.invokeExact();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String row(String label, String tag, boolean initFirst) throws Throwable {
        Loader l = new Loader(twin(tag));
        Class<?> c = l.loadClass("Twin");
        MethodHandle mh = MethodHandles.lookup().findStatic(c, "id", MethodType.methodType(String.class));
        if (initFirst) {
            Class.forName("Twin", true, l);
        }
        int before = clinits;
        String first = call(mh);
        int ran = clinits - before + (initFirst ? 1 : 0);
        // Warm the handle: every later call answers the same.
        for (int i = 0; i < 2000; i++) {
            String again = call(mh);
            if (!first.equals(again)) {
                return label + " " + first + " then " + again;
            }
        }
        return label + " " + first + " clinits=" + ran;
    }

    public static void main(String[] args) throws Throwable {
        System.out.println(row("a-cold", "A", false));
        System.out.println(row("b-cold", "B", false));
        System.out.println(row("c-init-first", "C", true));
        System.out.println(row("d-init-first", "D", true));
        // A first loader's handle still answers its own class.
        System.out.println(row("a-again", "A", false));
        Loader l = new Loader(twin("C"));
        Class<?> c = Class.forName("Twin", true, l);
        System.out.println("forname-c " + c.getMethod("id").invoke(null));
    }
}
