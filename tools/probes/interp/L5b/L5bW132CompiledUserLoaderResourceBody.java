// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 132, lane L5b: `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED`
// (default off; only with `CRATONVM_USER_LOADER_RESOURCE_BYTECODE`), the compiled half of
// docs/internal/proposals/interpreter/i73-L5-proposal-user-loader-get-resource-without-the-native-hop-20261106.md.
// A COMPILED caller's `l.getResource(name)` / `getResources` / `getResourceAsStream` on a
// user-defined loader (or a bare URLClassLoader) took the by-name tail, whose `Bridge` native
// re-entered the JDK's own `ClassLoader` body; with both switches it enters that body through the
// JIT helpers' per-(site, receiver class) frame template. The answers must not move.
//
// Each row runs its calls from ONE call site in `burst`, 3 rounds x 2 000 calls (the application loader row 3 x 200), so the caller
// is compiled and the site sees every receiver class of the row.
//
//   bare-url      a bare URLClassLoader (parent null): getResource / getResources go to
//                 ClassLoader's body; getResourceAsStream is URLClassLoader's own (control)
//   find-override a ClassLoader subclass overriding findResource(s) only: the body asks it,
//                 counted exactly (finds == calls of all three methods)
//   get-override  a ClassLoader subclass overriding getResource (control: its own code runs,
//                 counted exactly; the other two methods still reach findResource(s))
//   app-loader    the application loader (control: keeps the native)
//   poly          one site, the four receivers above in turn
//   null-name     getResource(null) from the compiled site: NullPointerException every call
//
// Bench (TIME lines): `find-get` (expected down with both switches), `app-get` flat (control).
//
// javac -d <dir> L5bW132CompiledUserLoaderResourceBody.java
// for m in --jdk-only --compatible; do for j in "" --nojit; do
//   cratonvm $m $j -cp <dir> L5bW132CompiledUserLoaderResourceBody
//   CRATONVM_USER_LOADER_RESOURCE_BYTECODE=1 CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED=1 \
//     cratonvm $m $j -cp <dir> L5bW132CompiledUserLoaderResourceBody
// done; done
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint; TIME lines excluded)
// bare-url get=file:...L5bW132CompiledUserLoaderResourceBody.class resources=1 stream=1 calls=6000
// find-override get=file:...thing.txt resources=0 stream=-1 calls=6000 finds=18000
// get-override get=null resources=0 stream=-1 calls=6000 own-gets=12000 finds=6000
// app-loader get=file:...L5bW132CompiledUserLoaderResourceBody.class resources=1 stream=1 calls=600
// poly bare=5000 find=5000 get=5000 app=5000 nonnull=15000 finds=5000 own-gets=5000
// null-name npe=20000
// bench find-get calls=100000
// bench app-get calls=500
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.util.Collections;
import java.util.Enumeration;

public class L5bW132CompiledUserLoaderResourceBody {
    static final String SELF = "L5bW132CompiledUserLoaderResourceBody.class";
    static final String THING = "virtual/thing.txt";

    static final class FindOverride extends ClassLoader {
        int finds;

        FindOverride() {
            super(null);
        }

        @Override
        protected URL findResource(String name) {
            finds++;
            if (name.equals(THING)) {
                try {
                    return new URL("file:/virtual/thing.txt");
                } catch (IOException e) {
                    throw new AssertionError(e);
                }
            }
            return null;
        }

        @Override
        protected Enumeration<URL> findResources(String name) {
            finds++;
            return Collections.emptyEnumeration();
        }
    }

    static final class GetOverride extends ClassLoader {
        int ownGets;
        int finds;

        GetOverride() {
            super(null);
        }

        @Override
        public URL getResource(String name) {
            ownGets++;
            return null;
        }

        @Override
        protected URL findResource(String name) {
            finds++;
            return null;
        }

        @Override
        protected Enumeration<URL> findResources(String name) {
            finds++;
            return Collections.emptyEnumeration();
        }
    }

    static String tail(URL u) {
        if (u == null) {
            return "null";
        }
        String s = u.toString();
        return u.getProtocol() + ":..." + s.substring(s.lastIndexOf('/') + 1);
    }

    static int count(Enumeration<URL> e) {
        int n = 0;
        while (e.hasMoreElements()) {
            e.nextElement();
            n++;
        }
        return n;
    }

    static int readAll(InputStream in) throws IOException {
        if (in == null) {
            return -1;
        }
        try (in) {
            return in.readAllBytes().length > 0 ? 1 : 0;
        }
    }

    // The one call site per method every row uses.
    static String last;
    static int lastResources;
    static int lastStream;

    static int burst(ClassLoader l, String name, int n) throws IOException {
        int calls = 0;
        for (int i = 0; i < n; i++) {
            last = tail(l.getResource(name));
            lastResources = count(l.getResources(name));
            lastStream = readAll(l.getResourceAsStream(name));
            calls++;
        }
        return calls;
    }

    static String rows(String label, ClassLoader l, String name) throws IOException {
        return rows(label, l, name, 2000);
    }

    static String rows(String label, ClassLoader l, String name, int perRound) throws IOException {
        int calls = 0;
        for (int r = 0; r < 3; r++) {
            calls += burst(l, name, perRound);
        }
        return label + " get=" + last + " resources=" + lastResources + " stream=" + lastStream
                + " calls=" + calls;
    }

    // Polymorphic: one site, four receiver classes in turn.
    static int nonNull;

    static int polyGet(ClassLoader l, String name) {
        URL u = l.getResource(name);
        if (u != null) {
            nonNull++;
        }
        return 1;
    }

    static int nullName(ClassLoader l) {
        try {
            l.getResource(null);
            return 0;
        } catch (NullPointerException e) {
            return 1;
        }
    }

    static int benchGet(ClassLoader l, String name, int n) {
        int calls = 0;
        for (int i = 0; i < n; i++) {
            if (l.getResource(name) != null || i >= 0) {
                calls++;
            }
        }
        return calls;
    }

    public static void main(String[] args) throws Exception {
        URL dir = Path.of(L5bW132CompiledUserLoaderResourceBody.class.getProtectionDomain()
                .getCodeSource().getLocation().toURI()).toUri().toURL();
        URLClassLoader bare = new URLClassLoader(new URL[] {dir}, null);
        ClassLoader app = L5bW132CompiledUserLoaderResourceBody.class.getClassLoader();

        System.out.println(rows("bare-url", bare, SELF));

        FindOverride find = new FindOverride();
        System.out.println(rows("find-override", find, THING) + " finds=" + find.finds);

        GetOverride get = new GetOverride();
        System.out.println(rows("get-override", get, SELF) + " own-gets=" + get.ownGets
                + " finds=" + get.finds);

        // The application loader is the slow control (a file open per stream): fewer calls.
        System.out.println(rows("app-loader", app, SELF, 200));

        FindOverride pFind = new FindOverride();
        GetOverride pGet = new GetOverride();
        ClassLoader[] ring = {bare, pFind, pGet, app};
        String[] names = {SELF, THING, SELF, SELF};
        int[] per = new int[4];
        nonNull = 0;
        for (int i = 0; i < 20000; i++) {
            int k = i & 3;
            per[k] += polyGet(ring[k], names[k]);
        }
        System.out.println("poly bare=" + per[0] + " find=" + per[1] + " get=" + per[2]
                + " app=" + per[3] + " nonnull=" + nonNull + " finds=" + pFind.finds
                + " own-gets=" + pGet.ownGets);

        int npe = 0;
        FindOverride nFind = new FindOverride();
        for (int i = 0; i < 20000; i++) {
            npe += nullName((i & 1) == 0 ? bare : nFind);
        }
        System.out.println("null-name npe=" + npe);

        FindOverride bFind = new FindOverride();
        for (String label : new String[] {"find-get", "app-get"}) {
            boolean findRow = label.equals("find-get");
            ClassLoader l = findRow ? bFind : app;
            String name = findRow ? THING : SELF;
            // The app loader's lookup is ~100x dearer; fewer calls keep the run short.
            int n = findRow ? 100000 : 500;
            benchGet(l, name, n / 5);
            long t0 = System.nanoTime();
            int calls = benchGet(l, name, n);
            long t1 = System.nanoTime();
            System.out.println("TIME " + label + " ns/op=" + ((t1 - t0) / n));
            System.out.println("bench " + label + " calls=" + calls);
        }
    }
}
