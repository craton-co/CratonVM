# ES failure family - DiversifyingChildren IVFKnn Objects.hash instability

Status: FIXED

Root cause:
- The visible `expected:<...> but was:<...>` numbers were not Lucene doc ids. They came from Lucene `QueryUtils.checkEqual(q, q)` asserting that two consecutive `hashCode()` calls on the same query returned the same value.
- CratonVM's native `java.util.Objects.hash(Object...)` / `Arrays.hashCode(Object[])` paths hashed reference-array elements by identity for common JDK value objects instead of their Java `hashCode()` values.
- `AbstractIVFKnnVectorQuery.hashCode()` allocates a fresh `Float.valueOf(providedVisitRatio)` inside `Objects.hash(...)`; identity hashing that fresh boxed float made the query hash change between calls. The outer diversifying query hash amplified this into the observed `+124` assertion drift.

Fix:
- `native-builtins/src/lib.rs`
  - `native_objects_hashCode(Object)` and `native_objects_hash(Object[])` now use Java value hash semantics for `String` and boxed primitive wrappers before falling back to the existing virtual path.
- `native-builtins/src/phases_early.rs`
  - `Arrays.hashCode(Object[])` now applies the same value hashing for the modular registration path.
  - Added a regression unit for a `String`, boxed `Integer`, boxed `Float`, and `null` Object-array hash.

Validation:
- Binary: `/data/data/bin/cratonvm-es-suite-objects-hash-20260710-070500-r2`
- Hash matrix: `/tmp/objectshash-matrix-r2-1783668680`
  - CratonVM matches HotSpot for `Objects.hash("field")`, `Objects.hash(2)`, `Objects.hash(Float.valueOf(0f))`, `Objects.hash(2,2)`, and repeated `Arrays.hashCode(new Object[]{...})`.
- Targeted methods:
  - `/tmp/cratonvm-testEmptyIndex-objects-hash-r2-1783668774`: `OK (1 test)`
  - `/tmp/cratonvm-testFilterWithNoVectorMatches-objects-hash-r2-1783668796`: `OK (1 test)`
  - `/tmp/cratonvm-testSkewedIndex-objects-hash-r2-1783668817`: `OK (1 test)`
- Full class:
  - `/tmp/cratonvm-DiversifyingChildrenIVFKnnFloatVectorQueryTests-objects-hash-r2-1783668874`: `OK (6 tests)`
- Rust checks:
  - `cargo check -p cratonvm-native-builtins`
  - `cargo test -p cratonvm-native-builtins t2_arrays_object_hash_code_uses_value_hash_for_jdk_wrappers --lib -- --nocapture`

Original failing proof:
- Binary: `/data/data/bin/cratonvm-es-suite-bytebuffer-floatview-20260710-063500-r2`
- Run: `/tmp/cratonvm-DiversifyingChildrenIVFKnnFloatVectorQueryTests-floatview-r2-1783666797`
- Result: FAIL, rc=1, 6 tests run, 3 failures.
