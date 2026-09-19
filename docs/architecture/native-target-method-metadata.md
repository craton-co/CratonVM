# Native targets in resolved method metadata

Native dispatch no longer relies on the unused VM-global
`native_method_cache`. Each `ResolvedMethod` entry now carries the exact
native callback and `NativeKind` discovered for its symbolic owner.

The lifecycle is:

1. Constant-pool method resolution computes the registry hash once.
2. The callback and category are stored beside the resolved names and
   parameter count.
3. Invoke-cache population copies the callback directly into its native
   target when the symbolic owner remains authoritative.
4. Loader-local or `invokespecial` owner redirects deliberately ignore the
   symbolic-owner target and perform one lookup for the redirected owner.
5. Existing resolution-cache invalidation on class redefinition and loader
   unloading discards the callback with the rest of the resolved metadata.

Caching `NativeKind` with the callback also removes the former second registry
hash used solely to decide whether a synthetic stub must yield to loaded real
bytecode.

The resolution cache is bounded, so native target metadata is reclaimed by
normal FIFO eviction and cannot reproduce the unbounded lifetime of the
removed global string-keyed cache.
