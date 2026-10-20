// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR round 5, lane {@code sysprops}: the first {@code java.nio.file} use after boot, on the
 * JDK 21 and 25 images ({@code user.dir} must be absolute in {@code StaticProperty}).
 */
public final class Sbr5SyspropsFs {
    public static void main(String[] a) throws Exception {
        System.out.println("user.dir absolute = " + System.getProperty("user.dir").startsWith("/"));
        System.out.println("path = " + java.nio.file.Path.of("x").toAbsolutePath().isAbsolute());
        System.out.println("file = " + new java.io.File("x").getAbsolutePath().startsWith("/"));
    }
}
