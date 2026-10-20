// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L6: `MethodHandles.privateLookupIn`'s
// module half (same module? `canRead`? `isOpen(pn, callerModule)`?) in both
// modes, the caller the proposal
// `docs/internal/proposals/interpreter/i45-L5-proposal-one-module-access-question-by-identity-20261009.md`
// moves onto the one module question by identity
// (`CRATONVM_MODULE_ACCESS_BY_IDENTITY`; `classloader.rs`
// `private_lookup_in_by_identity`, `access_control::module_access`).
//
// Rows (each: the lookup class, its `lookupModes()`, its
// `previousLookupClass()`; or the exception and its message with the
// unnamed module's identity hash elided):
//   same-class     the caller's own class
//   same-module    another class of the caller's loader (same unnamed module)
//   other-loader   a copy of a class defined by another loader (another
//                  unnamed module: cross-module, the target opens everything)
//   java-lang      Object.class (java.base does not open java.lang)
//   java-util      ArrayList.class (exported, not open)
//   jdk-internal   jdk.internal.misc.Unsafe (neither exported nor open)
//
// HotSpot 25.0.3 prints (the same with -Xint; measured):
//     same-class=L6W47PrivateLookupInByIdentity modes=31 prev=null
//     same-module=L6W47PrivateLookupInByIdentity$Mate modes=31 prev=null
//     other-loader=L6W47PrivateLookupInByIdentity$Mate modes=15 prev=L6W47PrivateLookupInByIdentity other-loader=true
//     java-lang=java.lang.IllegalAccessException: module java.base does not open java.lang to unnamed module @HASH
//     java-util=java.lang.IllegalAccessException: module java.base does not open java.util to unnamed module @HASH
//     jdk-internal=java.lang.IllegalAccessException: module java.base does not open jdk.internal.misc to unnamed module @HASH
//
// CratonVM `--compatible` before wave 47 answered every row as the same
// module (`modes=31 prev=null`, no exception). Positive control (switch on):
// `CRATONVM_DBG=access` prints `[ACCESS-DBG] privateLookupIn by identity: ...`
// with `SameModule` for the first two rows, `CrossModule` for `other-loader`
// and `NotOpen` for the last three.
//
//     javac -d out L6W47PrivateLookupInByIdentity.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W47PrivateLookupInByIdentity
import java.lang.invoke.MethodHandles;
import java.io.InputStream;

public class L6W47PrivateLookupInByIdentity {
    public static class Mate { }

    /** Defines its own copy of {@link Mate}; delegates everything else. */
    static final class Isolating extends ClassLoader {
        Isolating() {
            super(L6W47PrivateLookupInByIdentity.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals("L6W47PrivateLookupInByIdentity$Mate")) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                    byte[] b = in.readAllBytes();
                    return defineClass(name, b, 0, b.length);
                } catch (java.io.IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    interface Target { Class<?> get() throws Throwable; }

    static void row(String label, Target target) {
        try {
            Class<?> c = target.get();
            MethodHandles.Lookup l = MethodHandles.privateLookupIn(c, MethodHandles.lookup());
            Class<?> prev = l.previousLookupClass();
            String extra = c.getClassLoader() != L6W47PrivateLookupInByIdentity.class.getClassLoader()
                    ? " other-loader=true" : "";
            System.out.println(label + "=" + l.lookupClass().getName() + " modes=" + l.lookupModes()
                    + " prev=" + (prev == null ? "null" : prev.getName()) + extra);
        } catch (Throwable t) {
            String m = String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@HASH");
            System.out.println(label + "=" + t.getClass().getName() + ": " + m);
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader other = new Isolating();
        row("same-class", () -> L6W47PrivateLookupInByIdentity.class);
        row("same-module", () -> Mate.class);
        row("other-loader", () -> Class.forName("L6W47PrivateLookupInByIdentity$Mate", false, other));
        row("java-lang", () -> Object.class);
        row("java-util", () -> java.util.ArrayList.class);
        row("jdk-internal", () -> Class.forName("jdk.internal.misc.Unsafe"));
    }
}
