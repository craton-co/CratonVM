import java.io.*;
import java.lang.annotation.Annotation;
import java.util.*;

/**
 * Interpreter round i5 wave 102, lane L3 (hunt): package and module
 * annotations through reflection.
 *
 * Package: a loader defines package `p9` and serves a hand-built
 * `p9.package-info` interface (ACC_INTERFACE|ACC_ABSTRACT|ACC_SYNTHETIC)
 * whose RuntimeVisibleAnnotations carry `@Deprecated(since="9.9",
 * forRemoval=true)` and whose RuntimeInvisibleAnnotations carry a second
 * `@Deprecated` (must not show). `Package.getAnnotations` /
 * `getDeclaredAnnotations` / `getAnnotation` / `isAnnotationPresent` /
 * `getAnnotationsByType` load `p9.package-info` through the loader
 * (`Class.forName(name, false, loader)`); a package with no package-info
 * answers empty. Also how many times the loader was asked for the
 * package-info class (HotSpot's `Package` caches the answer).
 *
 * Module: every boot-layer module that has annotations, sorted by name, with
 * them; `java.base` (none). The unnamed module's annotations.
 *
 * Run: java -cp . L3W102PackageModuleAnnotations (both modes). Expected:
 * whatever HotSpot 25 prints.
 */
public class L3W102PackageModuleAnnotations {
    /** `p9/package-info`, a class file with one visible and one invisible annotation. */
    static byte[] packageInfo() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        String[] utf8 = {
            "p9/package-info",          // 1
            null,                       // 2 Class #1
            "java/lang/Object",         // 3
            null,                       // 4 Class #3
            "RuntimeVisibleAnnotations",   // 5
            "Ljava/lang/Deprecated;",      // 6
            "since",                       // 7
            "9.9",                         // 8
            "forRemoval",                  // 9
            null,                          // 10 Integer 1
            "RuntimeInvisibleAnnotations", // 11
            "invisible",                   // 12
        };
        out.writeShort(utf8.length + 1);
        for (int i = 0; i < utf8.length; i++) {
            int index = i + 1;
            if (index == 2) { out.writeByte(7); out.writeShort(1); }
            else if (index == 4) { out.writeByte(7); out.writeShort(3); }
            else if (index == 10) { out.writeByte(3); out.writeInt(1); }
            else { out.writeByte(1); out.writeUTF(utf8[i]); }
        }
        out.writeShort(0x1600);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(0); // methods
        out.writeShort(2); // attributes
        // RuntimeVisibleAnnotations: 1 annotation, 2 pairs.
        out.writeShort(5);
        out.writeInt(2 + 2 + 2 + (2 + 1 + 2) + (2 + 1 + 2));
        out.writeShort(1);
        out.writeShort(6);
        out.writeShort(2);
        out.writeShort(7); out.writeByte('s'); out.writeShort(8);
        out.writeShort(9); out.writeByte('Z'); out.writeShort(10);
        // RuntimeInvisibleAnnotations: 1 annotation, 1 pair.
        out.writeShort(11);
        out.writeInt(2 + 2 + 2 + (2 + 1 + 2));
        out.writeShort(1);
        out.writeShort(6);
        out.writeShort(1);
        out.writeShort(7); out.writeByte('s'); out.writeShort(12);
        out.flush();
        return bytes.toByteArray();
    }

    static final class PkgLoader extends ClassLoader {
        int asked;
        final byte[] info;

        PkgLoader(byte[] info) {
            super("pkgloader", null);
            this.info = info;
            definePackage("p9", null, null, null, null, null, null, null);
            definePackage("p10", null, null, null, null, null, null, null);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.endsWith(".package-info")) asked++;
            if (name.equals("p9.package-info")) return defineClass(name, info, 0, info.length);
            throw new ClassNotFoundException(name);
        }
    }

    static String show(Annotation[] as) {
        List<String> out = new ArrayList<>();
        for (Annotation a : as) {
            if (a instanceof Deprecated d) {
                out.add("@Deprecated(since=" + d.since() + ", forRemoval=" + d.forRemoval() + ")");
            } else {
                out.add("@" + a.annotationType().getName());
            }
        }
        return out.toString();
    }

    public static void main(String[] args) throws Exception {
        PkgLoader loader = new PkgLoader(packageInfo());
        for (String name : new String[] {"p9", "p10"}) {
            Package p = loader.getDefinedPackage(name);
            System.out.println("pkg " + name + " annotations: " + show(p.getAnnotations()));
            System.out.println("pkg " + name + " declared: " + show(p.getDeclaredAnnotations()));
            Deprecated d = p.getAnnotation(Deprecated.class);
            System.out.println("pkg " + name + " getAnnotation: " + (d == null ? "null" : d.since()));
            System.out.println("pkg " + name + " present: " + p.isAnnotationPresent(Deprecated.class));
            System.out.println("pkg " + name + " byType: " + p.getAnnotationsByType(Deprecated.class).length);
        }
        System.out.println("pkg loader asked for package-info: " + loader.asked);
        Class<?> info = Class.forName("p9.package-info", false, loader);
        System.out.println("pkg info class: " + info.getName() + " simple=" + info.getSimpleName()
                + " mods=" + Integer.toHexString(info.getModifiers()) + " synthetic=" + info.isSynthetic()
                + " interface=" + info.isInterface() + " package=" + info.getPackageName());

        List<Module> mods = new ArrayList<>(ModuleLayer.boot().modules());
        mods.sort(Comparator.comparing(Module::getName));
        for (Module m : mods) {
            Annotation[] as = m.getAnnotations();
            if (as.length > 0) System.out.println("module " + m.getName() + ": " + show(as));
        }
        Module base = Object.class.getModule();
        System.out.println("module java.base: " + show(base.getAnnotations())
                + " present=" + base.isAnnotationPresent(Deprecated.class));
        Module unnamed = L3W102PackageModuleAnnotations.class.getModule();
        System.out.println("module unnamed: " + show(unnamed.getAnnotations())
                + " declared=" + show(unnamed.getDeclaredAnnotations()));
    }
}
