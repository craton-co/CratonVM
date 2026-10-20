// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L4: class initialization requested
// through a Class MIRROR initializes that exact class (its defining loader is
// part of its identity, JVMS §5.3), never a same-named class of another
// loader. Each row loads a fresh copy of `L4W29EnsureInitializedByMirror$Dup`
// through its own child-first loader (named rowN) without initializing it,
// then asks for initialization through one door; `Dup.<clinit>` prints its
// defining loader's name.
//
// The JDK's doors here -- `Lookup.ensureInitialized`, the `DirectMethodHandle`
// init barrier (rows 2 / 5), `MethodHandleAccessorFactory` (rows 3 / 4) and
// `LazyInitializingVarHandle` (row 7) -- end in `Unsafe.ensureClassInitialized0`
// (`native-builtins/src/unsafe_natives_ext.rs`
// `native_unsafe_ensure_class_initialized`), which turned the mirror into a
// NAME and initialized whatever that name resolved to from the calling
// frames: the application class path's copy, or nothing for a hidden class
// (row 6). Wherever CratonVM runs one of those JDK bodies under `--jdk-only`,
// the row printed "Dup.<clinit> loader=app" and the final `app` row printed
// no clinit line. The native now initializes the mirror's own class id.
// Which door answers a row first is decided outside this native: the
// registered `Lookup.ensureInitialized` Bridge (`lang_invoke.rs`, which
// already initializes by id) answers row 1 when it wins step 1. Read from the
// code, not run; the orchestrator's host run is the record. `--compatible`
// keeps the native's by-name route (not changed).
//
// Run: cratonvm --java-home <jdk25> [--nojit] -cp <dir> L4W29EnsureInitializedByMirror
//
// HotSpot 25 (25.0.3, default and -Xint) prints exactly:
//   row1: loaded by row1
//   row1: Lookup.ensureInitialized
//     Dup.<clinit> loader=row1
//   row2: loaded by row2
//   row2: findStatic done, invoking
//     Dup.<clinit> loader=row2
//   row2: v()=42
//   row3: loaded by row3
//   row3: Field.get
//     Dup.<clinit> loader=row3
//   row3: X=5
//   row4: loaded by row4
//   row4: Method.invoke
//     Dup.<clinit> loader=row4
//   row4: v()=42
//   row5: loaded by row5
//   row5: findStaticGetter done, invoking
//     Dup.<clinit> loader=row5
//   row5: X=5
//   row7: loaded by row7
//   row7: findStaticVarHandle done, get
//     Dup.<clinit> loader=row7
//   row7: X=5
//   row6: hidden defined, Lookup.ensureInitialized
//     Hid.<clinit> hidden=true
//   row6: returned
//   app Dup initialized by now? touching it:
//     Dup.<clinit> loader=app
//   app: X=5

import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W29EnsureInitializedByMirror {
    public static class Dup {
        public static int X = 5;
        static {
            ClassLoader l = Dup.class.getClassLoader();
            System.out.println("  Dup.<clinit> loader=" + (l == null ? "boot" : l.getName()));
        }
        public static int v() { return 42; }
    }

    public static class Hid {
        static {
            System.out.println("  Hid.<clinit> hidden=" + Hid.class.isHidden());
        }
    }

    static final class ChildFirst extends ClassLoader {
        private final byte[] bytes;
        ChildFirst(String name, byte[] bytes) {
            super(name, L4W29EnsureInitializedByMirror.class.getClassLoader());
            this.bytes = bytes;
        }
        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals("L4W29EnsureInitializedByMirror$Dup")) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) c = defineClass(name, bytes, 0, bytes.length);
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    static byte[] dupBytes() throws Exception {
        try (InputStream in = L4W29EnsureInitializedByMirror.class.getResourceAsStream("L4W29EnsureInitializedByMirror$Dup.class")) {
            return in.readAllBytes();
        }
    }

    static Class<?> fresh(String row, byte[] b) throws Exception {
        Class<?> c = Class.forName("L4W29EnsureInitializedByMirror$Dup", false, new ChildFirst(row, b));
        System.out.println(row + ": loaded by " + c.getClassLoader().getName());
        return c;
    }

    public static void main(String[] a) throws Throwable {
        byte[] b = dupBytes();
        MethodHandles.Lookup lk = MethodHandles.lookup();

        Class<?> c1 = fresh("row1", b);
        System.out.println("row1: Lookup.ensureInitialized");
        lk.ensureInitialized(c1);

        Class<?> c2 = fresh("row2", b);
        MethodHandle mh = lk.findStatic(c2, "v", MethodType.methodType(int.class));
        System.out.println("row2: findStatic done, invoking");
        System.out.println("row2: v()=" + (int) mh.invokeExact());

        Class<?> c3 = fresh("row3", b);
        System.out.println("row3: Field.get");
        System.out.println("row3: X=" + c3.getField("X").getInt(null));

        Class<?> c4 = fresh("row4", b);
        System.out.println("row4: Method.invoke");
        System.out.println("row4: v()=" + c4.getMethod("v").invoke(null));

        Class<?> c5 = fresh("row5", b);
        MethodHandle g = lk.findStaticGetter(c5, "X", int.class);
        System.out.println("row5: findStaticGetter done, invoking");
        System.out.println("row5: X=" + (int) g.invokeExact());

        Class<?> c7 = fresh("row7", b);
        java.lang.invoke.VarHandle vh = lk.findStaticVarHandle(c7, "X", int.class);
        System.out.println("row7: findStaticVarHandle done, get");
        System.out.println("row7: X=" + (int) vh.get());

        byte[] hb;
        try (InputStream in = L4W29EnsureInitializedByMirror.class.getResourceAsStream("L4W29EnsureInitializedByMirror$Hid.class")) {
            hb = in.readAllBytes();
        }
        Class<?> hc = lk.defineHiddenClass(hb, false).lookupClass();
        System.out.println("row6: hidden defined, Lookup.ensureInitialized");
        try {
            lk.ensureInitialized(hc);
            System.out.println("row6: returned");
        } catch (Throwable t) {
            System.out.println("row6: " + t);
        }

        System.out.println("app Dup initialized by now? touching it:");
        System.out.println("app: X=" + Dup.X);
    }
}
