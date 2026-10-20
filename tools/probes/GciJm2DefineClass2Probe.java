// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.nio.ByteBuffer;

/**
 * gci i2/jm (2026-10-12): {@code ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain)}
 * over a DIRECT buffer whose position is not 0. The JDK passes {@code b.position()} as the native
 * {@code defineClass2}'s offset; HotSpot reads {@code address + offset}
 * ({@code docs/known-issues/jdk-only/gci-i2jm-defineclass2-direct-buffer-counts-its-position-twice-20261012.md}).
 *
 * <p>stdout (deterministic; HotSpot 25 is the oracle): one line per prefix length, then
 * {@code PASS define-class2}. Run: {@code cratonvm -cp <classes> GciJm2DefineClass2Probe}.
 */
public class GciJm2DefineClass2Probe {
    /** The class defined again, by its bytes, in a fresh loader. */
    public static class Payload {
        @Override
        public String toString() {
            return "payload-ok";
        }
    }

    static final class BufferLoader extends ClassLoader {
        BufferLoader() {
            super(GciJm2DefineClass2Probe.class.getClassLoader());
        }

        Class<?> define(String name, ByteBuffer bytes) {
            return defineClass(name, bytes, null);
        }
    }

    public static void main(String[] args) throws Exception {
        byte[] klass;
        try (InputStream in = GciJm2DefineClass2Probe.class.getResourceAsStream(
                "GciJm2DefineClass2Probe$Payload.class")) {
            klass = in.readAllBytes();
        }
        String name = "GciJm2DefineClass2Probe$Payload";
        for (int prefix : new int[] {0, 8, 37}) {
            ByteBuffer direct = ByteBuffer.allocateDirect(prefix + klass.length);
            for (int i = 0; i < prefix; i++) {
                direct.put((byte) 0x5A);
            }
            direct.put(klass);
            direct.flip();
            direct.position(prefix);
            String outcome;
            try {
                Class<?> c = new BufferLoader().define(name, direct);
                Object o = c.getDeclaredConstructor().newInstance();
                outcome = "defined " + o + " position=" + direct.position();
            } catch (Throwable t) {
                outcome = "threw " + t.getClass().getName();
            }
            System.out.println("prefix=" + prefix + " " + outcome);
        }
        System.out.println("PASS define-class2");
    }
}
