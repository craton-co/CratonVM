package com.cratonvm.jdkonly.svc.internal;

/**
 * The class file that actually runs -- see the doc comment on the
 * `modules/` source of the same name for why there are two.
 *
 * `provider()` returns `java.lang.Object`, which is NOT a subtype of
 * `com.cratonvm.jdkonly.svc.Rejected`. JDK 25
 * `java.util.ServiceLoader.loadProvider` must answer
 *
 *   ServiceConfigurationError: com.cratonvm.jdkonly.svc.Rejected:
 *       public static java.lang.Object
 *       com.cratonvm.jdkonly.svc.internal.WrongFactory.provider()
 *       return type not a subtype
 *
 * with a null cause, on EVERY traversal of the loader -- `iterator()` and
 * `stream()` alike. Measured on HotSpot 25.0.3.9, not assumed.
 *
 * Compiled by a second javac pass over `build-modules/`, with no module
 * context, so javac never sees the `provides` clause that would reject it.
 */
public final class WrongFactory {
    private WrongFactory() {
    }

    public static Object provider() {
        return "not-a-Rejected";
    }
}
