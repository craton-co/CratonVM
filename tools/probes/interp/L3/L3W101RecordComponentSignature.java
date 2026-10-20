import java.lang.classfile.*;
import java.lang.classfile.attribute.*;
import java.lang.constant.*;
import java.lang.reflect.*;
import java.util.*;

/**
 * Interpreter round i5 wave 101, lane L3 (page
 * i101-L3-record-component-signature-read-from-the-field): HotSpot fills
 * {@code RecordComponent.signature} from the COMPONENT's own Signature
 * attribute (inside the Record attribute); CratonVM reads the same-named
 * field's. javac writes both identically, so only a generated class tells them
 * apart. Run both modes; every line must match HotSpot.
 */
public class L3W101RecordComponentSignature {
    static final ClassDesc LIST = ClassDesc.of("java.util.List");

    static byte[] gen(String name, String componentSig, String fieldSig) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withSuperclass(ClassDesc.of("java.lang.Record"));
            List<Attribute<?>> attrs = new ArrayList<>();
            if (componentSig != null) attrs.add(SignatureAttribute.of(Signature.parseFrom(componentSig)));
            cb.with(RecordAttribute.of(RecordComponentInfo.of("x", LIST, attrs)));
            cb.withField("x", LIST, fb -> {
                fb.withFlags(ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
                if (fieldSig != null) fb.with(SignatureAttribute.of(Signature.parseFrom(fieldSig)));
            });
        });
    }

    static final class Loader extends ClassLoader {
        Loader() { super(L3W101RecordComponentSignature.class.getClassLoader()); }
        Class<?> define(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
    }

    static void show(String label, String name, byte[] bytes) {
        String line;
        try {
            Class<?> c = new Loader().define(name, bytes);
            RecordComponent rc = c.getRecordComponents()[0];
            line = c.isRecord() + " sig=" + rc.getGenericSignature() + " type=" + rc.getGenericType().getTypeName();
        } catch (Throwable t) {
            line = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + " -> " + line);
    }

    public static void main(String[] args) {
        String ls = "Ljava/util/List<Ljava/lang/String;>;";
        String li = "Ljava/util/List<Ljava/lang/Integer;>;";
        show("both", "RS0", gen("RS0", ls, ls));
        show("component-only", "RS1", gen("RS1", ls, null));
        show("field-only", "RS2", gen("RS2", null, ls));
        show("differ", "RS3", gen("RS3", ls, li));
    }
}
