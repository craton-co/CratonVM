package com.cratonvm.jdkonly.svc.internal;

/**
 * The class file that actually runs -- see the doc comment on the
 * {@code modules/} source of the same name for why there are two.
 *
 * It implements NOTHING, so it is not a subtype of
 * {@code com.cratonvm.jdkonly.svc.Unsub}, and it declares no static
 * {@code provider()}, so {@code findStaticProviderMethod} answers null and
 * {@code loadProvider} falls through to its constructor form:
 *
 * <pre>
 * // no factory method so must be a subtype
 * if (!service.isAssignableFrom(clazz))
 *     fail(service, clazz + " not a subtype");
 * </pre>
 *
 * Note {@code clazz}, not {@code clazz.getName()}: the module path renders the
 * Class through {@code toString()} ("class com.cratonvm...NotSubProvider")
 * while the CLASSPATH iterator uses the bare binary name for the same rule.
 * RJdkModule asserts the opening, the provider name and the tail rather than
 * the whole string, so it pins the shape without pinning that asymmetry.
 *
 * The constructor is deliberately PUBLIC: this fixture must fail the subtype
 * rule and nothing else. {@code HiddenCtor} is the one that fails the
 * constructor-visibility rule.
 *
 * Compiled by a second javac pass over {@code build-modules/}, with no module
 * context, so javac never sees the {@code provides} clause that would reject it.
 */
public final class NotSubProvider {
    public NotSubProvider() {
    }
}
