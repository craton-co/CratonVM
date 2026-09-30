package com.cratonvm.jdkonly.svc.internal;

import com.cratonvm.jdkonly.svc.Nulled;

/**
 * A module-path factory provider that answers {@code null}.
 *
 * javac accepts this -- the return type IS a subtype, so the only rule it can
 * check is satisfied -- which is why, unlike {@code WrongFactory}, this fixture
 * needs no overlay pass. {@code ServiceLoader} must still refuse it, and must
 * refuse it at {@code Provider.get()} rather than while building the wrapper.
 */
public final class NullProvider {
    private NullProvider() {
    }

    public static Nulled provider() {
        return null;
    }
}
