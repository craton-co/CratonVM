// Interpreter round i4, wave 98, lane L5 -- `ClassFileParser` refusals a
// user-defined loader meets at `defineClass`, with HotSpot 25's texts:
//
//   extra-bytes   a valid class file plus two trailing zero bytes
//                 -> ClassFormatError: Extra bytes at the end of class file p/E
//                    (`parse_stream`'s `guarantee_property(stream->at_eos())`);
//                 CratonVM said `p/E: invalid class data: class file has 2
//                 trailing bytes after class attributes`.
//   dup-field     two fields `f:I`
//   dup-field-ref two fields `s:Ljava/lang/String;` (the signature prints
//                 through `as_klass_external_name`, slashes as dots)
//   dup-method    two abstract methods `m(Ljava/lang/String;)V`
//                 -> ClassFormatError: Duplicate field|method name "<n>" with
//                    signature "<sig>" in class file <internal name>
//                 (`parse_fields` / `parse_methods`, `_need_verify`, which is
//                 on for every loader but the boot loader); CratonVM defined
//                 these classes silently.
//   field-overload / method-overload  same name, different descriptor: defined.
//
// Run (no setup):
//   javac -d out L5W98ClassFileParseTexts.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W98ClassFileParseTexts
//
// Expected HotSpot 25 output (from the JDK sources; not measured by this lane):
// HOTSPOT_EXPECTED_BEGIN
// extra-bytes: java.lang.ClassFormatError: Extra bytes at the end of class file p/E
// dup-field: java.lang.ClassFormatError: Duplicate field name "f" with signature "I" in class file p/F
// dup-field-ref: java.lang.ClassFormatError: Duplicate field name "s" with signature "Ljava.lang.String;" in class file p/G
// field-overload: defined p.H fields=2
// dup-method: java.lang.ClassFormatError: Duplicate method name "m" with signature "(Ljava.lang.String;)V" in class file p/M
// method-overload: defined p.N methods=2
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.MethodTypeDesc;
import java.util.Arrays;
import java.util.function.Consumer;

public class L5W98ClassFileParseTexts {
    static final ClassDesc OBJECT = ClassDesc.of("java.lang.Object");
    static final ClassDesc STRING = ClassDesc.of("java.lang.String");

    static byte[] cls(String name, int flags, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(flags);
            cb.withSuperclass(OBJECT);
            body.accept(cb);
        });
    }

    static final class Gen extends ClassLoader {
        Gen() {
            super(L5W98ClassFileParseTexts.class.getClassLoader());
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static void run(String label, String name, byte[] b) {
        try {
            Class<?> c = new Gen().def(name, b);
            System.out.println(label + ": defined " + c.getName()
                    + " fields=" + c.getDeclaredFields().length);
        } catch (Throwable t) {
            System.out.println(label + ": " + t);
        }
    }

    static void runMethods(String label, String name, byte[] b) {
        try {
            Class<?> c = new Gen().def(name, b);
            System.out.println(label + ": defined " + c.getName() + " methods=" + c.getDeclaredMethods().length);
        } catch (Throwable t) {
            System.out.println(label + ": " + t);
        }
    }

    public static void main(String[] args) {
        int pub = 0x0001 | 0x0020;
        byte[] e = cls("p.E", pub, cb -> { });
        byte[] extra = Arrays.copyOf(e, e.length + 2);
        run("extra-bytes", "p.E", extra);

        run("dup-field", "p.F", cls("p.F", pub, cb -> {
            cb.withField("f", ClassDesc.ofDescriptor("I"), 0x0001);
            cb.withField("f", ClassDesc.ofDescriptor("I"), 0x0001);
        }));
        run("dup-field-ref", "p.G", cls("p.G", pub, cb -> {
            cb.withField("s", STRING, 0x0001);
            cb.withField("s", STRING, 0x0002);
        }));
        run("field-overload", "p.H", cls("p.H", pub, cb -> {
            cb.withField("f", ClassDesc.ofDescriptor("I"), 0x0001);
            cb.withField("f", ClassDesc.ofDescriptor("J"), 0x0001);
        }));
        MethodTypeDesc takesString = MethodTypeDesc.of(ClassDesc.ofDescriptor("V"), STRING);
        MethodTypeDesc takesInt = MethodTypeDesc.of(ClassDesc.ofDescriptor("V"), ClassDesc.ofDescriptor("I"));
        int abs = 0x0001 | 0x0400;
        runMethods("dup-method", "p.M", cls("p.M", pub | 0x0400, cb -> {
            cb.withMethod("m", takesString, abs, mb -> { });
            cb.withMethod("m", takesString, abs, mb -> { });
        }));
        runMethods("method-overload", "p.N", cls("p.N", pub | 0x0400, cb -> {
            cb.withMethod("m", takesString, abs, mb -> { });
            cb.withMethod("m", takesInt, abs, mb -> { });
        }));
    }
}
