// Interpreter round i1, wave 28, lane L5 — the built-in loaders are
// parallel-capable, so their class-loading lock is a per-name object, never
// the loader itself.
//
// `BuiltinClassLoader` and both of its subclasses call
// `registerAsParallelCapable()` in their static initializers, and JDK 25's
// `ClassLoader` constructor then gives the application and platform loaders a
// `parallelLockMap`, so `getClassLoadingLock(name)` answers a per-name lock
// and `BuiltinClassLoader.loadClassOrNull` never serializes all class loading
// on the loader. CratonVM allocates both loaders without running a constructor
// (`native-builtins/src/classloader.rs` `alloc_classloader`), which set
// `assertionLock` but not `parallelLockMap`, so the lock was the loader.
//
// `getClassLoadingLock` is protected in `java.base`; the probe needs
// `--add-opens java.base/java.lang=ALL-UNNAMED` on both VMs.
//
// Run:
//   javac -d out L5W28BuiltinLoaderLock.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] --add-opens java.base/java.lang=ALL-UNNAMED -cp out L5W28BuiltinLoaderLock
//
// Expected HotSpot 25 output (compare verbatim):
//   app registered: true
//   app lock is loader: false
//   platform registered: true
//   platform lock is loader: false
//
// CratonVM before wave 28 (from the code, not run): rows 2 and 4 `true`.
// After wave 28 `--jdk-only` is expected to match HotSpot. `--compatible` is
// unchanged: rows 2 and 4 `true`, and rows 1 and 3 `false` (that mode's
// `registerAsParallelCapable` natives never register a class).

import java.lang.reflect.Method;

public class L5W28BuiltinLoaderLock {
    public static void main(String[] args) throws Exception {
        Method lock = ClassLoader.class.getDeclaredMethod("getClassLoadingLock", String.class);
        lock.setAccessible(true);
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        System.out.println("app registered: " + app.isRegisteredAsParallelCapable());
        System.out.println("app lock is loader: " + (lock.invoke(app, "l5w28.X") == app));
        System.out.println("platform registered: " + platform.isRegisteredAsParallelCapable());
        System.out.println("platform lock is loader: "
                + (lock.invoke(platform, "l5w28.X") == platform));
    }
}
