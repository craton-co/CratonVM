// Interpreter round i1, wave 27, lane L5 — what a user class loader is told
// about its own parallel capability, and which lock its `loadClass` takes.
//
// `ClassLoader.<init>` sets `parallelLockMap` (and `assertionLock`) from
// `ParallelLoaders.isRegistered(getClass())`: a loader class that did not call
// `registerAsParallelCapable()` in its static initializer gets
// `getClassLoadingLock(name) == this` (every `loadClass` locks the loader, and
// HotSpot's VM-initiated loads lock the same object — `SystemDictionary`'s
// `ObjectLocker` for a non-parallel-capable loader); a registered one gets a
// per-name lock object. `isRegisteredAsParallelCapable()` reads the same set.
//
// This is the signal the loader lock of
// docs/internal/fixed-bugs/interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md
// needs, and why it cannot be added yet: CratonVM's real-JDK `ClassLoader`
// constructors are `Bridge` natives (`native-builtins/src/classloader_real.rs`
// `register_classloader_real_natives` -> `init_classloader_common_fields`)
// that allocate `parallelLockMap` for EVERY loader, so rows 2 and 4 read
// `false` there, while row 1/3 (`isRegisteredAsParallelCapable`, real bytecode
// on a real image, whose `registerAsParallelCapable` shadow was retired for
// `--jdk-only` on 2026-09-10) are expected to be right under `--jdk-only`.
//
// Run (no setup):
//   javac -d out L5W27LoaderLockShape.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W27LoaderLockShape
//
// Expected HotSpot 25 output (compare verbatim):
//   plain registered: false
//   plain lock is loader: true
//   parallel registered: true
//   parallel lock is loader: false
//
// CratonVM (from the code, not run): `--jdk-only` rows 1 and 3 as HotSpot,
// row 2 `false` (per-name lock); `--compatible` additionally row 3 `false`
// (its `registerAsParallelCapable` native answers `true` without registering).
// Wave 28 (lane L5): the constructor bridges ask `ParallelLoaders.isRegistered`
// under `--jdk-only`, so all four rows are expected as HotSpot there;
// `--compatible` is unchanged (rows 2 and 3 `false`).

public class L5W27LoaderLockShape {
    static final class Plain extends ClassLoader {
        Plain() {
            super(ClassLoader.getPlatformClassLoader());
        }

        boolean lockIsLoader() {
            return getClassLoadingLock("l5w27.X") == this;
        }
    }

    static final class Parallel extends ClassLoader {
        static {
            registerAsParallelCapable();
        }

        Parallel() {
            super(ClassLoader.getPlatformClassLoader());
        }

        boolean lockIsLoader() {
            return getClassLoadingLock("l5w27.X") == this;
        }
    }

    public static void main(String[] args) {
        Plain plain = new Plain();
        Parallel parallel = new Parallel();
        System.out.println("plain registered: " + plain.isRegisteredAsParallelCapable());
        System.out.println("plain lock is loader: " + plain.lockIsLoader());
        System.out.println("parallel registered: " + parallel.isRegisteredAsParallelCapable());
        System.out.println("parallel lock is loader: " + parallel.lockIsLoader());
    }
}
