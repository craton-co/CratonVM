package com.cratonvm.jdkonly.svc.internal;

import com.cratonvm.jdkonly.svc.Ctored;

/**
 * The class file that actually runs -- see the doc comment on the
 * {@code modules/} source of the same name for why there are two.
 *
 * It IS a subtype of {@code Ctored}, so the subtype rule passes and only the
 * constructor-visibility rule can fire. Its no-arg constructor is PRIVATE, and
 * {@code ServiceLoader.getConstructor} asks {@code clazz.getConstructor()},
 * which searches public members only:
 *
 * <pre>
 * java.util.ServiceConfigurationError: com.cratonvm.jdkonly.svc.Ctored:
 *   com.cratonvm.jdkonly.svc.internal.HiddenCtor
 *   Unable to get public no-arg constructor
 *       caused by java.lang.NoSuchMethodException:
 *           com.cratonvm.jdkonly.svc.internal.HiddenCtor.&lt;init&gt;()
 * </pre>
 *
 * The cause is the point of the three-argument {@code fail}: this is the only
 * {@code ServiceConfigurationError} in the fixture that has one, and a VM that
 * refuses with a bare message is refusing for a reason it never established.
 *
 * A private constructor is enough on its own -- package-private would refuse
 * for the same reason -- and it is the shape a real "singleton-ish" provider is
 * written in by accident, which is why {@code getDeclaredConstructor()} plus a
 * reflective override accepted it here for so long.
 *
 * Compiled by a second javac pass over {@code build-modules/} on a plain
 * classpath, so javac never sees the {@code provides} clause that would reject
 * it. {@code Ctored} is resolvable there because the pass runs with
 * {@code -classpath build-modules/cratonvm.jdkonly.svc}.
 */
public final class HiddenCtor implements Ctored {
    private HiddenCtor() {
    }

    @Override
    public String id() {
        return "never-reached";
    }
}
