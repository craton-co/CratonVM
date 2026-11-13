// Interpreter round i4, wave 98, lane L5 -- `ServiceLoader` over class-path
// providers through a `URLClassLoader`, and the unnamed-module / boot-layer
// answers next to it.
//
// The probe writes, under a fresh temp directory `d/`:
//   d/s/P1.class, d/s/P2.class  public, implement Runnable, public no-arg ctor
//   d/s/Bad.class               public, does NOT implement Runnable
//   d/META-INF/services/java.lang.Runnable:
//       "# comment\ns.P2\n  s.P1  # trailing comment\ns.P2\n"
//   d2/META-INF/services/java.lang.Runnable: "s.Bad\n"
// (class files generated with java.lang.classfile).
//
// HotSpot (`ServiceLoader.LazyClassPathLookupIterator`): providers in file
// order, comments and blanks dropped, a name repeated in the file (or across
// files) loaded once; a provider of the wrong type is
// `ServiceConfigurationError: java.lang.Runnable: Provider s.Bad not a subtype`.
//
// Run (no setup):
//   javac -d out L5W98ServiceLoaderClassPath.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W98ServiceLoaderClassPath
//
// Expected HotSpot 25 output (from the JDK sources; not measured by this lane):
// HOTSPOT_EXPECTED_BEGIN
// providers: [s.P2, s.P1]
// stream types: [s.P2, s.P1]
// provider loader is l: true
// bad: java.util.ServiceConfigurationError: java.lang.Runnable: Provider s.Bad not a subtype
// unnamed named=false name=null descriptor=null layer=null same=true
// unnamed toString prefix=true
// unnamed packages has s=true
// boot layer java.base=true parents-empty=true object-layer-boot=true
// java.base has java.lang=true unnamed-of-boot-loader=true
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.ServiceLoader;
import java.util.stream.Collectors;

public class L5W98ServiceLoaderClassPath {
    static byte[] provider(String name, boolean runnable) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(0x0021);
            cb.withSuperclass(ConstantDescs.CD_Object);
            if (runnable) {
                cb.withInterfaceSymbols(ClassDesc.of("java.lang.Runnable"));
                cb.withMethodBody("run", MethodTypeDesc.of(ConstantDescs.CD_void), 0x0001,
                        code -> code.return_());
            }
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, 0x0001, code -> code
                    .aload(0)
                    .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                    .return_());
        });
    }

    static void write(Path p, byte[] b) throws Exception {
        Files.createDirectories(p.getParent());
        Files.write(p, b);
    }

    public static void main(String[] args) throws Exception {
        Path root = Files.createTempDirectory("l5w98sl").toRealPath();
        Path d = root.resolve("d");
        Path d2 = root.resolve("d2");
        write(d.resolve("s/P1.class"), provider("s.P1", true));
        write(d.resolve("s/P2.class"), provider("s.P2", true));
        write(d.resolve("s/Bad.class"), provider("s.Bad", false));
        write(d.resolve("META-INF/services/java.lang.Runnable"),
                "# comment\ns.P2\n  s.P1  # trailing comment\ns.P2\n".getBytes(StandardCharsets.UTF_8));
        write(d2.resolve("META-INF/services/java.lang.Runnable"),
                "s.Bad\n".getBytes(StandardCharsets.UTF_8));

        try (URLClassLoader l = new URLClassLoader(new URL[] {d.toUri().toURL()}, null)) {
            List<String> names = new ArrayList<>();
            boolean sameLoader = true;
            for (Runnable r : ServiceLoader.load(Runnable.class, l)) {
                names.add(r.getClass().getName());
                sameLoader &= r.getClass().getClassLoader() == l;
            }
            System.out.println("providers: " + names);
            System.out.println("stream types: " + ServiceLoader.load(Runnable.class, l).stream()
                    .map(p -> p.type().getName()).collect(Collectors.toList()));
            System.out.println("provider loader is l: " + sameLoader);

            Module m = l.loadClass("s.P1").getModule();
            System.out.println("unnamed named=" + m.isNamed() + " name=" + m.getName()
                    + " descriptor=" + m.getDescriptor() + " layer=" + m.getLayer()
                    + " same=" + (m == l.getUnnamedModule()));
            System.out.println("unnamed toString prefix=" + m.toString().startsWith("unnamed module @"));
            System.out.println("unnamed packages has s=" + m.getPackages().contains("s"));
        }

        try (URLClassLoader l = new URLClassLoader(new URL[] {d2.toUri().toURL(), d.toUri().toURL()}, null)) {
            try {
                for (Runnable r : ServiceLoader.load(Runnable.class, l)) {
                    System.out.println("bad: loaded " + r.getClass().getName());
                    break;
                }
            } catch (Throwable t) {
                System.out.println("bad: " + t);
            }
        }

        ModuleLayer boot = ModuleLayer.boot();
        System.out.println("boot layer java.base=" + boot.findModule("java.base").isPresent()
                + " parents-empty=" + (boot.parents().size() == 1 && boot.parents().get(0) == ModuleLayer.empty())
                + " object-layer-boot=" + (Object.class.getModule().getLayer() == boot));
        System.out.println("java.base has java.lang=" + Object.class.getModule().getPackages().contains("java.lang")
                + " unnamed-of-boot-loader=" + !ClassLoader.getPlatformClassLoader().getUnnamedModule().isNamed());
    }
}
