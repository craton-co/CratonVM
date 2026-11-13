// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 139, lane L5 (page
// docs/feature-designs/real-jdk-mode.md):
// a class a user loader defines under a JDK-global prefix (`javax/`) names
// ITSELF. JVMS 5.3.5: the defining loader is an initiating loader of the
// class, so `ldc Self.class`, a static call to its own method and a static
// field of its own resolve to the class itself, even while the application
// loader holds another `javax.l5w139.Self`. CratonVM resolved JDK-global names
// through the global namespace, which answers the application's copy.
// Switch: CRATONVM_SELF_NAME_RESOLVES_TO_SELF (default ON, `=0` control).
//
// The probe writes the application copy into the first class-path directory
// (`-cp out`), so run it with a writable directory class path.
//
// Run: javac -d out L5W139SelfNameGlobalPrefix.java
//      java -cp out L5W139SelfNameGlobalPrefix                       (HotSpot)
//      cratonvm --real-jdk|--compatible [--nojit] -cp out L5W139SelfNameGlobalPrefix
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// app-copy: loader=app tag=app
// user-ldc: loader=user
// user-static-self: loader=user
// user-field: tag=user
// app-again: loader=app tag=app
// HOTSPOT_EXPECTED_END
import java.io.File;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.nio.file.Files;
import java.nio.file.Path;

public class L5W139SelfNameGlobalPrefix {
    static final String NAME = "javax.l5w139.Self";
    static final ClassDesc SELF = ClassDesc.of(NAME);
    static final MethodTypeDesc LOADER = MethodTypeDesc.of(ClassDesc.of("java.lang.ClassLoader"));
    static final MethodTypeDesc STRING = MethodTypeDesc.of(ConstantDescs.CD_String);

    // class Self { static String TAG = <tag>;
    //   public static ClassLoader who() { return Self.class.getClassLoader(); }
    //   public static ClassLoader viaStatic() { return who(); }
    //   public static String tag() { return TAG; } }
    static byte[] self(String tag) {
        return ClassFile.of().build(SELF, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("TAG", ConstantDescs.CD_String, ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    code -> code.ldc(tag).putstatic(SELF, "TAG", ConstantDescs.CD_String).return_());
            cb.withMethodBody("who", LOADER, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc(SELF)
                            .invokevirtual(ConstantDescs.CD_Class, "getClassLoader", LOADER)
                            .areturn());
            cb.withMethodBody("viaStatic", LOADER, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokestatic(SELF, "who", LOADER).areturn());
            cb.withMethodBody("tag", STRING, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.getstatic(SELF, "TAG", ConstantDescs.CD_String).areturn());
        });
    }

    static final class User extends ClassLoader {
        final byte[] bytes = self("user");

        User() {
            super(null);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(NAME)) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static String who(ClassLoader app, ClassLoader user, Object loader) {
        return loader == app ? "app" : loader == user ? "user" : String.valueOf(loader);
    }

    public static void main(String[] args) throws Exception {
        String first = System.getProperty("java.class.path").split(File.pathSeparator)[0];
        Path file = Path.of(first, "javax", "l5w139", "Self.class");
        Files.createDirectories(file.getParent());
        Files.write(file, self("app"));
        ClassLoader app = ClassLoader.getSystemClassLoader();
        Class<?> appCopy = Class.forName(NAME, true, app);
        User user = new User();
        Class<?> userCopy = Class.forName(NAME, true, user);
        System.out.println("app-copy: loader=" + who(app, user, appCopy.getMethod("who").invoke(null))
                + " tag=" + appCopy.getMethod("tag").invoke(null));
        System.out.println("user-ldc: loader=" + who(app, user, userCopy.getMethod("who").invoke(null)));
        System.out.println("user-static-self: loader="
                + who(app, user, userCopy.getMethod("viaStatic").invoke(null)));
        System.out.println("user-field: tag=" + userCopy.getMethod("tag").invoke(null));
        System.out.println("app-again: loader=" + who(app, user, appCopy.getMethod("who").invoke(null))
                + " tag=" + appCopy.getMethod("tag").invoke(null));
    }
}
