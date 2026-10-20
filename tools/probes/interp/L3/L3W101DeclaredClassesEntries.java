import java.lang.classfile.*;
import java.lang.classfile.attribute.*;
import java.lang.constant.*;
import java.util.*;

/**
 * Interpreter round i5 wave 101, lane L3: {@code Class.getDeclaredClasses()}
 * over InnerClasses tables javac never writes (java.lang.classfile).
 * HotSpot's {@code JVM_GetDeclaredClasses} takes every entry whose
 * inner_class_info and outer_class_info are set and whose outer is this class,
 * whatever its inner_name (DC4). Its parser (with verification, every loader
 * but the boot loader) refuses inner == outer (DC1) and an exact repeat (DC3)
 * with ClassFormatError, and IGNORES the whole table when one inner class
 * appears twice (DC0) or the outer chain loops (DC5). Run both modes.
 */
public class L3W101DeclaredClassesEntries {
    static byte[] plain(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb.withFlags(ClassFile.ACC_PUBLIC));
    }

    static byte[] withInner(String name, List<InnerClassInfo> infos) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.with(InnerClassesAttribute.of(infos));
        });
    }

    static InnerClassInfo entry(String inner, String outer, String simple) {
        return entry(inner, outer, simple, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
    }

    static InnerClassInfo entry(String inner, String outer, String simple, int flags) {
        return InnerClassInfo.of(ClassDesc.of(inner), Optional.of(ClassDesc.of(outer)),
                Optional.ofNullable(simple), flags);
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        Loader() { super(L3W101DeclaredClassesEntries.class.getClassLoader()); }
        @Override protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] b = bytes.get(n);
            if (b == null) throw new ClassNotFoundException(n);
            return defineClass(n, b, 0, b.length);
        }
    }

    static void show(Loader l, String name) {
        String line;
        try {
            List<String> names = new ArrayList<>();
            for (Class<?> c : Class.forName(name, false, l).getDeclaredClasses()) names.add(c.getName());
            line = names.toString();
        } catch (Throwable t) {
            line = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + " -> " + line);
    }

    public static void main(String[] args) {
        Loader l = new Loader();
        // The same member twice with different flags (an exact 4-tuple repeat
        // is a ClassFormatError on HotSpot, row DC3), and a member entry with
        // no inner_name.
        l.bytes.put("DC0", withInner("DC0", List.of(
                entry("DC0$A", "DC0", "A"),
                entry("DC0$A", "DC0", "A", ClassFile.ACC_PUBLIC),
                entry("DC0$B", "DC0", null))));
        l.bytes.put("DC0$A", plain("DC0$A"));
        l.bytes.put("DC0$B", plain("DC0$B"));
        // An entry naming the class itself as its own member (HotSpot's parser:
        // "Class is both outer and inner class" ClassFormatError at define).
        l.bytes.put("DC1", withInner("DC1", List.of(entry("DC1", "DC1", "DC1"))));
        // A member entry whose class does not exist: the resolution error.
        l.bytes.put("DC2", withInner("DC2", List.of(entry("DC2$Gone", "DC2", "Gone"))));
        // An exact duplicate entry (HotSpot's parser: "Duplicate entry in
        // InnerClasses" ClassFormatError at define).
        l.bytes.put("DC3", withInner("DC3", List.of(entry("DC3$A", "DC3", "A"), entry("DC3$A", "DC3", "A"))));
        l.bytes.put("DC3$A", plain("DC3$A"));
        // A member entry with no inner_name, alone (listed by HotSpot).
        l.bytes.put("DC4", withInner("DC4", List.of(entry("DC4$B", "DC4", null))));
        l.bytes.put("DC4$B", plain("DC4$B"));
        // A circular outer chain: HotSpot ignores the whole table.
        l.bytes.put("DC5", withInner("DC5", List.of(
                entry("DC5$A", "DC5$B", "A"),
                entry("DC5$B", "DC5$A", "B"),
                entry("DC5$C", "DC5", "C"))));
        l.bytes.put("DC5$C", plain("DC5$C"));
        show(l, "DC0");
        show(l, "DC1");
        show(l, "DC2");
        show(l, "DC3");
        show(l, "DC4");
        show(l, "DC5");
    }
}
