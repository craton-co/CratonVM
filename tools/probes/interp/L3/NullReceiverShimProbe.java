// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 10, lane L3: the `--compatible` null-receiver
// shims of `execute_invoke_kind` (File / URL / Class accessors answering
// null, 0 or false, and `Unsafe` dispatched on the constant-pool class) are
// deleted, so every row below throws the JEP 358 NullPointerException, as on
// HotSpot. Before the change `--compatible` printed "no exception" rows.
//
// HotSpot 25 prints, for example:
//   File.exists: Cannot invoke "java.io.File.exists()" because "NullReceiverShimProbe.FILE" is null
// and the same shape for every row, then "hot File.exists: 20000 NPEs" and
// "hot Class.getSuperclass: 20000 NPEs".
// Compare stdout under --compatible with and without --nojit.

import java.io.File;
import java.net.URL;

public class NullReceiverShimProbe {
    static File FILE;
    static URL URL_;
    static Class<?> CLASS;
    @SuppressWarnings("removal")
    static sun.misc.Unsafe UNSAFE;

    interface Row {
        Object run() throws Exception;
    }

    static void row(String name, Row r) {
        try {
            Object v = r.run();
            System.out.println(name + ": no exception, answered " + v);
        } catch (NullPointerException e) {
            System.out.println(name + ": " + e.getMessage());
        } catch (Exception e) {
            System.out.println(name + ": " + e.getClass().getName());
        }
    }

    static int hotExists() {
        int npes = 0;
        for (int i = 0; i < 20000; i++) {
            try {
                if (FILE.exists()) {
                    npes -= 1000000;
                }
            } catch (NullPointerException e) {
                npes++;
            }
        }
        return npes;
    }

    static int hotSuperclass() {
        int npes = 0;
        for (int i = 0; i < 20000; i++) {
            try {
                if (CLASS.getSuperclass() != null) {
                    npes -= 1000000;
                }
            } catch (NullPointerException e) {
                npes++;
            }
        }
        return npes;
    }

    @SuppressWarnings("removal")
    public static void main(String[] a) {
        row("File.exists", () -> FILE.exists());
        row("File.isDirectory", () -> FILE.isDirectory());
        row("File.length", () -> FILE.length());
        row("File.lastModified", () -> FILE.lastModified());
        row("File.getParentFile", () -> FILE.getParentFile());
        row("File.getAbsolutePath", () -> FILE.getAbsolutePath());
        row("File.list", () -> FILE.list());
        row("URL.getHost", () -> URL_.getHost());
        row("URL.getProtocol", () -> URL_.getProtocol());
        row("Class.getSuperclass", () -> CLASS.getSuperclass());
        row("Class.getComponentType", () -> CLASS.getComponentType());
        row("Class.componentType", () -> CLASS.componentType());
        row("Class.arrayType", () -> CLASS.arrayType());
        row("Class.getInterfaces", () -> CLASS.getInterfaces().length);
        row("Unsafe.addressSize", () -> UNSAFE.addressSize());
        System.out.println("hot File.exists: " + hotExists() + " NPEs");
        System.out.println("hot Class.getSuperclass: " + hotSuperclass() + " NPEs");
    }
}
