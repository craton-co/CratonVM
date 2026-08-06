// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.spi.FileSystemProvider;
import java.util.Arrays;
import java.util.Collection;
import java.util.Map;

/**
 * The name-keyed half of the NIO file-attribute surface:
 * {@code Files.readAttributes(path, "view:attrs")} and
 * {@code Files.getAttribute(path, "view:name")}.
 *
 * <p>Written for the {@code VirtualMachine.list()} diagnosis (2026-08-06). That
 * call threw {@code InternalError}, and the cause two frames down was
 * {@code FileSystemProvider.readAttributes(Path, String, LinkOption...)}
 * resolving to the ABSTRACT declaration on
 * {@code java.nio.file.spi.FileSystemProvider} — CratonVM's default provider
 * object is stamped with that abstract class, so real JDK bytecode had nothing
 * to dispatch to. jvmstat's container detection reads {@code unix:dev}, which
 * is why an attach-API probe was the thing that surfaced it.
 *
 * <p>What it prints is deliberately shape, not value: the provider class name
 * differs between platforms and the numbers differ between runs, so every line
 * is either a boolean, a sorted key set, or a "did it throw" verdict. The one
 * exception is {@code unix:dev}, printed as {@code present}/{@code absent} —
 * its value is the device number of whatever filesystem {@code /tmp} is on.
 */
public class NioAttrProbe {
    public static void main(String[] args) {
        FileSystem fs = FileSystems.getDefault();
        FileSystemProvider provider = fs.provider();
        System.out.println("provider scheme=" + provider.getScheme()
                + " views=" + sorted(fs.supportedFileAttributeViews()));

        Path path = Paths.get(System.getProperty("java.io.tmpdir", "/tmp"));

        System.out.println("basic " + keysOf(path, "basic:*"));
        System.out.println("posix " + keysOf(path, "posix:*"));
        System.out.println("unix " + keysOf(path, "unix:*"));

        // The single-attribute form jvmstat uses. Only its presence is asserted;
        // the device number itself is host state.
        String dev;
        try {
            Object v = Files.getAttribute(path, "unix:dev");
            dev = v == null ? "null" : "present";
        } catch (UnsupportedOperationException e) {
            // Legal on a platform with no unix view (Windows) — not a defect.
            dev = "unsupported";
        } catch (Throwable t) {
            dev = "throw-" + t.getClass().getSimpleName();
        }
        System.out.println("getAttribute unix:dev=" + dev);

        // The error contract, which is the part an empty-map implementation
        // silently passed: an unknown name must throw, not answer null.
        System.out.println("unknownName=" + verdict(path, "basic:noSuchAttribute")
                + " unknownView=" + verdict(path, "noSuchView:*"));
    }

    private static String keysOf(Path path, String spec) {
        try {
            Map<String, Object> m = Files.readAttributes(path, spec);
            return m.isEmpty() ? "empty" : sorted(m.keySet());
        } catch (UnsupportedOperationException e) {
            return "unsupported";
        } catch (Throwable t) {
            return "throw-" + t.getClass().getSimpleName();
        }
    }

    /**
     * Render a collection of names sorted, without going through a sorted
     * collection's own {@code toString}.
     *
     * <p>The obvious {@code new TreeSet<>(c).toString()} makes this probe report
     * a {@code TreeSet$Itr} gap under {@code --jdk-only} instead of whatever it
     * was asked about — a probe that fails for a reason unrelated to its subject
     * measures the wrong thing.
     */
    private static String sorted(Collection<String> c) {
        String[] names = c.toArray(new String[0]);
        Arrays.sort(names);
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < names.length; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            sb.append(names[i]);
        }
        return sb.append(']').toString();
    }

    private static String verdict(Path path, String spec) {
        try {
            Files.readAttributes(path, spec);
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getSimpleName();
        }
    }
}
