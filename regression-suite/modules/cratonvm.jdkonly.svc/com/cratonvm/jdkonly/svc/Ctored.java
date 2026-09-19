package com.cratonvm.jdkonly.svc;

/**
 * A fifth service type, whose single module-declared provider --
 * {@code internal.HiddenCtor} -- IS a subtype but whose no-arg constructor is
 * NOT public.
 *
 * {@code ServiceLoader.getConstructor} asks {@code clazz.getConstructor()},
 * which searches public members only, and turns the resulting
 * {@code NoSuchMethodException} into
 *
 * <pre>
 * fail(service, cn + " Unable to get public no-arg constructor", x);
 * </pre>
 *
 * -- the THREE-argument {@code fail}, so this is the one
 * {@code ServiceConfigurationError} in this fixture that carries a cause.
 *
 * CratonVM's native loader asked {@code getDeclaredConstructor()} instead and
 * then opened the result reflectively, so a provider the JDK refuses outright
 * was constructed and handed out. Both illegal shapes -- no no-arg constructor
 * at all, and a non-public one -- are indistinguishable to the JDK because
 * {@code getConstructor()} cannot see either.
 */
public interface Ctored {
    String id();
}
