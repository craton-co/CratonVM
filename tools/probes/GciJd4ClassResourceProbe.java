// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.net.URL;
import java.util.zip.CRC32;

/**
 * gci i4/jd (2026-10-12): {@code Class.getResourceAsStream} / {@code Class.getResource}
 * of classes of every loader kind, asked again in each round.
 *
 * <p>{@code --compatible} answered {@code null} for {@code java.sql.Connection}'s own
 * {@code Connection.class} from the second ask on
 * ({@code docs/known-issues/jdk-only/gci-i3jd-compatible-platform-class-resource-is-null-after-the-first-round-20261012.md}):
 * a boot module class ({@code java.base}), two platform-module classes
 * ({@code java.sql}, {@code java.scripting}), an application-loader image module
 * class ({@code jdk.compiler}'s exported {@code com.sun.source.util.JavacTask}), this
 * class (class path), an absolute name through this class, and a missing name.
 *
 * <p>stdout (deterministic; HotSpot 25 is the oracle, same JDK image): one line per
 * round, then {@code PASS class-resource rounds=R}. Diagnostics on stderr.
 *
 * <p>Run: {@code cratonvm -cp <classes> GciJd4ClassResourceProbe 3}.
 */
public class GciJd4ClassResourceProbe {
    static String stream(Class<?> c, String name) {
        try (InputStream in = c.getResourceAsStream(name)) {
            if (in == null) {
                return "null";
            }
            byte[] b = in.readAllBytes();
            CRC32 crc = new CRC32();
            crc.update(b, 0, b.length);
            return b.length + "/" + Long.toHexString(crc.getValue());
        } catch (Exception e) {
            return "threw " + e.getClass().getName();
        }
    }

    static String url(Class<?> c, String name) {
        try {
            URL u = c.getResource(name);
            return u == null ? "null" : "url";
        } catch (Exception e) {
            return "threw " + e.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        Class<?>[] classes = {
            String.class,
            Class.forName("java.sql.Connection"),
            Class.forName("javax.script.ScriptEngine"),
            Class.forName("com.sun.source.util.JavacTask"),
            GciJd4ClassResourceProbe.class,
        };
        String[] names = {
            "String.class",
            "Connection.class",
            "ScriptEngine.class",
            "JavacTask.class",
            "GciJd4ClassResourceProbe.class",
        };
        long t0 = System.nanoTime();
        for (int r = 1; r <= rounds; r++) {
            StringBuilder line = new StringBuilder("round ").append(r);
            for (int i = 0; i < classes.length; i++) {
                line.append(' ').append(names[i]).append('=').append(stream(classes[i], names[i]))
                        .append(',').append(url(classes[i], names[i]));
            }
            line.append(" abs=").append(stream(GciJd4ClassResourceProbe.class, "/java/lang/Object.class"));
            line.append(" missing=").append(stream(GciJd4ClassResourceProbe.class, "NoSuchResource42.txt"))
                    .append(',').append(url(GciJd4ClassResourceProbe.class, "NoSuchResource42.txt"));
            System.out.println(line);
        }
        System.out.println("PASS class-resource rounds=" + rounds);
        System.err.println("[class-resource] ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
