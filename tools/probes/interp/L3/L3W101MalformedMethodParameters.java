import java.lang.classfile.*;
import java.lang.classfile.attribute.*;
import java.lang.classfile.constantpool.*;
import java.lang.constant.*;
import java.lang.reflect.*;
import java.util.*;

/**
 * Interpreter round i5 wave 101, lane L3: {@code Executable.getParameters()}
 * over malformed {@code MethodParameters} attributes (class files javac never
 * writes, generated with java.lang.classfile). HotSpot's
 * {@code JVM_GetMethodParameters} answers the attribute's own entries (sized
 * by the attribute, an empty name kept, a bad name_index an
 * IllegalArgumentException) and the JDK's {@code verifyParameters} throws
 * MalformedParametersException. Run both modes; every line must match HotSpot.
 */
public class L3W101MalformedMethodParameters {
    static final MethodTypeDesc II = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final MethodTypeDesc I = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int);

    static byte[] gen(String name, MethodTypeDesc mtd, List<MethodParameterInfo> infos) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.withMethod("m", mtd, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, mb -> {
                mb.with(MethodParametersAttribute.of(infos));
                mb.withCode(c -> c.return_());
            });
        });
    }

    static MethodParameterInfo p(String name, int flags) {
        return MethodParameterInfo.ofParameter(Optional.ofNullable(name), flags);
    }

    /** Point the single entry named QQQQ at {@code newIndex} instead. */
    static byte[] repoint(byte[] bytes, int newIndex) {
        ClassModel cm = ClassFile.of().parse(bytes);
        int q = -1;
        for (PoolEntry e : cm.constantPool()) {
            if (e instanceof Utf8Entry u && u.equalsString("QQQQ")) q = e.index();
        }
        byte[] pat = {0, 0, 0, 5, 1, (byte) (q >> 8), (byte) q, 0, 0};
        outer:
        for (int i = 0; i + pat.length <= bytes.length; i++) {
            for (int j = 0; j < pat.length; j++) if (bytes[i + j] != pat[j]) continue outer;
            byte[] out = bytes.clone();
            out[i + 5] = (byte) (newIndex >> 8);
            out[i + 6] = (byte) newIndex;
            return out;
        }
        throw new AssertionError("attribute not found");
    }

    static int thisClassIndex(byte[] bytes) {
        return ClassFile.of().parse(bytes).thisClass().index();
    }

    static final class Loader extends ClassLoader {
        Loader() { super(L3W101MalformedMethodParameters.class.getClassLoader()); }
        Class<?> define(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
    }

    static void run(String label, String cls, byte[] bytes) {
        String line;
        try {
            Class<?> c = new Loader().define(cls, bytes);
            Method m = c.getDeclaredMethods()[0];
            StringBuilder sb = new StringBuilder();
            for (Parameter par : m.getParameters()) {
                sb.append(par.getName()).append(':').append(par.isNamePresent())
                  .append(':').append(par.getModifiers()).append(':').append(par.isImplicit()).append(' ');
            }
            line = "OK " + sb.toString().trim();
        } catch (Throwable t) {
            line = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + " -> " + line);
    }

    public static void main(String[] args) {
        run("valid", "MP0", gen("MP0", II, List.of(p("a", 0), p("b", 0x10))));
        run("unnamed-mandated", "MP1", gen("MP1", I, List.of(p(null, 0x8000))));
        run("count-short", "MP2", gen("MP2", II, List.of(p("a", 0))));
        run("count-zero", "MP3", gen("MP3", I, List.of()));
        run("count-long", "MP4", gen("MP4", I, List.of(p("a", 0), p("b", 0))));
        run("empty-name", "MP5", gen("MP5", I, List.of(p("", 0))));
        run("dot-name", "MP6", gen("MP6", I, List.of(p("a.b", 0))));
        run("slash-name", "MP7", gen("MP7", I, List.of(p("a/b", 0))));
        run("public-flag", "MP8", gen("MP8", I, List.of(p("a", 0x0001))));
        run("synthetic-flag", "MP9", gen("MP9", I, List.of(p("a", 0x1000))));
        byte[] wrongType = gen("MPA", I, List.of(p("QQQQ", 0)));
        run("name-index-class", "MPA", repoint(wrongType, thisClassIndex(wrongType)));
        byte[] oob = gen("MPB", I, List.of(p("QQQQ", 0)));
        run("name-index-oob", "MPB", repoint(oob, 0x7FFF));
    }
}
