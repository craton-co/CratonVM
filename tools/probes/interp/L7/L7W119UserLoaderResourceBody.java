// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 119, lane L7: `CRATONVM_USER_LOADER_RESOURCE_BYTECODE` (default off;
// the L7 half of
// docs/internal/proposals/interpreter/i73-L5-proposal-user-loader-get-resource-without-the-native-hop-20261106.md).
// For a user-defined loader (or a bare URLClassLoader) the `ClassLoader.getResource` /
// `getResources` / `getResourceAsStream` `Bridge` natives only re-enter the JDK's own body; with
// the switch the interpreter's virtual-dispatch cache files that body for the receiver class.
// Rows (each call made 2 000 times at one call site, so the cache entry is hit):
//
//   bare-url        a bare URLClassLoader over this probe's directory, parent null:
//                   getResource / getResources / getResourceAsStream of this probe's .class
//   sub-url         a URLClassLoader subclass (a user loader class)
//   find-override   a ClassLoader subclass overriding only findResource: the JDK body asks it
//   get-override    a ClassLoader subclass overriding getResource itself (control: its own code)
//   app-loader      the application loader (control: stays on the native)
//   missing         a name no loader has: null / empty / null, all four loaders
//
// Bench (TIME lines): `bare-get` and `sub-get` (expected down with the switch: no native funnel
// pass and no re-entry per call), `app-get` flat (control).
//
// Run, switch off and on, --jdk-only and --compatible, JIT and --nojit:
//   cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W119UserLoaderResourceBody
//   CRATONVM_USER_LOADER_RESOURCE_BYTECODE=1 cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W119UserLoaderResourceBody
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// bare-url get=file:...L7W119UserLoaderResourceBody.class resources=1 stream=1
// sub-url get=file:...L7W119UserLoaderResourceBody.class resources=1 stream=1
// find-override get=file:...thing.txt resources=0 stream=-1 finds>0=true
// get-override get=null
// app-loader get=file:...L7W119UserLoaderResourceBody.class resources=1 stream=1
// missing-bare get=null resources=0 stream=-1
// missing-sub get=null resources=0 stream=-1
// missing-app get=null resources=0 stream=-1
// bare-get hits=2000
// sub-get hits=2000
// app-get hits=2000
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.util.Collections;
import java.util.Enumeration;

public class L7W119UserLoaderResourceBody {
    static final String SELF = "L7W119UserLoaderResourceBody.class";
    static final String NONE = "no/such/resource-L7W119.txt";

    static final class SubUrl extends URLClassLoader {
        SubUrl(URL[] urls) {
            super(urls, null);
        }
    }

    static final class FindOverride extends ClassLoader {
        int finds;

        FindOverride() {
            super(null);
        }

        @Override
        protected URL findResource(String name) {
            finds++;
            if (name.equals("virtual/thing.txt")) {
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
        GetOverride() {
            super(null);
        }

        @Override
        public URL getResource(String name) {
            return null;
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

    static String rows(String label, ClassLoader l, String name) throws IOException {
        String g = null;
        int rs = 0;
        int st = 0;
        for (int i = 0; i < 2000; i++) {
            g = tail(l.getResource(name));
            rs = count(l.getResources(name));
            st = readAll(l.getResourceAsStream(name));
        }
        return label + " get=" + g + " resources=" + rs + " stream=" + st;
    }

    public static void main(String[] args) throws Exception {
        URL dir = Path.of(L7W119UserLoaderResourceBody.class.getProtectionDomain()
                .getCodeSource().getLocation().toURI()).toUri().toURL();
        URLClassLoader bare = new URLClassLoader(new URL[] {dir}, null);
        SubUrl sub = new SubUrl(new URL[] {dir});
        FindOverride find = new FindOverride();
        GetOverride get = new GetOverride();
        ClassLoader app = L7W119UserLoaderResourceBody.class.getClassLoader();
        System.out.println(rows("bare-url", bare, SELF));
        System.out.println(rows("sub-url", sub, SELF));
        System.out.println(rows("find-override", find, "virtual/thing.txt") + " finds>0=" + (find.finds > 0));
        System.out.println("get-override get=" + tail(get.getResource(SELF)));
        System.out.println(rows("app-loader", app, SELF));
        System.out.println(rows("missing-bare", bare, NONE));
        System.out.println(rows("missing-sub", sub, NONE));
        System.out.println(rows("missing-app", app, NONE));
        bench(bare, sub, app);
        bare.close();
        sub.close();
    }

    static void bench(ClassLoader bare, ClassLoader sub, ClassLoader app) {
        String[] names = {"bare-get", "sub-get", "app-get"};
        ClassLoader[] loaders = {bare, sub, app};
        final int n = 2000;
        for (int r = 0; r < names.length; r++) {
            ClassLoader l = loaders[r];
            long best = Long.MAX_VALUE;
            int hits = 0;
            for (int rep = 0; rep < 3; rep++) {
                hits = 0;
                long t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    if (l.getResource(SELF) != null) {
                        hits++;
                    }
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(names[r] + " hits=" + hits);
            System.out.println("TIME " + names[r] + " " + (best / n) + " ns/call (min of 3)");
        }
    }
}
