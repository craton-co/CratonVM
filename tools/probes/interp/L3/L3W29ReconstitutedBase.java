// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-a-generated-class-whose-base-was-evicted-is-not-retransformed-FIXED-20260930.md):
// retransformClasses of GENERATED classes -- defined through a loader from
// bytes that are no resource anywhere, as Byte Buddy / CGLIB / a test
// generator define them -- after CratonVM's 16 MiB class-bytes FIFO evicted
// their retransformation base. HotSpot reconstitutes a class file from the
// live class when it has none cached; CratonVM skipped the class.
//   generated        a small generated class: the transformer must be handed
//                    the class's own file ("tag-orig") and its output installed;
//   shape            a generated class with everything a written-back file
//                    must carry (two bootstrap methods: a lambda and a string
//                    concatenation; a handler; a tableswitch; long and double
//                    constants; generic signatures; class, method and
//                    parameter annotations; InnerClasses / NestHost), checked
//                    by running and reflecting on the RETRANSFORMED class;
//   generated-again  the first class retransformed a second time after
//                    another flood: the base must still be the original file,
//                    not the woven one (which carries no "tag-orig":
//                    base=none).
// Each flood defines 400 generated classes of ~64 KB (~26 MB of class files)
// through another loader.
//
// HotSpot 25 prints (agent; -Xint too):
//     generated: calls=1 base=tag-orig value=tag-retr
//     shape: calls=1 base=tag-orig value=tag-retr lambda=lam-1234605616436508552 guarded=ok5,caught switch=one,seven,other consts=1122334455667788,0.5 generic=java.util.List<java.lang.String> typevar=T:java.lang.Comparable<T> mark=shape,m,p
//     generated-again: calls=1 base=tag-orig value=tag-retr
// CratonVM before wave 29 (read from the code): no base was found for a
// generated class once evicted, the class was skipped with a warning and the
// transformer never ran:
//     generated: calls=0 base=- value=tag-orig
//     shape: calls=0 base=- value=tag-orig lambda=... (the rest as HotSpot)
//     generated-again: calls=0 base=- value=tag-orig
// CratonVM since wave 29: the three HotSpot lines. Positive control: with
// CRATONVM_DBG_RETRANSFORM=1 stderr carries
//     [RETRANSFORM] reconstituted the base of L3W29ReconstitutedBase$GenerCpy from the live class: <n> bytes
// (and one for ...$ShapeCpy), and NO such line for the `generated-again`
// retransform (its base is pinned since the first retransform).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W29ReconstitutedBase$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W29ReconstitutedBase*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W29ReconstitutedBase
// Without the agent both VMs print "no agent".
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.InputStream;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.lang.reflect.TypeVariable;
import java.security.ProtectionDomain;
import java.util.List;
import java.util.function.Supplier;

public class L3W29ReconstitutedBase {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    @Retention(RetentionPolicy.RUNTIME)
    public @interface Mark {
        String value();
    }

    // Renamed to ...$GenerCpy / ...$ShapeCpy (same length) before being defined.
    public static class GenerDnr {
        public static String value() {
            return "tag-orig";
        }
    }

    @Mark("shape")
    public static class ShapeDnr<T extends Comparable<T>> {
        public static final long BIG = 0x1122334455667788L;
        public static final double HALF = 0.5;

        public static String value() {
            return "tag-orig";
        }

        public static List<String> names() {
            return List.of("a");
        }

        public static String lambda() {
            Supplier<String> s = () -> "lam-" + BIG;
            return s.get();
        }

        public static String guarded(int x) {
            try {
                return "ok" + (10 / x);
            } catch (ArithmeticException e) {
                return "caught";
            }
        }

        public static String pick(int k) {
            switch (k) {
                case 1: return "one";
                case 2: return "two";
                case 3: return "three";
                case 7: return "seven";
                default: return "other";
            }
        }

        @Mark("m")
        public static int annotated(@Mark("p") int p) {
            return p;
        }
    }

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

    /** "tag-orig" if the class file carries it, else "none". */
    static String tagOf(byte[] b) {
        for (int i = 0; i + 8 <= b.length; i++) {
            if (b[i] == 't' && b[i + 1] == 'a' && b[i + 2] == 'g' && b[i + 3] == '-'
                    && b[i + 4] == 'o' && b[i + 5] == 'r' && b[i + 6] == 'i' && b[i + 7] == 'g') {
                return "tag-orig";
            }
        }
        return "none";
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
        try (InputStream in = L3W29ReconstitutedBase.class
                .getResourceAsStream("L3W29ReconstitutedBase$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** The donor's class file, every occurrence of its name turned into `target`. */
    static byte[] renamed(String donor, String target) throws Exception {
        byte[] b = bytesOf(donor);
        String from = "L3W29ReconstitutedBase$" + donor;
        String to = "L3W29ReconstitutedBase$" + target;
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

    /** ~26 MB of class files through a loader of its own: the FIFO keeps 16 MiB. */
    static void flood(ClassLoader parent, String prefix) throws Exception {
        Definer flood = new Definer(parent);
        for (int n = 0; n < 400; n++) {
            String name = String.format("%s%03d", prefix, n);
            flood.define(name, bigClass(name, 64000));
        }
    }

    static Object call(Class<?> c, String name, Object... args) throws Exception {
        for (Method m : c.getMethods()) {
            if (m.getName().equals(name)) {
                return m.invoke(null, args);
            }
        }
        throw new NoSuchMethodException(name);
    }

    static String retransform(Class<?> c) throws Exception {
        Retr retr = new Retr();
        retr.target = c.getName().replace('.', '/');
        inst.addTransformer(retr, true);
        String threw = "";
        try {
            inst.retransformClasses(c);
        } catch (Throwable t) {
            threw = " threw " + t;
        }
        inst.removeTransformer(retr);
        return "calls=" + retr.calls + " base=" + retr.base + " value=" + call(c, "value") + threw;
    }

    static String shapeOf(Class<?> c) throws Exception {
        StringBuilder sb = new StringBuilder();
        sb.append(" lambda=").append(call(c, "lambda"));
        sb.append(" guarded=").append(call(c, "guarded", 2)).append(',').append(call(c, "guarded", 0));
        sb.append(" switch=").append(call(c, "pick", 1)).append(',').append(call(c, "pick", 7))
                .append(',').append(call(c, "pick", 9));
        sb.append(" consts=").append(Long.toHexString(c.getField("BIG").getLong(null)))
                .append(',').append(c.getField("HALF").getDouble(null));
        sb.append(" generic=").append(c.getMethod("names").getGenericReturnType().getTypeName());
        TypeVariable<?> t = c.getTypeParameters()[0];
        sb.append(" typevar=").append(t.getName()).append(':').append(t.getBounds()[0].getTypeName());
        Method annotated = c.getMethod("annotated", int.class);
        sb.append(" mark=").append(c.getAnnotation(Mark.class).value())
                .append(',').append(annotated.getAnnotation(Mark.class).value())
                .append(',').append(((Mark) annotated.getParameterAnnotations()[0][0]).value());
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        ClassLoader app = L3W29ReconstitutedBase.class.getClassLoader();
        Definer definer = new Definer(app);
        Class<?> generated = definer.define("L3W29ReconstitutedBase$GenerCpy",
                renamed("GenerDnr", "GenerCpy"));
        call(generated, "value");
        Class<?> shape = definer.define("L3W29ReconstitutedBase$ShapeCpy",
                renamed("ShapeDnr", "ShapeCpy"));
        call(shape, "value");

        flood(app, "L3W29FloodA");
        System.out.println("generated: " + retransform(generated));
        System.out.println("shape: " + retransform(shape) + shapeOf(shape));

        flood(app, "L3W29FloodB");
        System.out.println("generated-again: " + retransform(generated));
    }
}
