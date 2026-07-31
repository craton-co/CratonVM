package com.cratonvm.jdkonly.svc.internal;

import com.cratonvm.jdkonly.svc.Greeter;

/**
 * A module-path service provider discovered through its public no-arg
 * constructor. Its package is fully encapsulated, so it can only be reached via
 * {@code ServiceLoader} -- never by {@code Class.forName} + {@code newInstance}.
 */
public final class EnGreeter implements Greeter {
    public EnGreeter() {
    }

    @Override
    public String greet() {
        return "module-hello";
    }
}
