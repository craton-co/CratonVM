// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Standalone reproduction of the Infinispan `ConfigurationBuilder` retransform
// rejection filed as
// docs/known-issues/springboot/infinispan-configurationbuilder-retransform-verify-20260805.md
//
// `CacheAutoConfigurationTests$InfinispanCustomConfiguration.configurationBuilder()`
// does exactly one interesting thing: `mock(ConfigurationBuilder.class)`. Mockito's
// inline mock maker retransforms the class through ByteBuddy and hands the woven
// bytes back to the VM, whose verifier rejected them with
// "stack overflow during verification" at offset 27 of `simpleCache`.
//
// Run with `-Dnet.bytebuddy.dump=<dir>` to capture what ByteBuddy actually
// produced, and with `CRATONVM_DBG_REDEFINE_DUMP=<dir>` to capture what the VM
// rejected. Comparing the two separates "our verifier is wrong" from "the bytes
// were damaged in flight".

import java.lang.reflect.Method;

public class InfinispanMockRetransformProbe {
    public static void main(String[] args) throws Exception {
        String target = args.length > 0
                ? args[0]
                : "org.infinispan.configuration.cache.ConfigurationBuilder";

        Class<?> cls = Class.forName(target);
        System.out.println("loaded: " + cls.getName());

        Class<?> mockito = Class.forName("org.mockito.Mockito");
        Method mock = mockito.getMethod("mock", Class.class);

        Object m = mock.invoke(null, cls);
        System.out.println("mock created: " + (m == null ? "null" : m.getClass().getName()));

        // Exercise the woven method that the verifier rejected. `simpleCache()`
        // returns boolean; a mock must answer the default `false` rather than
        // running the real body.
        Method simpleCache = cls.getMethod("simpleCache");
        Object v = simpleCache.invoke(m);
        System.out.println("simpleCache() -> " + v);

        Method simpleCacheSet = cls.getMethod("simpleCache", boolean.class);
        Object r = simpleCacheSet.invoke(m, true);
        System.out.println("simpleCache(true) -> " + (r == null ? "null" : r.getClass().getName()));

        System.out.println("PROBE-OK");
    }
}
