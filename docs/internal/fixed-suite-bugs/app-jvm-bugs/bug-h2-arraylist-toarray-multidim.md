# H2 — `ArrayList.toArray(T[])` multi-dimensional array `ClassCastException`

## Status
**FIXED** (dev `04e7e3c`, predates this session — confirmed 2026-06-05) — `real_jdk_to_array_typed` (the active handler for real `java.util.ArrayList`/`AbstractCollection`, registered in `vm/src/vm/vm_init.rs`) now reproduces the template array's runtime component type via `class_id_of_object(template)` → `new_ref_array`, preserving multi-dimensional element types (`Value[][]`) instead of allocating a bare `Object[]`. Verified: `SortProbe --nojit` (multi-column ORDER BY → `SortOrder.sort` → `rows.toArray(new Value[0][])`) matches HotSpot exactly (`OK rows=50 checksum=139940`); **0** `cannot be cast` in `TestScript --nojit`.

## Severity
**MEDIUM** — correctness in collection/array copy paths used by H2 sort logic.

## App / suite
- **App:** H2 Database (`apps/h2database`)
- **Trigger:** `org.h2.result.SortOrder.sort` with `Value[][]` via `ArrayList.toArray(T[])`
- **Reference:** `continue_prompt_h2_testall.md`

## Symptom

```
ClassCastException
  at ArrayList.toArray(ArrayList.java:…)
  at org.h2.result.SortOrder.sort(…)
```

When `T[]` is a multi-dimensional array type (e.g. `Value[][]`).

## HotSpot behavior

`ArrayList.toArray(T[])` allocates or fills an array of the correct runtime component type, including multi-dimensional arrays.

## CratonVM behavior

`toArray` produces an array object whose component type does not match the requested `T[]`, causing `ClassCastException` when H2 uses the result.

## Root cause (suspected)

`ArrayList.toArray(T[])` native or intrinsic does not handle **array types with rank > 1** when copying elements — likely treats component type as `Object` or one dimension only.

**Suspect areas:** `native_collections` / `ArrayList.toArray`, array allocation (`newarray` / reflection array create).

## Impact

Sort/order-by paths in H2 fail once tests reach those code paths. Blocked from measurement in latest run by earlier SHA1PRNG fatal.

## Reproduce

After SHA1PRNG fix, run `TestAll` until SortOrder tests run, or minimal:

```java
import java.util.*;
public class ToArray2DProbe {
    public static void main(String[] a) {
        ArrayList<Object[]> list = new ArrayList<>();
        list.add(new Object[1]);
        Object[][] arr = list.toArray(new Object[0][]);
        System.out.println(arr.getClass().getName());
    }
}
```

Compare output/runtime class under CratonVM vs HotSpot.

## What to fix

1. Match JDK `ArrayList.toArray(T[])` spec for generic array types including `[][]`.
2. Add regression test in CratonVM’s own Java probes.
3. Re-run H2 `TestAll` / SortOrder-related tests.

## Related

- [bug-h2-securerandom-sha1prng.md](bug-h2-securerandom-sha1prng.md)
