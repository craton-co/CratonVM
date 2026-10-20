// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- the class-file VERSION check of
// JDK 25's `Lookup.ClassFile.validateAndFindInternalName`, which every
// `Lookup.defineClass` / `defineHiddenClass` runs in Java before the VM sees
// the bytes: `VM.isSupportedClassFileVersion(major, minor)` (45..69; from 56
// on the minor must be 0 or 65535), else `UnsupportedClassVersionError:
// Unsupported class file version <major>.<minor>`. CratonVM's `Lookup`
// stand-ins (registered in both modes) checked the magic and the length only
// (wave 95) and handed any version to the backend, which raised HotSpot's
// `ClassFileParser` text instead. Fix: `lookup_define.rs`
// `lookup_header_refusal`. Each class file is 16 bytes: magic, minor, major,
// then zeros, so a SUPPORTED version reaches the ClassFile API parse, which
// fails (`ClassFormatError` with no message, the API's
// `IllegalArgumentException` as its cause) -- the `v52-truncated` row.
//
// Run (no setup):
//   javac -d out L5W106LookupDefineVersions.java
//   java -cp out L5W106LookupDefineVersions                        (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W106LookupDefineVersions
//   cratonvm --compatible [--nojit] -cp out L5W106LookupDefineVersions
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources, not run.
// HOTSPOT_EXPECTED_BEGIN
// lookup-v99.0=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 99.0
// lookup-v70.0=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 70.0
// lookup-v44.0=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 44.0
// lookup-v69.1=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 69.1
// lookup-v60.3=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 60.3
// hidden-v99.0=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 99.0
// hidden-v69.1=java.lang.UnsupportedClassVersionError msg=Unsupported class file version 69.1
// lookup-v52-truncated=java.lang.ClassFormatError msg=null
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandles;

public class L5W106LookupDefineVersions {
    static byte[] header(int major, int minor) {
        byte[] b = new byte[16];
        b[0] = (byte) 0xCA;
        b[1] = (byte) 0xFE;
        b[2] = (byte) 0xBA;
        b[3] = (byte) 0xBE;
        b[4] = (byte) (minor >>> 8);
        b[5] = (byte) minor;
        b[6] = (byte) (major >>> 8);
        b[7] = (byte) major;
        return b;
    }

    interface Door {
        Object run(byte[] b) throws Throwable;
    }

    static void row(String label, Door d, byte[] b) {
        String v;
        try {
            v = "defined " + d.run(b);
        } catch (Throwable t) {
            v = t.getClass().getName() + " msg=" + t.getMessage();
        }
        System.out.println(label + "=" + v);
    }

    public static void main(String[] args) {
        Door lookup = b -> MethodHandles.lookup().defineClass(b);
        Door hidden = b -> MethodHandles.lookup().defineHiddenClass(b, true);
        row("lookup-v99.0", lookup, header(99, 0));
        row("lookup-v70.0", lookup, header(70, 0));
        row("lookup-v44.0", lookup, header(44, 0));
        row("lookup-v69.1", lookup, header(69, 1));
        row("lookup-v60.3", lookup, header(60, 3));
        row("hidden-v99.0", hidden, header(99, 0));
        row("hidden-v69.1", hidden, header(69, 1));
        row("lookup-v52-truncated", lookup, header(52, 0));
    }
}
