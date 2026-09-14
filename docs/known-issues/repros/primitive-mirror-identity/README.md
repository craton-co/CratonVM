# Primitive class-mirror identity — two eliminations

Both probes **pass** on CratonVM and match HotSpot. They are checked in as
*eliminations*, not reproducers: they encode two plausible explanations for
the retired `spring-bean-attribute-type-null-flake` record
that have been tested and are not it, so nobody spends an afternoon on them
again.

That bug is Spring's `ClassUtils.resolvePrimitiveIfNecessary` returning null,
which is only reachable when `clazz.isPrimitive()` is true and an
`IdentityHashMap<Class,Class>` keyed on the `X.class` literals misses it.

## Build and run

```bash
javac -d . PrimitiveMirrorIdentityProbe.java CrossLoaderPrimitiveProbe.java
cratonvm -cp . PrimitiveMirrorIdentityProbe 20000   # PROBE PASS
cratonvm -cp . CrossLoaderPrimitiveProbe 300        # PROBE PASS
java     -cp . PrimitiveMirrorIdentityProbe 20000   # control
java     -cp . CrossLoaderPrimitiveProbe 300        # control
```

## What each one rules out

* **`PrimitiveMirrorIdentityProbe`** — a primitive mirror is a singleton within
  one loader. 20,000 rounds reach all nine primitives through `X.class`,
  `X.TYPE`, `Method.getReturnType()`, `Class.getComponentType()`, and an
  annotation's `annotationType().getDeclaredMethods()`, checking object identity
  *and* membership of a map keyed on the literals, with a `System.gc()` every
  1024 rounds.

* **`CrossLoaderPrimitiveProbe`** — primitive mirrors are not per-loader. 300
  parent-last child loaders each define their own copy of the same class; every
  primitive literal and every `getReturnType()` result is compared across the
  boundary, and Spring's exact map predicate is run against the child's methods
  using the parent's map.

Both were written for a process where two loaders define every name — the
recurring shape of the Spring AOT cluster — which is why the cross-loader arm
exists at all.
