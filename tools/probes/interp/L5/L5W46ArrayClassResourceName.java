// Interpreter round i1, wave 46, lane L5 (review of the resource doors) --
// `Class.getResource` / `getResourceAsStream` of an ARRAY or PRIMITIVE class
// resolve a relative name against the ELEMENT's package (`Class.resolveName`
// -> `getPackageName()`, which is `elementType()`'s package, and `java.lang`
// for a primitive).
//
// Rows (each `true` when the door found the resource):
//   string-array   String[].class.getResource("String.class")
//   object-2d      Object[][].class.getResource("Object.class")
//   int-array      int[].class.getResource("Integer.class")
//   int            int.class.getResource("Integer.class")
//   own-array      Self[].class.getResourceAsStream("L5W46ArrayClassResourceName.class")
//   own            L5W46ArrayClassResourceName.class.getResourceAsStream(same)
//
// Run: javac -d out L5W46ArrayClassResourceName.java;
//      cratonvm --java-home <jdk25> [--nojit] -cp out L5W46ArrayClassResourceName
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical):
//   string-array=true
//   object-2d=true
//   int-array=true
//   int=true
//   own-array=true
//   own=true
//
// CratonVM base `55834015b` (from the code, not run): the natives
// (`lang_class.rs` `t19_h10_resolve_resource_name`) took the package of the
// class's own name, `[Ljava/lang/String;` -> `[Ljava/lang`, and a primitive's
// empty one, so the first four rows print `false`; `own-array` and `own`
// print `true` (this probe's class is in the unnamed package, where the
// wrong prefix is no prefix). Both modes (a genuine bug; `--compatible`
// changes with it).

import java.io.InputStream;

public class L5W46ArrayClassResourceName {
    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName());
        }
    }

    static boolean found(InputStream in) throws Exception {
        if (in == null) return false;
        in.close();
        return true;
    }

    public static void main(String[] args) {
        row("string-array", () -> String[].class.getResource("String.class") != null);
        row("object-2d", () -> Object[][].class.getResource("Object.class") != null);
        row("int-array", () -> int[].class.getResource("Integer.class") != null);
        row("int", () -> int.class.getResource("Integer.class") != null);
        row("own-array", () -> found(L5W46ArrayClassResourceName[].class
                .getResourceAsStream("L5W46ArrayClassResourceName.class")));
        row("own", () -> found(L5W46ArrayClassResourceName.class
                .getResourceAsStream("L5W46ArrayClassResourceName.class")));
    }
}
