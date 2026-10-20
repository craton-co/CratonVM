// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 15, lane L5: a REF_invokeStatic lambda
// implementation whose host class was defined by a USER class loader
// initializes the class that declares the implementation before it runs
// (JVMS 5.5), exactly as it does under the application loader.
//
// `main` defines its own copies of `$Host`, `$Other`, `$Parent` and `$Child`
// through a fresh URLClassLoader over `java.class.path` (parent: the platform
// loader), so CratonVM dispatches the lambdas on the loader-faithful owner
// (`lambda_impl_dispatch_override*`). Before wave 15 that arm initialized
// nothing, so `Other.m` could print before `Other.<clinit>` (which then ran
// only when `m`'s `getstatic` reached it). No extra setup; run with the default settings and
// with --nojit. HotSpot 25 prints exactly:
//
//   user loader: true
//   Other.<clinit>
//   Other.m
//   7
//   Parent.<clinit>
//   Parent.m
//   8
//   done
import java.io.File;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.List;
import java.util.function.IntSupplier;

public class LoaderLambdaStaticInit {
    public static class Other {
        static int V;

        static {
            System.out.println("Other.<clinit>");
            V = 7;
        }

        public static int m() {
            System.out.println("Other.m");
            return V;
        }
    }

    public static class Parent {
        static int W;

        static {
            System.out.println("Parent.<clinit>");
            W = 8;
        }

        public static int m() {
            System.out.println("Parent.m");
            return W;
        }
    }

    public static class Child extends Parent {
        static {
            System.out.println("Child.<clinit>");
        }
    }

    public static class Host {
        public static void run() {
            System.out.println("user loader: " + (Host.class.getClassLoader() instanceof URLClassLoader));
            IntSupplier s = Other::m;
            System.out.println(s.getAsInt());
            // An inherited static named through its subclass: only the
            // declaring class is initialized.
            IntSupplier t = Child::m;
            System.out.println(t.getAsInt());
        }
    }

    public static void main(String[] args) throws Throwable {
        List<URL> urls = new ArrayList<>();
        for (String entry : System.getProperty("java.class.path").split(File.pathSeparator)) {
            if (!entry.isEmpty()) {
                urls.add(new File(entry).toURI().toURL());
            }
        }
        URLClassLoader loader =
                new URLClassLoader(urls.toArray(new URL[0]), ClassLoader.getPlatformClassLoader());
        Class<?> host = Class.forName("LoaderLambdaStaticInit$Host", true, loader);
        host.getMethod("run").invoke(null);
        System.out.println("done");
    }
}
