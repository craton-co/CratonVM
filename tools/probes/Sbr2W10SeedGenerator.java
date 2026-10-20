// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR2 wave 10 (PUREDOORS family F3): {@code sun.security.provider.NativeSeedGenerator.nativeGenerateSeed(byte[])}
 * is {@code native} on Windows and had no registration, so the real JDK
 * {@code SeedGenerator} died with an {@code UnsatisfiedLinkError} once nothing
 * stood in front of it. Run under {@code CRATONVM_PURE_BYTECODE=1}:
 * <pre>cratonvm --jdk-only -cp . Sbr2W10SeedGenerator</pre>
 * Expected: {@code generateSeed ok nonzero=true} (also on HotSpot, with
 * {@code --add-opens java.base/sun.security.provider=ALL-UNNAMED}).
 */
public class Sbr2W10SeedGenerator {
    public static void main(String[] a) throws Exception {
        Class<?> c = Class.forName("sun.security.provider.SeedGenerator");
        java.lang.reflect.Method m = c.getDeclaredMethod("generateSeed", byte[].class);
        m.setAccessible(true);
        byte[] b = new byte[16];
        m.invoke(null, (Object) b);
        int nz = 0;
        for (byte x : b) if (x != 0) nz++;
        System.out.println("generateSeed ok nonzero=" + (nz > 0));
    }
}
