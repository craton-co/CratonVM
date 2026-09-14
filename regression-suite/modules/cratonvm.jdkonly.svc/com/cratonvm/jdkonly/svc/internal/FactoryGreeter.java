package com.cratonvm.jdkonly.svc.internal;

import com.cratonvm.jdkonly.svc.Greeter;

/**
 * A module-path service provider discovered through the static
 * {@code provider()} FACTORY. This form is honoured only for providers in a
 * named module -- the class deliberately does NOT implement the service type,
 * which is exactly what makes it invalid on the class path.
 */
public final class FactoryGreeter {
    private FactoryGreeter() {
    }

    public static Greeter provider() {
        return () -> "module-factory";
    }
}
