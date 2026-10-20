// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L4: `java.lang.Class` answers CratonVM's
// natives give (the registered `Bridge`s answer at the invoke doors' step 1
// in both modes; `native-builtins/src/lang_class.rs`).
//
//   mods X[]       an array's modifiers carry its ELEMENT's `getModifiers()`
//                  access bits -- the InnerClasses flags for a nested element
//                  (`native_class_get_modifiers` read the element class file's
//                  own `access_flags`: `PrivNested[]` was 0x410, `ProtNested[]`
//                  0x411). `computeDefaultSUID` hashes these.
//   isInstance     `native_class_is_instance`'s array fall-through compared
//                  the target's COMPONENT id with the mirror's ARRAY id, so an
//                  array one dimension deeper matched: `String[].class
//                  .isInstance(new String[1][])` was true, and `Class.cast`
//                  (JDK bytecode, which asks `isInstance`) did not throw.
//   accessFlags    `getClassAccessFlagsRaw0` was `getModifiers`, which masks
//                  `SUPER`: `String.class.accessFlags()` lacked SUPER.
//   hidden         a hidden class's `/0x` tail is part of its name:
//                  `getSimpleName` answered `0x...`, `getCanonicalName`
//                  `L4W29CnHid.0x...` (HotSpot: null), `getPackageName`
//                  `L4W29CnHid` (HotSpot: "").
//
// Under `--jdk-only` an array `isInstance` is also decided by the
// `instanceof` bytecode's identity verdict now (`array_cast_resolved_verdict`
// through the new `NativeContext::array_instance_of_array_class`), so an array
// of another loader's same-named class is refused as `instanceof` refuses it.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W29ClassReflectionShapes
// (the same lines in every mode; the fixed rows were genuine bugs in both)
//
// HotSpot 25 (25.0.3, default and -Xint) prints exactly:
//   mods PrivNested[]: private abstract final [412]
//   mods ProtNested[]: protected abstract final [414]
//   mods ProtNested[][]: protected abstract final [414]
//   mods PubNested[]: public abstract final [411]
//   mods PrivIface[]: private abstract final [412]
//   mods En[]: abstract final [410]
//   isInstance String[] <- String[1][]: false
//   isInstance int[] <- int[1][]: false
//   isInstance Object[] <- Object[1][]: true
//   isInstance Object[] <- String[1][]: true
//   isInstance String[][] <- String[1][][]: false
//   cast String[] <- String[1][]: java.lang.ClassCastException: Cannot cast [[Ljava.lang.String; to [Ljava.lang.String;
//   accessFlags String: [PUBLIC, FINAL, SUPER]
//   accessFlags L4W29ClassReflectionShapes: [PUBLIC, SUPER]
//   accessFlags Runnable: [PUBLIC, INTERFACE, ABSTRACT]
//   accessFlags int: [PUBLIC, FINAL, ABSTRACT]
//   accessFlags PrivNested: [PRIVATE, STATIC]
//   accessFlags PrivNested[]: [PRIVATE, FINAL, ABSTRACT]
//   accessFlags En: [STATIC, ENUM]
//   hidden isHidden: true
//   hidden name has /0x: true
//   hidden simpleName has /0x: true
//   hidden canonical: null
//   hidden typeName has /0x: true
//   hidden mods:  [0]
//   hidden nestHost self: true
//   hidden package: 
//   lambda nestHost: class L4W29ClassReflectionShapes
//   lambda ifaces: [interface java.lang.Runnable]
//   lambda super: class java.lang.Object
//   lambda enclosingMethod: null
//   lambda package: 
//   lambda sealed: false
//   lambda accessFlags: [FINAL, SUPER, SYNTHETIC]
//   isAssignable Object[][] <- int[][]: false
//   isAssignable int[] <- int[][]: false
//   recordComponents En: null
//   nestMembers PrivNested len>1: true

import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Modifier;
import java.util.Arrays;
import java.util.function.Supplier;

class L4W29CnHid { public String toString() { return "hid"; } }

public class L4W29ClassReflectionShapes {
    private static class PrivNested {}
    protected static class ProtNested {}
    public static class PubNested {}
    private interface PrivIface {}
    enum En { A, B { } }
    record Rec(int x) {}

    static void row(String n, Supplier<Object> s) {
        String out;
        try {
            Object v = s.get();
            out = v instanceof Object[] arr ? Arrays.toString(arr) : String.valueOf(v);
        } catch (Throwable t) {
            out = t.toString();
        }
        System.out.println(n + ": " + out);
    }
    static String hx(int m) { return Modifier.toString(m) + " [" + Integer.toHexString(m) + "]"; }

    public static void main(String[] args) throws Exception {
        row("mods PrivNested[]", () -> hx(PrivNested[].class.getModifiers()));
        row("mods ProtNested[]", () -> hx(ProtNested[].class.getModifiers()));
        row("mods ProtNested[][]", () -> hx(ProtNested[][].class.getModifiers()));
        row("mods PubNested[]", () -> hx(PubNested[].class.getModifiers()));
        row("mods PrivIface[]", () -> hx(PrivIface[].class.getModifiers()));
        row("mods En[]", () -> hx(En[].class.getModifiers()));
        row("isInstance String[] <- String[1][]", () -> String[].class.isInstance(new String[1][]));
        row("isInstance int[] <- int[1][]", () -> int[].class.isInstance(new int[1][]));
        row("isInstance Object[] <- Object[1][]", () -> Object[].class.isInstance(new Object[1][]));
        row("isInstance Object[] <- String[1][]", () -> Object[].class.isInstance(new String[1][]));
        row("isInstance String[][] <- String[1][][]", () -> String[][].class.isInstance(new String[1][][]));
        row("cast String[] <- String[1][]", () -> String[].class.cast(new String[1][]).getClass());
        row("accessFlags String", () -> String.class.accessFlags());
        row("accessFlags L4W29ClassReflectionShapes", () -> L4W29ClassReflectionShapes.class.accessFlags());
        row("accessFlags Runnable", () -> Runnable.class.accessFlags());
        row("accessFlags int", () -> int.class.accessFlags());
        row("accessFlags PrivNested", () -> PrivNested.class.accessFlags());
        row("accessFlags PrivNested[]", () -> PrivNested[].class.accessFlags());
        row("accessFlags En", () -> En.class.accessFlags());
        MethodHandles.Lookup lk = MethodHandles.lookup();
        byte[] bytes;
        try (InputStream in = L4W29ClassReflectionShapes.class.getResourceAsStream("L4W29CnHid.class")) { bytes = in.readAllBytes(); }
        Class<?> h = lk.defineHiddenClass(bytes, true).lookupClass();
        row("hidden isHidden", () -> h.isHidden());
        row("hidden name has /0x", () -> h.getName().startsWith("L4W29CnHid/0x"));
        row("hidden simpleName has /0x", () -> h.getSimpleName().startsWith("L4W29CnHid/0x"));
        row("hidden canonical", () -> h.getCanonicalName());
        row("hidden typeName has /0x", () -> h.getTypeName().startsWith("L4W29CnHid/0x"));
        row("hidden mods", () -> hx(h.getModifiers()));
        row("hidden nestHost self", () -> h.getNestHost() == h);
        row("hidden package", () -> h.getPackageName());
        Runnable lam = () -> {};
        Class<?> lc = lam.getClass();
        row("lambda nestHost", () -> lc.getNestHost());
        row("lambda ifaces", () -> lc.getInterfaces());
        row("lambda super", () -> lc.getSuperclass());
        row("lambda enclosingMethod", () -> lc.getEnclosingMethod());
        row("lambda package", () -> lc.getPackageName());
        row("lambda sealed", () -> lc.isSealed());
        row("lambda accessFlags", () -> lc.accessFlags());
        row("isAssignable Object[][] <- int[][]", () -> Object[][].class.isAssignableFrom(int[][].class));
        row("isAssignable int[] <- int[][]", () -> int[].class.isAssignableFrom(int[][].class));
        row("recordComponents En", () -> En.class.getRecordComponents());
        row("nestMembers PrivNested len>1", () -> PrivNested.class.getNestMembers().length > 1);
    }
}
