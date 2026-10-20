// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * `--jdk-only` boot regression probe: `ServiceLoader` and `java.util.logging`
 * both reach `ClassLoaders.<clinit>`, which reads `VM.savedProps`. When
 * `VM.saveProperties` stored into an uninitialised `VM` the value was wiped by
 * `VM.<clinit>` and every one of these threw `NoClassDefFoundError:
 * jdk/internal/loader/ClassLoaders` for the rest of the process, while a bare
 * `System.out.println` (which needs none of it) kept working and hid it.
 *
 * Expected output, in every mode: hello / false / x
 */
public class ServiceLoaderBootProbe {
    public static void main(String[] a) {
        System.out.println("hello");
        System.out.println(java.util.ServiceLoader.load(Runnable.class).iterator().hasNext());
        System.out.println(java.util.logging.Logger.getLogger("x").getName());
    }
}
