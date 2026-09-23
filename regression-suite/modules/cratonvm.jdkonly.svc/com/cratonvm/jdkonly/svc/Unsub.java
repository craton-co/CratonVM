package com.cratonvm.jdkonly.svc;

/**
 * A fourth service type, whose single module-declared provider --
 * {@code internal.NotSubProvider} -- has an ordinary public no-arg constructor
 * and is NOT a subtype of this interface, and declares no {@code provider()}
 * factory either.
 *
 * That is the CONSTRUCTOR-form half of {@code ServiceLoader.loadProvider}'s
 * subtype rule:
 *
 * <pre>
 * // no factory method so must be a subtype
 * if (!service.isAssignableFrom(clazz))
 *     fail(service, clazz + " not a subtype");
 * </pre>
 *
 * The FACTORY half is {@link Rejected}, and it was armed a day earlier. This
 * one was absent on both provider paths, which is exactly why it needs its own
 * service type rather than another provider on an existing one: a loader for a
 * legal service must stay a pure positive case.
 */
public interface Unsub {
    String id();
}
