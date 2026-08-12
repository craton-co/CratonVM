package com.cratonvm.jdkonly.svc;

/**
 * A third service type, whose single module-declared provider's static
 * {@code provider()} factory returns {@code null}.
 *
 * This is the OTHER kind of illegal factory, and it fails at a different
 * moment: {@code ProviderImpl.invokeFactoryMethod} raises
 * {@code ServiceConfigurationError} when the factory answers {@code null}, so
 * the refusal belongs to {@code Provider.get()} -- NOT to
 * {@code Provider.type()}, which answers normally. Measured on HotSpot 25:
 * {@code stream().map(Provider::type)} returns {@code [.. .Nulled]} without
 * throwing, while {@code map(Provider::get)} and every {@code iterator()}
 * traversal throw. A VM that refuses too early here is as wrong as one that
 * never refuses.
 */
public interface Nulled {
    String id();
}
