// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6 (review of the protection-domain
// area): `Class.getProtectionDomain()` of the classes the JDK defines with
// its LOOKUP class's domain or with none at all. HotSpot:
//   * a lambda proxy and a `Lookup.defineHiddenClass` class share their
//     lookup class's domain (`Lookup.lookupClassProtectionDomain`);
//   * a `Proxy` class, an array class and a primitive class have the JDK's
//     shared all-permission domain (null `CodeSource`, null loader) -- the
//     same object `String.class` answers.
// Before wave 49 this VM (both modes, measured locally) answered
// `lambda=false`, `hidden=false`, `proxy=false <cs> AppClassLoader`
// (a `file:/.../$Proxy0.class` CodeSource), `array=false file:.
// AppClassLoader` and `prim=pd-null`. The `lambda` row runs first (cold) and
// again last. `CRATONVM_DBG=sbload` prints `[DBG_SBLOAD] hidden domain ...
// host=L6W49ProtectionDomainShapes -> Some(..)` for the lambda rows (the
// positive control).
//
// HotSpot 25.0.3 prints (measured; the same with -Xint):
//     lambda=true
//     nested=true
//     hidden=true
//     proxy=true cs=null loader=null
//     array=true cs=null loader=null
//     prim=true cs=null loader=null
//     user-domain=true
//     lambda-again=true
//
//     javac -d out L6W49ProtectionDomainShapes.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W49ProtectionDomainShapes
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Proxy;
import java.net.URL;
import java.security.CodeSource;
import java.security.ProtectionDomain;
import java.security.cert.Certificate;
import java.util.function.Supplier;

public class L6W49ProtectionDomainShapes {
    public static class Hello {
    }

    public interface Face {
        void go();
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(null);
        }

        Class<?> define(byte[] b, ProtectionDomain pd) {
            return defineClass(null, b, 0, b.length, pd);
        }
    }

    static byte[] own(String n) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(n + ".class")) {
            return in.readAllBytes();
        }
    }

    static String allPerm(ProtectionDomain pd) {
        ProtectionDomain boot = String.class.getProtectionDomain();
        return (pd == boot) + " cs=" + (pd == null ? "pd-null" : String.valueOf(pd.getCodeSource()))
                + " loader=" + (pd == null ? "pd-null" : String.valueOf(pd.getClassLoader()));
    }

    public static void main(String[] args) throws Throwable {
        ProtectionDomain app = L6W49ProtectionDomainShapes.class.getProtectionDomain();
        Supplier<String> s = () -> "x";
        System.out.println("lambda=" + (s.getClass().getProtectionDomain() == app));
        System.out.println("nested=" + (Hello.class.getProtectionDomain() == app));
        Class<?> hidden = MethodHandles.lookup()
                .defineHiddenClass(own("L6W49ProtectionDomainShapes$Hello"), true).lookupClass();
        System.out.println("hidden=" + (hidden.getProtectionDomain() == app));
        Object p = Proxy.newProxyInstance(L6W49ProtectionDomainShapes.class.getClassLoader(),
                new Class<?>[] {Face.class}, (o, m, a) -> null);
        System.out.println("proxy=" + allPerm(p.getClass().getProtectionDomain()));
        System.out.println("array=" + allPerm(Hello[].class.getProtectionDomain()));
        System.out.println("prim=" + allPerm(int.class.getProtectionDomain()));
        ProtectionDomain mine = new ProtectionDomain(
                new CodeSource(new URL("file:/l6w49/mine.jar"), (Certificate[]) null), null);
        Class<?> user = new Loader().define(own("L6W49ProtectionDomainShapes$Hello"), mine);
        System.out.println("user-domain=" + (user.getProtectionDomain() == mine));
        Supplier<String> t = () -> "y";
        System.out.println("lambda-again=" + (t.getClass().getProtectionDomain() == app));
    }
}
