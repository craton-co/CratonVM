// Interpreter round i1, wave 28, lane L5 — the MODULE of a user loader's own
// class under a JDK package name, and which loaders the VM asks for a JDK
// name.
//
// `ClassLoader.defineClass` puts a class of a user-defined loader in that
// loader's unnamed module; the JDK's platform modules are defined to the
// built-in loaders only. So a child-first loader's own
// `javax.security.auth.x500.X500PrivateCredential` is NOT in `java.base`
// (rows `own ...`). CratonVM labelled a class's module by its PACKAGE
// (`classloading/src/class_manager.rs` `define_class_shared_with_options`,
// `module_for_package`), whatever the defining loader, so `getModule()` (the
// `Class.getModule` intrinsic in `native-builtins/src/lib.rs`, which reads
// that label) answered `java.base`.
//
// Rows `asked ...`: whether a loader's `loadClass` is called for a JDK name
// its class references (`javax.security.auth.x500.X500Principal`, which
// java.base has). HotSpot asks the initiating loader for every name
// (`SystemDictionary::resolve_instance_class_or_null`); a loader that
// overrides `loadClass` and delegates parent-first sees the request and
// answers java.base's class. CratonVM's global route never asked a loader for
// a `javax/`, `jdk/`, `sun/`, `com/sun/` name before wave 28; since wave 28,
// under `--jdk-only`, it asks a loader that overrides `loadClass` (and still
// skips one whose delegation chain overrides nothing, which cannot answer
// differently: `classloader_real::loader_answers_jdk_names_as_the_jdk`).
// The `asked` rows are the positive control of that drive
// (`constants.rs` `drive_loader_for_global_name`).
//
// Run (no setup):
//   javac -d out L5W28JdkNamedModule.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W28JdkNamedModule
//
// Expected HotSpot 25 output (compare verbatim):
//   own class loader is user loader: true
//   own class module named: false
//   own class in loader's unnamed module: true
//   asked for jdk name (overriding loader): true
//   jdk name answer is java.base's: true
//
// CratonVM before wave 28 (from the code, not run): rows 2-4 `true`, `false`,
// `false`. After wave 28 `--jdk-only` matches HotSpot; `--compatible` keeps
// the old rows 2-4 (unchanged mode).

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.function.Supplier;

public class L5W28JdkNamedModule {
    static final String CRED = "javax.security.auth.x500.X500PrivateCredential";
    static final String PRINCIPAL = "javax.security.auth.x500.X500Principal";
    static final String USER = "l5m.User";

    static byte[] plain(String name) {
        return ClassFile.of().build(ClassDesc.of(name), clb -> clb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_()));
    }

    /// `l5m.User implements Supplier`: `get()` returns `X500Principal.class`
    /// (an `ldc` of the JDK name, resolved through User's loader).
    static byte[] user() {
        ClassDesc self = ClassDesc.of(USER);
        return ClassFile.of().build(self, clb -> clb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.Supplier"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("get", MethodTypeDesc.of(ConstantDescs.CD_Object), ClassFile.ACC_PUBLIC,
                        cb -> cb.ldc(ClassDesc.of(PRINCIPAL)).areturn()));
    }

    /// Child-first for `CRED` and `USER`; parent-first (the platform loader)
    /// for everything else, recording whether it was asked for `PRINCIPAL`.
    static final class Loader extends ClassLoader {
        volatile boolean askedPrincipal;

        Loader() {
            super(ClassLoader.getPlatformClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.equals(PRINCIPAL)) {
                    askedPrincipal = true;
                }
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = name.equals(CRED) ? plain(CRED) : name.equals(USER) ? user() : null;
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        // java.base's copies first, as an application would have them.
        Class<?> jdkPrincipal = Class.forName(PRINCIPAL);
        Class.forName(CRED);
        Loader loader = new Loader();
        Class<?> own = loader.loadClass(CRED);
        System.out.println("own class loader is user loader: " + (own.getClassLoader() == loader));
        System.out.println("own class module named: " + own.getModule().isNamed());
        System.out.println("own class in loader's unnamed module: "
                + (own.getModule() == loader.getUnnamedModule()));
        @SuppressWarnings("unchecked")
        Supplier<Object> user = (Supplier<Object>) loader.loadClass(USER).getConstructor().newInstance();
        Object answer = user.get();
        System.out.println("asked for jdk name (overriding loader): " + loader.askedPrincipal);
        System.out.println("jdk name answer is java.base's: " + (answer == jdkPrincipal));
    }
}
