// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L5
// (`i42-L5-proposal-one-classfileparser-shaped-error-for-every-define-door`):
// one malformed class file per header shape, through three define doors, so
// the doors' `ClassFormatError` texts can be compared with HotSpot's.
//
// Doors:
//   define1  `ClassLoader.defineClass("Bad", b, 0, len)` on a user loader
//            (HotSpot: `defineClass1` -> `ClassFileParser`)
//   lookup   `MethodHandles.lookup().defineClass(b)`
//            (JDK 25: `Lookup.validateAndFindInternalName` in Java first)
//   hidden   `MethodHandles.lookup().defineHiddenClass(b, true)` (the same)
// Shapes:
//   empty     0 bytes
//   short2    0xCA 0xFE
//   badmagic  0xDEADBEEF, then minor 0, major 69, then zeros (16 bytes)
//   version99 0xCAFEBABE, minor 0, major 99, then zeros (16 bytes)
//
// Output: "<door>-<shape>=<exception class> msg=<message>".
//
// Run: javac -d . L5W94DefineDoorErrors.java
//      java -cp . L5W94DefineDoorErrors
//      cratonvm --jdk-only   -cp . L5W94DefineDoorErrors
//      cratonvm --compatible -cp . L5W94DefineDoorErrors
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources, not run. `define1`:
// HotSpot's `ClassFileParser` (`guarantee_more(8)` -> "Truncated class file";
// the magic as an unsigned decimal and the name in internal form; the
// version text of `verify_class_version` with the external name; wave 42's
// `L5W42DefineClassErrors` measured the first two shapes on 25.0.3).
// `lookup` / `hidden`: `Lookup.validateAndFindInternalName` (`readInt` ->
// "Invalid ClassFile structure" under four bytes; a signed `int` magic;
// `UnsupportedClassVersionError("Unsupported class file version 99.0")`).
// HOTSPOT_EXPECTED_BEGIN
// define1-empty=java.lang.ClassFormatError msg=Truncated class file
// define1-short2=java.lang.ClassFormatError msg=Truncated class file
// define1-badmagic=java.lang.ClassFormatError msg=Incompatible magic value 3735928559 in class file Bad
// define1-version99=java.lang.UnsupportedClassVersionError msg=Bad has been compiled by a more recent version of the Java Runtime (class file version 99.0), this version of the Java Runtime only recognizes class file versions up to 69.0
// lookup-empty=java.lang.ClassFormatError msg=Invalid ClassFile structure
// lookup-short2=java.lang.ClassFormatError msg=Invalid ClassFile structure
// lookup-badmagic=java.lang.ClassFormatError msg=Incompatible magic value: -559038737
// lookup-version99=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 99.0
// hidden-empty=java.lang.ClassFormatError msg=Invalid ClassFile structure
// hidden-short2=java.lang.ClassFormatError msg=Invalid ClassFile structure
// hidden-badmagic=java.lang.ClassFormatError msg=Incompatible magic value: -559038737
// hidden-version99=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 99.0
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.Callable;

public class L5W94DefineDoorErrors {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W94DefineDoorErrors.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String attempt(Callable<Object> c) {
        try {
            return "ok " + c.call();
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static byte[] header(int magic, int major) {
        byte[] b = new byte[16];
        b[0] = (byte) (magic >>> 24);
        b[1] = (byte) (magic >>> 16);
        b[2] = (byte) (magic >>> 8);
        b[3] = (byte) magic;
        b[6] = (byte) (major >>> 8);
        b[7] = (byte) major;
        return b;
    }

    public static void main(String[] args) {
        Map<String, byte[]> shapes = new LinkedHashMap<>();
        shapes.put("empty", new byte[0]);
        shapes.put("short2", new byte[] {(byte) 0xCA, (byte) 0xFE});
        shapes.put("badmagic", header(0xDEADBEEF, 69));
        shapes.put("version99", header(0xCAFEBABE, 99));
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (Map.Entry<String, byte[]> e : shapes.entrySet()) {
            byte[] b = e.getValue();
            System.out.println("define1-" + e.getKey() + "=" + attempt(() -> new Loader().define("Bad", b)));
        }
        for (Map.Entry<String, byte[]> e : shapes.entrySet()) {
            byte[] b = e.getValue();
            System.out.println("lookup-" + e.getKey() + "=" + attempt(() -> lookup.defineClass(b)));
        }
        for (Map.Entry<String, byte[]> e : shapes.entrySet()) {
            byte[] b = e.getValue();
            System.out.println("hidden-" + e.getKey() + "="
                    + attempt(() -> lookup.defineHiddenClass(b, true)));
        }
    }
}
