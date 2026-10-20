// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import java.util.function.Supplier;

/**
 * gci i32/sl: a lone-surrogate String LITERAL dies with its class.
 * Page {@code docs/known-issues/gc/gci-i20mr-merge-review-findings-20261015.md}
 * finding 10: the literal's surrogate-pool row was STRONG for the life of the
 * VM, where HotSpot holds a literal only through its class's resolved
 * references (and weakly through its StringTable), so after the class unloads
 * the literal is collected.
 *
 * <p>{@link Lit} returns the literal {@code "\uD800sl32"}; it is defined by
 * throw-away class loaders (its bytes are read as a resource). This class never
 * names the literal itself (it builds the text from chars), or its own
 * constant pool would keep it alive.
 *
 * <p>Deterministic stdout (HotSpot 25 prints exactly this):
 * <pre>
 *   sl32 identity: shared=true canonical=true
 *   sl32 kept: true
 *   sl32 unloaded=true literal-cleared=true
 *   sl32 reloaded: canonical=true
 * </pre>
 * {@code identity}: two classes' literals are one object and the intern pool's.
 * {@code kept}: while one defining class lives, System.gc() keeps the literal.
 * {@code unloaded}: every loader that defined {@link Lit} is collected;
 * {@code literal-cleared}: a WeakReference to the literal clears (within 10
 * System.gc() rounds). {@code reloaded}: a new class's literal is canonical
 * again. Stderr (not compared): {@code [sl32] rounds=R}.
 */
public final class GciSl32SurrogateLiteralUnloadProbe {

    /** Defined once per loader. Holds the only copy of the literal. */
    public static final class Lit implements Supplier<String> {
        public Lit() {
        }

        @Override
        public String get() {
            return "\uD800sl32";
        }
    }

    /** Defines {@link Lit} itself and delegates everything else. */
    static final class LitLoader extends ClassLoader {
        private final String own;
        private final byte[] bytes;

        LitLoader(String own, byte[] bytes) {
            super(GciSl32SurrogateLiteralUnloadProbe.class.getClassLoader());
            this.own = own;
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(own)) {
                synchronized (this) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        c = defineClass(name, bytes, 0, bytes.length);
                    }
                    if (resolve) {
                        resolveClass(c);
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    static String litName;
    static byte[] litBytes;
    static Supplier<String> keepA;

    static String text() {
        return new String(new char[] {'\uD800', 's', 'l', '3', '2'});
    }

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in =
                GciSl32SurrogateLiteralUnloadProbe.class.getClassLoader().getResourceAsStream(res)) {
            if (in == null) {
                throw new IllegalStateException("class bytes not on the classpath: " + res);
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    @SuppressWarnings("unchecked")
    static Supplier<String> define() throws Exception {
        LitLoader loader = new LitLoader(litName, litBytes);
        Class<?> c = loader.loadClass(litName);
        if (c.getClassLoader() != loader) {
            throw new IllegalStateException("Lit was not defined by its own loader");
        }
        return (Supplier<String>) c.getConstructor().newInstance();
    }

    /** Phase 1: two classes, one kept in {@link #keepA}. Answers loader B weakly. */
    static WeakReference<ClassLoader> identity() throws Exception {
        Supplier<String> a = define();
        Supplier<String> b = define();
        String la = a.get();
        String lb = b.get();
        String interned = text().intern();
        System.out.println("sl32 identity: shared=" + (la == lb) + " canonical=" + (la == interned));
        keepA = a;
        return new WeakReference<>(b.getClass().getClassLoader());
    }

    static WeakReference<String> literalWeakly() {
        return new WeakReference<>(keepA.get());
    }

    static WeakReference<ClassLoader> loaderAWeakly() {
        return new WeakReference<>(keepA.getClass().getClassLoader());
    }

    static boolean stillTheLiteral(WeakReference<String> w) {
        String s = w.get();
        return s != null && s == keepA.get();
    }

    static String reloaded() throws Exception {
        Supplier<String> c = define();
        String lc = c.get();
        return "sl32 reloaded: canonical=" + (lc == text().intern());
    }

    public static void main(String[] args) throws Exception {
        litName = GciSl32SurrogateLiteralUnloadProbe.class.getName() + "$Lit";
        litBytes = readClassBytes(litName);

        WeakReference<ClassLoader> loaderB = identity();
        WeakReference<String> lit = literalWeakly();
        for (int r = 0; r < 3; r++) {
            System.gc();
        }
        System.out.println("sl32 kept: " + stillTheLiteral(lit));

        WeakReference<ClassLoader> loaderA = loaderAWeakly();
        keepA = null;
        int rounds = 0;
        boolean unloaded = false;
        boolean cleared = false;
        for (int r = 0; r < 10; r++) {
            System.gc();
            rounds++;
            unloaded = loaderA.refersTo(null) && loaderB.refersTo(null);
            cleared = lit.refersTo(null);
            if (unloaded && cleared) {
                break;
            }
            Thread.sleep(20);
        }
        System.out.println("sl32 unloaded=" + unloaded + " literal-cleared=" + cleared);
        System.err.println("[sl32] rounds=" + rounds);
        System.out.println(reloaded());
    }
}
