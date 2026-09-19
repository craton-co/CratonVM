package com.cratonvm.jdkonly.svc;

/**
 * A second service type, declared by this module with exactly ONE provider --
 * {@code internal.WrongFactory} -- whose static {@code provider()} factory
 * returns a type that is NOT a subtype of this interface.
 *
 * {@code ServiceLoader.loadProvider} must therefore raise
 * {@code ServiceConfigurationError} for this service on EVERY traversal, and
 * the point of a separate service type is that the {@link Greeter} checks stay
 * a pure positive case: a loader for a legal service must not be perturbed by
 * an illegal one declared beside it.
 */
public interface Rejected {
    String id();
}
