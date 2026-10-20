// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L3 -- item 3 of
// docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007.md:
// HotSpot's `Reflection::new_method` / `new_constructor` / `new_field`
// resolve every listed member's signature types (parameters, return,
// `throws`, a field's type) through the HOLDER's loader, so a listing asks
// that loader for each type it has not initiated yet, and a type the loader
// cannot find fails the listing with `NoClassDefFoundError` (its cause the
// loader's `ClassNotFoundException`).
//
// The holder classes are defined by a logging loader (`Log`, a child of the
// application loader that defines every `L3W54ReflectionLoadsSignatureTypes$*`
// class itself and refuses `...$Gone`). Each row prints the names the
// listing asked `Log.loadClass` for (only `java.util.concurrent.*` and the
// probe's own classes, sorted, deduplicated), or the throwable's class,
// message, cause and the frames above the probe's first frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   methods       -- `Uses.getDeclaredMethods()` (public `use(Exchanger)`,
//                    `phaser()` returning `Phaser`, `throws BrokenBarrierException`);
//   ctors         -- `Uses.getDeclaredConstructors()` (`Uses(Semaphore)`);
//   fields        -- `Uses.getDeclaredFields()` (`CountDownLatch latch`);
//   public-only   -- `Mixed.getMethods()`: a public method's types are asked,
//                    a private one's (`LinkedTransferQueue`) are not;
//   missing       -- `Holder.getDeclaredMethods()`, a method taking `Gone`;
//   missing-field -- `FieldHolder.getFields()`, a field of type `Gone`;
//   methods-again -- `Uses2.getDeclaredMethods()` (the `methods` shape on a
//                    fresh class).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// methods: 2 asked [java.util.concurrent.BrokenBarrierException, java.util.concurrent.Exchanger, java.util.concurrent.Phaser]
// ctors: 1 asked [java.util.concurrent.Semaphore]
// fields: 1 asked [java.util.concurrent.CountDownLatch]
// public-only: 10 asked [java.util.concurrent.CyclicBarrier]
// missing: java.lang.NoClassDefFoundError L3W54ReflectionLoadsSignatureTypes$Gone cause=java.lang.ClassNotFoundException: L3W54ReflectionLoadsSignatureTypes$Gone [java.lang.Class.getDeclaredMethods0 java.lang.Class.privateGetDeclaredMethods java.lang.Class.getDeclaredMethods] asked [$Gone]
// missing-field: java.lang.NoClassDefFoundError L3W54ReflectionLoadsSignatureTypes$Gone cause=java.lang.ClassNotFoundException: L3W54ReflectionLoadsSignatureTypes$Gone [java.lang.Class.getDeclaredFields0 java.lang.Class.privateGetDeclaredFields java.lang.Class.privateGetPublicFields java.lang.Class.getFields] asked [$Gone]
// methods-again: 2 asked [java.util.concurrent.BrokenBarrierException, java.util.concurrent.Exchanger, java.util.concurrent.Phaser]
// HOTSPOT_EXPECTED_END
// (A packaged type's message is its internal name, `pk/Gone`: measured.)
//
// CratonVM before wave 54: a listing never asks the holder's loader (a type
// is resolved by name, falling back to a synthetic mirror), so every row
// prints `asked []` and the two missing rows list without failing. Since
// wave 54, under `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES` (default off
// until 2026-10-07, on since; `=0` turns it off), the listing resolves each
// type through the holder's loader first
// (`lang_class::resolve_reflected_signature_types`); since 2026-10-07 every
// row, its frames included, prints HotSpot's line in both modes.
// Positive control: `CRATONVM_DBG_REFLECT_SIGNATURE_TYPES=1` prints one
// `[reflect-sig] <holder>: <n> type(s) resolved` line per listing.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W54ReflectionLoadsSignatureTypes`.
import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;

public class L3W54ReflectionLoadsSignatureTypes {
    public static class Gone {}

    public static class Uses {
        public java.util.concurrent.CountDownLatch latch;
        public Uses(java.util.concurrent.Semaphore s) {}
        public void use(java.util.concurrent.Exchanger<?> e) {}
        public java.util.concurrent.Phaser phaser() throws java.util.concurrent.BrokenBarrierException {
            return null;
        }
    }

    public static class Uses2 {
        public void use(java.util.concurrent.Exchanger<?> e) {}
        public java.util.concurrent.Phaser phaser() throws java.util.concurrent.BrokenBarrierException {
            return null;
        }
    }

    public static class Mixed {
        public void pub(java.util.concurrent.CyclicBarrier b) {}
        private void priv(java.util.concurrent.LinkedTransferQueue<?> q) {}
    }

    public static class Holder {
        public void take(Gone g) {}
        public int plain() { return 1; }
    }

    public static class FieldHolder {
        public Gone g;
        public int x;
    }

    static final String PREFIX = "L3W54ReflectionLoadsSignatureTypes$";

    /** Defines the probe's nested classes itself; refuses `Gone`; logs. */
    static final class Log extends ClassLoader {
        final List<String> asked = new ArrayList<>();

        Log(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.startsWith("java.util.concurrent.") || name.startsWith(PREFIX)) {
                asked.add(name);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null && name.startsWith(PREFIX)) {
                    if (name.equals(PREFIX + "Gone")) {
                        throw new ClassNotFoundException(name);
                    }
                    String res = name + ".class";
                    try (InputStream in = getParent().getResourceAsStream(res)) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                if (c == null) {
                    c = super.loadClass(name, false);
                }
                return c;
            }
        }

        String take() {
            TreeSet<String> s = new TreeSet<>();
            for (String n : asked) {
                if (!n.startsWith(PREFIX + "Uses") && !n.startsWith(PREFIX + "Mixed")
                        && !n.startsWith(PREFIX + "Holder") && !n.startsWith(PREFIX + "FieldHolder")) {
                    s.add(n.startsWith(PREFIX) ? n.substring(PREFIX.length() - 1) : n);
                }
            }
            asked.clear();
            return s.toString();
        }
    }

    interface Listing {
        int run(Class<?> c) throws Throwable;
    }

    static void row(String name, String holder, Listing l) {
        Log log = new Log(L3W54ReflectionLoadsSignatureTypes.class.getClassLoader());
        Class<?> c;
        try {
            c = Class.forName(PREFIX + holder, false, log);
        } catch (Throwable t) {
            System.out.println(name + ": setup " + t);
            return;
        }
        log.take();
        try {
            int n = l.run(c);
            System.out.println(name + ": " + n + " asked " + log.take());
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int i = 0;
            while (i < st.length && !st[i].getClassName().startsWith("L3W54")) {
                jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
                i++;
            }
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " cause="
                    + t.getCause() + " [" + jdk + "] asked " + log.take());
        }
    }

    public static void main(String[] args) {
        row("methods", "Uses", c -> c.getDeclaredMethods().length);
        row("ctors", "Uses", c -> c.getDeclaredConstructors().length);
        row("fields", "Uses", c -> c.getDeclaredFields().length);
        row("public-only", "Mixed", c -> c.getMethods().length);
        row("missing", "Holder", c -> c.getDeclaredMethods().length);
        row("missing-field", "FieldHolder", c -> c.getFields().length);
        row("methods-again", "Uses2", c -> c.getDeclaredMethods().length);
    }
}
