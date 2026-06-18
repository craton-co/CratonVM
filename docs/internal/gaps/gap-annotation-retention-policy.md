# Gap: `@Retention(CLASS)` annotations visible at runtime

**Discovered:** 2026-06-10 (craton-gpu Demo, Section 12 — annotation surface)  
**Affects:** Any code that checks `isAnnotationPresent` or `getAnnotation` for CLASS-retained annotations  
**Severity:** Medium — behavioral divergence from HotSpot; affects libraries that use CLASS-retained annotations (JPA, Hibernate, craton-gpu `@GpuKernel`/`@GpuExclude`/`@EnableGpuAsync`, Lombok, etc.)  
**Status:** ✅ **FIXED 2026-06-11** — reflection now registers only `RuntimeVisibleAnnotations`. Filtered at every reflection source: `class.annotations` (3 spots in `classloading/src/class_manager.rs`: define / redefine / in-place-update) and method/field annotations (`extract_annotations_from_attributes` in `vm/src/vm/vm_exec.rs`). Verified CratonVM == HotSpot: a CLASS-retained annotation on class/method/field is invisible to `isAnnotationPresent`/`getAnnotation` and absent from `getDeclaredAnnotations()`, while a RUNTIME-retained one stays visible.

---

## Symptom

```java
@Retention(RetentionPolicy.CLASS)   // compile-time only, NOT visible at runtime
@interface GpuKernel { ... }

@GpuKernel(grid = GridShape.ELEMENTWISE, blockX = 128)
public static void elemwise(int[] a, int[] b, int[] out) { ... }

// HotSpot:
method.getAnnotation(GpuKernel.class)  == null   // ✓ correct
klass.isAnnotationPresent(EnableGpuAsync.class)  == false  // ✓ correct

// CratonVM:
method.getAnnotation(GpuKernel.class)  != null   // ✗ WRONG: annotation visible
klass.isAnnotationPresent(EnableGpuAsync.class)  == true   // ✗ WRONG
```

Observed in craton-gpu Demo v0.2.0, Section 12 "Annotation surface":

| Check | Expected (HotSpot) | CratonVM CPU | CratonVM GPU |
|---|---|---|---|
| `Kernels.class.isAnnotationPresent(EnableGpuAsync.class)` | `false` | **`true`** | **`true`** |
| `elemwise.getAnnotation(GpuKernel.class) == null` | `true` | **`false`** | **`false`** |
| `hostOnly.getAnnotation(GpuExclude.class) == null` | `true` | **`false`** | **`false`** |

---

## Root cause

Java class files have two annotation attribute tables:
- `RuntimeVisibleAnnotations` — annotations with `@Retention(RUNTIME)`; must be available via reflection
- `RuntimeInvisibleAnnotations` — annotations with `@Retention(CLASS)` (default) or `@Retention(SOURCE)`; must **not** be exposed via reflection

CratonVM's class file parser does not distinguish these two attributes. It registers annotations from **both** tables into the runtime metadata, making CLASS-retained annotations visible via `java.lang.reflect.Method.getAnnotation()` and `Class.isAnnotationPresent()`.

SOURCE-retained annotations are stripped by `javac` and never appear in the `.class` file at all — those are unaffected.

---

## Reproduction

```bash
# Compile and run craton-gpu Demo
JDK="C:/Program Files/Java/jdk-25"
JAR="C:/craton/gpu-java/.claude/worktrees/sweet-wozniak-381a1f/target/craton-gpu-0.2.0.jar"
CLASSES="C:/craton/tmp/demo-classes"
"$JDK/bin/javac.exe" -cp "$JAR" -d "$CLASSES" "C:/craton/gpu-java/examples/Demo.java"

CV="C:/craton/CratonVM/target/release/cratonvm.exe"
"$CV" --java-home "$JDK" -cp "$CLASSES;$JAR" Demo 2>&1 | grep -A2 "Section 12\|isAnnotation\|annotation == null"
# Expected: present at runtime? false
# Actual:   present at runtime? true
```

---

## Fix direction

In the class file parsing / annotation loading code:
1. Find where `RuntimeVisibleAnnotations` and `RuntimeInvisibleAnnotations` attributes are parsed (likely in `classloading/` crate)
2. Only register annotations from `RuntimeVisibleAnnotations` into the runtime metadata table
3. Skip or discard `RuntimeInvisibleAnnotations` entirely — they are the CLASS-retained annotations

This is a 1-line fix in the attribute dispatcher: add a check `if attribute_name == "RuntimeInvisibleAnnotations { continue; }`.

---

## Impact

- **craton-gpu `@GpuKernel`, `@GpuExclude`, `@EnableGpuAsync`** — all use CLASS retention (AOT compiler reads them from bytecode; runtime should not see them). Incorrect visibility doesn't break the craton-gpu Demo (SimBridge doesn't use these annotations) but could affect production GPU dispatch.
- **JPA/Hibernate** — many JPA annotations (`@Column`, `@JoinColumn`, etc.) are RUNTIME-retained, so they're unaffected. But some ORM bytecode-weaving frameworks use CLASS-retained annotations.
- **Lombok** — uses SOURCE retention primarily; unaffected.
- Any library that uses annotation presence as an **absence check** (guards against a CLASS-retained annotation being present at runtime) would see false positives.
