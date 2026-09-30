// Interpreter round i1, wave 37, lane L5 -- `Lookup.defineClass` through a
// lookup on a BOOTSTRAP class (JaCoCo's `privateLookupIn(Object.class, ...)`
// shape) defines the class in the bootstrap loader, as HotSpot's
// `JVM_LookupDefineClass` does: `getClassLoader()` is null, the bootstrap
// loader finds it by name, and it is in `java.base`.
//
// CratonVM before wave 37 (`--jdk-only`): wave 29 made the define succeed
// (`defineClass0`'s privileged bootstrap-lookup define) but mapped the null
// loader to the `0` sentinel, i.e. the APPLICATION namespace, so the class was
// the application loader's: `loader=jdk.internal.loader.ClassLoaders$AppClassLoader@<hash>`
// (the wave-29 host run of `L5W29ChildFirstAgentLoader`, `inject` row); the
// other rows are not predicted (the flat `load_class` behind
// `findBootstrapClass` / `forName(null)` can find an application-namespace
// class). Filed as
// `docs/known-issues/interpreter/i29-L5-module-opens-to-an-unnamed-module-and-java-lang-lookup-defines-20260930.md`
// item 2.
//
// `--compatible` (by design, unchanged): not predicted.
//
// Setup: the probe needs `java.lang` opened to it.
//   javac -d out L5W37BootstrapLookupDefine.java
//   cratonvm --java-home <jdk25> [--nojit] --add-opens java.base/java.lang=ALL-UNNAMED -cp out L5W37BootstrapLookupDefine
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   defined=java.lang.L5W37Injected
//   loader=null
//   module=java.base
//   package=java.lang
//   same package as Object=true
//   boot forName same=true
//   app forName same=true
//   data=ok
//   second define=java.lang.LinkageError

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;

public class L5W37BootstrapLookupDefine {
    static final String NAME = "java.lang.L5W37Injected";

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup =
                MethodHandles.privateLookupIn(Object.class, MethodHandles.lookup());
        Class<?> c = lookup.defineClass(injected());
        System.out.println("defined=" + c.getName());
        System.out.println("loader=" + c.getClassLoader());
        System.out.println("module=" + c.getModule().getName());
        System.out.println("package=" + c.getPackageName());
        System.out.println("same package as Object="
                + (c.getPackage() == Object.class.getPackage()));
        try {
            System.out.println("boot forName same=" + (Class.forName(NAME, false, null) == c));
        } catch (Throwable t) {
            System.out.println("boot forName=" + t.getClass().getName());
        }
        try {
            System.out.println("app forName same="
                    + (Class.forName(NAME, false, L5W37BootstrapLookupDefine.class.getClassLoader()) == c));
        } catch (Throwable t) {
            System.out.println("app forName=" + t.getClass().getName());
        }
        c.getField("data").set(null, "ok");
        System.out.println("data=" + c.getField("data").get(null));
        try {
            lookup.defineClass(injected());
            System.out.println("second define=ok");
        } catch (Throwable t) {
            System.out.println("second define=" + t.getClass().getName());
        }
    }

    /** `public class java.lang.L5W37Injected { public static Object data; }`, class file 49. */
    static byte[] injected() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(7);
        out.writeByte(1);
        out.writeUTF("java/lang/L5W37Injected");
        out.writeByte(7);
        out.writeShort(1);
        out.writeByte(1);
        out.writeUTF("java/lang/Object");
        out.writeByte(7);
        out.writeShort(3);
        out.writeByte(1);
        out.writeUTF("data");
        out.writeByte(1);
        out.writeUTF("Ljava/lang/Object;");
        out.writeShort(0x0021);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(1);
        out.writeShort(0x0009);
        out.writeShort(5);
        out.writeShort(6);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }
}
