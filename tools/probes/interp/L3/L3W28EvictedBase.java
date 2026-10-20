// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 28, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-an-evicted-retransformation-base-skips-the-class-FIXED-20260929.md):
// retransformClasses of a class whose retransformation base CratonVM's 16 MiB
// class-bytes FIFO has evicted, for four kinds of base:
//   load-transformed  the class file a non-retransformable transformer changed
//                     at load ("tag-orig" -> "tag-load");
//   redefined         the class file redefineClasses installed ("tag-rdef");
//   other-loader      a class a URLClassLoader defined from a directory that is
//                     not on the class path;
//   generated         a class defined from bytes that are no resource anywhere.
// Between the setup and the retransforms the probe defines 400 generated
// classes of ~64 KB each (~26 MB of class files) through its own loader, so
// every base in the FIFO is evicted. A retransform-capable transformer then
// reports the base it is handed and turns "tag-XXXX" into "tag-retr".
//
// HotSpot 25 prints (agent; -Xint too):
//     load-transformed: calls=1 base=tag-load value=tag-retr
//     redefined: calls=1 base=tag-rdef value=tag-retr
//     other-loader: calls=1 base=tag-orig value=tag-retr
//     generated: calls=1 base=tag-orig value=tag-retr
// CratonVM before wave 28 (read from the code): the evicted base was replaced
// by the class-path file (a different build: skipped with a warning) or by
// nothing (skipped), so the transformer never ran:
//     load-transformed: calls=0 base=- value=tag-load
//     redefined: calls=0 base=- value=tag-rdef
//     other-loader: calls=0 base=- value=tag-orig
//     generated: calls=0 base=- value=tag-orig
// CratonVM since wave 28: the first two bases are pinned out of the FIFO's
// reach and the third is read back through the class's defining loader, so the
// first three rows match HotSpot. Since wave 29 `generated` matches too: as
// HotSpot does, CratonVM reconstitutes a class file from the live class
// (docs/internal/fixed-bugs/interpreter-L3-a-generated-class-whose-base-was-evicted-is-not-retransformed-FIXED-20260930.md;
// `L3W29ReconstitutedBase`).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W28EvictedBase$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W28EvictedBase*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W28EvictedBase
// Without the agent both VMs print "no agent".
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.ProtectionDomain;

public class L3W28EvictedBase {
    static volatile Instrumentation inst;

    static final String LOADED = "L3W28EvictedBase$Loaded";

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
            // Not retransform-capable: its output is the class file JVMTI keeps.
            instrumentation.addTransformer(new AtLoad(), false);
        }
    }

    public static class Loaded { public static String value() { return "tag-orig"; } }
    public static class Redefined { public static String value() { return "tag-orig"; } }
    // Renamed to ...$OtherCpy / ...$GenerCpy (same length) before being defined.
    public static class OtherDnr { public static String value() { return "tag-orig"; } }
    public static class GenerDnr { public static String value() { return "tag-orig"; } }

    /** Every "tag-orig" of `b` turned into `to` (four letters). */
    static byte[] retag(byte[] b, String to) {
        byte[] out = b.clone();
        for (int i = 0; i + 8 <= out.length; i++) {
            if (out[i] == 't' && out[i + 1] == 'a' && out[i + 2] == 'g' && out[i + 3] == '-') {
                for (int k = 0; k < 4; k++) {
                    out[i + 4 + k] = (byte) to.charAt(k);
                }
            }
        }
        return out;
    }

    /**
     * Which tag the class file carries: the newest of its tags. HotSpot
     * reconstitutes a redefined class's file from the live class, whose
     * constant pool still holds the replaced "tag-orig" beside "tag-rdef".
     */
    static String tagOf(byte[] b) {
        String best = "none";
        String[] order = {"tag-orig", "tag-load", "tag-rdef"};
        for (int i = 0; i + 8 <= b.length; i++) {
            if (b[i] == 't' && b[i + 1] == 'a' && b[i + 2] == 'g' && b[i + 3] == '-') {
                String tag = new String(b, i, 8, java.nio.charset.StandardCharsets.ISO_8859_1);
                for (int k = 0; k < order.length; k++) {
                    if (order[k].equals(tag) && k > java.util.Arrays.asList(order).indexOf(best)) {
                        best = tag;
                    }
                }
            }
        }
        return best;
    }

    static final class AtLoad implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined != null || !LOADED.equals(className)) {
                return null;
            }
            return retag(bytes, "load");
        }
    }

    /** Retransform-capable: records the base it is handed, installs "tag-retr". */
    static final class Retr implements ClassFileTransformer {
        String target;
        int calls;
        String base = "-";

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null || !className.equals(target)) {
                return null;
            }
            calls++;
            base = tagOf(bytes);
            return retag(bytes, "retr");
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W28EvictedBase.class
                .getResourceAsStream("L3W28EvictedBase$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** The donor's class file, every occurrence of its name turned into `target`. */
    static byte[] renamed(String donor, String target) throws Exception {
        byte[] b = bytesOf(donor);
        String from = "L3W28EvictedBase$" + donor;
        String to = "L3W28EvictedBase$" + target;
        for (int i = 0; i + from.length() <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length() && match; k++) {
                match = b[i + k] == (byte) from.charAt(k);
            }
            if (match) {
                for (int k = 0; k < to.length(); k++) {
                    b[i + k] = (byte) to.charAt(k);
                }
            }
        }
        return b;
    }

    /** A loader that defines what it is handed. */
    static final class Definer extends ClassLoader {
        Definer(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** A method-less class `name` carrying one unused `pad`-byte constant. */
    static byte[] bigClass(String name, int pad) throws Exception {
        ByteArrayOutputStream bos = new ByteArrayOutputStream(pad + 128);
        DataOutputStream out = new DataOutputStream(bos);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(6);
        out.writeByte(1);
        out.writeUTF(name);
        out.writeByte(7);
        out.writeShort(1);
        out.writeByte(1);
        out.writeUTF("java/lang/Object");
        out.writeByte(7);
        out.writeShort(3);
        out.writeByte(1);
        out.writeUTF("x".repeat(pad));
        out.writeShort(0x0021);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bos.toByteArray();
    }

    static String value(Class<?> c) throws Exception {
        return (String) c.getMethod("value").invoke(null);
    }

    static void retransform(String label, Class<?> c) throws Exception {
        Retr retr = new Retr();
        retr.target = c.getName().replace('.', '/');
        inst.addTransformer(retr, true);
        try {
            inst.retransformClasses(c);
        } catch (Throwable t) {
            System.out.println(label + ": threw " + t);
        }
        inst.removeTransformer(retr);
        System.out.println(label + ": calls=" + retr.calls + " base=" + retr.base
                + " value=" + value(c));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported() || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        ClassLoader app = L3W28EvictedBase.class.getClassLoader();

        // First touch by invokestatic, the load L3W26LoadHookFirstTouch covers.
        String first = Loaded.value() + Redefined.value();
        if (!first.equals("tag-loadtag-orig")) {
            System.out.println("setup: " + first);
        }
        Class<?> loaded = Loaded.class;
        Class<?> redefined = Redefined.class;
        i.redefineClasses(new ClassDefinition(redefined, retag(bytesOf("Redefined"), "rdef")));

        Path dir = Files.createTempDirectory("l3w28");
        Path file = dir.resolve("L3W28EvictedBase$OtherCpy.class");
        Files.write(file, renamed("OtherDnr", "OtherCpy"));
        URLClassLoader otherLoader = new URLClassLoader(new URL[] {dir.toUri().toURL()}, app);
        Class<?> other = otherLoader.loadClass("L3W28EvictedBase$OtherCpy");
        value(other);

        Definer definer = new Definer(app);
        Class<?> generated = definer.define("L3W28EvictedBase$GenerCpy",
                renamed("GenerDnr", "GenerCpy"));
        value(generated);

        // ~26 MB of class files through another loader: the FIFO keeps 16 MiB.
        Definer flood = new Definer(app);
        for (int n = 0; n < 400; n++) {
            String name = String.format("L3W28Flood%03d", n);
            flood.define(name, bigClass(name, 64000));
        }

        retransform("load-transformed", loaded);
        retransform("redefined", redefined);
        retransform("other-loader", other);
        retransform("generated", generated);

        otherLoader.close();
        Files.delete(file);
        Files.delete(dir);
    }
}
