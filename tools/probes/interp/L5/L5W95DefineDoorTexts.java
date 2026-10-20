// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 95, lane L5
// (`i42-L5-proposal-one-classfileparser-shaped-error-for-every-define-door`):
// the header refusals of the define doors wave 94 left with the VM's own text
// ("<door>: not a valid class file (bad magic)"), next to the doors wave 94
// already aligned, for the two header shapes L5W94DefineDoorErrors does not
// try (three bytes of the magic; the whole magic and only two more bytes).
//
// Doors:
//   define1  `ClassLoader.defineClass("Bad", b, 0, len)` on a user loader
//            (HotSpot: `defineClass1` -> `ClassFileParser`)
//   lookup   `MethodHandles.lookup().defineClass(b)`
//            (JDK 25: `Lookup.validateAndFindInternalName` in Java first)
//   hidden   `MethodHandles.lookup().defineHiddenClass(b, true)` (the same)
//   unsafe   `jdk.internal.misc.Unsafe.defineClass("Bad", b, 0, len, loader, null)`
//            (HotSpot: `defineClass0` -> `JVM_DefineClass` -> `ClassFileParser`).
//            Needs `--add-exports java.base/jdk.internal.misc=ALL-UNNAMED`;
//            without it the row prints `unsafe-<shape>=unavailable <exception>`.
// Shapes:
//   short3    0xCA 0xFE 0xBA
//   magic6    0xCAFEBABE, then 0x00 0x00
//   badmagic  0xDEADBEEF, then minor 0, major 69, then zeros (16 bytes)
//
// Output: "<door>-<shape>=<exception class> msg=<message>".
//
// Run: javac -d . L5W95DefineDoorTexts.java
//      java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp . L5W95DefineDoorTexts
//      cratonvm --jdk-only   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp . L5W95DefineDoorTexts
//      cratonvm --compatible --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp . L5W95DefineDoorTexts
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources, not run. `define1` /
// `unsafe`: `ClassFileParser` (`guarantee_more(8)` -> "Truncated class file";
// the magic as an unsigned decimal and the name in internal form, as wave 42's
// `L5W42DefineClassErrors` measured for `define1` on 25.0.3; `Unsafe.defineClass`
// checks only `b == null` and `len < 0` in Java). `lookup` / `hidden`:
// `Lookup.validateAndFindInternalName` (`readInt` -> "Invalid ClassFile
// structure" under four bytes; a signed `int` magic; `readUnsignedShort` of the
// major version -> "Invalid ClassFile structure" under eight bytes).
// HOTSPOT_EXPECTED_BEGIN
// define1-short3=java.lang.ClassFormatError msg=Truncated class file
// define1-magic6=java.lang.ClassFormatError msg=Truncated class file
// define1-badmagic=java.lang.ClassFormatError msg=Incompatible magic value 3735928559 in class file Bad
// lookup-short3=java.lang.ClassFormatError msg=Invalid ClassFile structure
// lookup-magic6=java.lang.ClassFormatError msg=Invalid ClassFile structure
// lookup-badmagic=java.lang.ClassFormatError msg=Incompatible magic value: -559038737
// hidden-short3=java.lang.ClassFormatError msg=Invalid ClassFile structure
// hidden-magic6=java.lang.ClassFormatError msg=Invalid ClassFile structure
// hidden-badmagic=java.lang.ClassFormatError msg=Incompatible magic value: -559038737
// unsafe-short3=java.lang.ClassFormatError msg=Truncated class file
// unsafe-magic6=java.lang.ClassFormatError msg=Truncated class file
// unsafe-badmagic=java.lang.ClassFormatError msg=Incompatible magic value 3735928559 in class file Bad
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.LinkedHashMap;
import java.util.Map;

public class L5W95DefineDoorTexts {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W95DefineDoorTexts.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    interface Door {
        Object run(byte[] b) throws Throwable;
    }

    static Object unsafe;
    static Method unsafeDefine;
    static Throwable unsafeUnavailable;

    static void report(String row, Door door, byte[] b) {
        try {
            Object r = door.run(b);
            System.out.println(row + "=defined " + r);
        } catch (Throwable t) {
            Throwable e = t;
            if (e instanceof InvocationTargetException && e.getCause() != null) {
                e = e.getCause();
            }
            System.out.println(row + "=" + e.getClass().getName() + " msg=" + e.getMessage());
        }
    }

    public static void main(String[] args) throws Throwable {
        try {
            Class<?> u = Class.forName("jdk.internal.misc.Unsafe");
            unsafe = u.getMethod("getUnsafe").invoke(null);
            unsafeDefine = u.getMethod("defineClass", String.class, byte[].class, int.class,
                    int.class, ClassLoader.class, ProtectionDomain.class);
        } catch (Throwable t) {
            unsafeUnavailable = t;
        }
        Map<String, byte[]> shapes = new LinkedHashMap<>();
        shapes.put("short3", new byte[] {(byte) 0xCA, (byte) 0xFE, (byte) 0xBA});
        shapes.put("magic6", new byte[] {(byte) 0xCA, (byte) 0xFE, (byte) 0xBA, (byte) 0xBE, 0, 0});
        byte[] bad = new byte[16];
        bad[0] = (byte) 0xDE;
        bad[1] = (byte) 0xAD;
        bad[2] = (byte) 0xBE;
        bad[3] = (byte) 0xEF;
        bad[7] = 69;
        shapes.put("badmagic", bad);

        Map<String, Door> doors = new LinkedHashMap<>();
        doors.put("define1", b -> new Loader().define("Bad", b));
        doors.put("lookup", b -> MethodHandles.lookup().defineClass(b));
        doors.put("hidden", b -> MethodHandles.lookup().defineHiddenClass(b, true));
        doors.put("unsafe", b -> {
            if (unsafeDefine == null) {
                return "unavailable " + unsafeUnavailable.getClass().getName();
            }
            return unsafeDefine.invoke(unsafe, "Bad", b, 0, b.length, new Loader(), null);
        });
        for (Map.Entry<String, Door> door : doors.entrySet()) {
            for (Map.Entry<String, byte[]> shape : shapes.entrySet()) {
                String row = door.getKey() + "-" + shape.getKey();
                if (door.getKey().equals("unsafe") && unsafeDefine == null) {
                    System.out.println(row + "=unavailable " + unsafeUnavailable.getClass().getName());
                    continue;
                }
                report(row, door.getValue(), shape.getValue().clone());
            }
        }
    }
}
