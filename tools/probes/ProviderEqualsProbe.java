// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.security.Provider;
import java.util.Map;
import java.util.Set;
import javax.net.ssl.SSLContext;

/**
 * `java.security.Provider` is a `Properties`, and real `Properties.equals` /
 * `hashCode` read the inherited `map`. Under `--jdk-only` a fabricated
 * `Provider` (one whose constructor never ran) has a null `map`, and a
 * constructed one whose `putId` only wrote the VM's side store has an EMPTY
 * one -- so `provider.equals(provider)` was an NPE, then `false`.
 *
 * netty's `JdkSslContext.<init>` asks `DEFAULT_PROVIDER.equals(
 * sslContext.getProvider())`; 95 tests in 20 classes died on it.
 *
 * Run under HotSpot and under `--jdk-only`; every line must match.
 */
public class ProviderEqualsProbe {
    static class MyProv extends Provider {
        MyProv(String n) {
            super(n, "1.0", "info of " + n);
        }
    }

    public static void main(String[] a) throws Exception {
        Provider p = SSLContext.getInstance("TLS").getProvider();
        Provider p2 = SSLContext.getInstance("TLS").getProvider();
        System.out.println("ctx provider name = " + p.getName());
        System.out.println("ctx p.equals(p)   = " + p.equals(p));
        System.out.println("ctx p.equals(p2)  = " + p.equals(p2));
        System.out.println("ctx hash equal    = " + (p.hashCode() == p2.hashCode()));

        Provider x = new MyProv("X");
        Provider y = new MyProv("X");
        Provider z = new MyProv("Z");
        System.out.println("app x.equals(x)   = " + x.equals(x));
        System.out.println("app x.equals(y)   = " + x.equals(y));
        System.out.println("app x.equals(z)   = " + x.equals(z));
        System.out.println("app hash x==y     = " + (x.hashCode() == y.hashCode()));
        Set<Map.Entry<Object, Object>> es = x.entrySet();
        System.out.println("app entries       = " + es.size());
    }
}
