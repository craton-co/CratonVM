import java.lang.module.Configuration;
import java.lang.module.ModuleDescriptor;
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReader;
import java.lang.module.ModuleReference;
import java.lang.ref.WeakReference;
import java.net.URI;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.Set;
import java.util.function.BooleanSupplier;
import java.util.stream.Stream;

/**
 * gc-common w17-d: a dropped {@code ModuleLayer} whose module {@code provides}
 * a service must unload its loader, as on HotSpot.
 *
 * <p>{@code Module.defineModules} registers such a module in
 * {@code ServicesCatalog.getServicesCatalog(loader)}, which is
 * {@code AbstractClassLoaderValue.putIfAbsent(loader, catalog)}. CratonVM
 * serves that natively from a side table. Until w17-d the table's value was a
 * permanent root, and {@code catalog -> ServiceProvider -> Module -> loader}
 * kept the loader alive for good
 * ({@code common-w16b-clv-values-are-permanent-roots-that-pin-their-loader}).
 *
 * <p>The control layers are identical but provide nothing, so no catalog is
 * made. If the control layers stay alive too, something other than the
 * catalog pins layers, and the {@code services} line says nothing about w17-d.
 *
 * <p>The modules have a package and a provider class name but no class bytes.
 * Nothing loads the provider: neither resolution nor layer definition needs
 * it. The probe ends on its own (every loop is bounded) and prints one line
 * per kind plus a {@code RESULT} line.
 */
public final class ClvLayerUnloadProbe {
    private static final int LAYERS = 4;
    private static final int GC_CAP = 60;

    private static ModuleFinder finderFor(ModuleDescriptor descriptor) {
        ModuleReference ref =
                new ModuleReference(descriptor, URI.create("clvprobe:///" + descriptor.name())) {
                    @Override
                    public ModuleReader open() {
                        return new ModuleReader() {
                            @Override
                            public Optional<URI> find(String name) {
                                return Optional.empty();
                            }

                            @Override
                            public Stream<String> list() {
                                return Stream.empty();
                            }

                            @Override
                            public void close() {}
                        };
                    }
                };
        return new ModuleFinder() {
            @Override
            public Optional<ModuleReference> find(String name) {
                return name.equals(descriptor.name()) ? Optional.of(ref) : Optional.empty();
            }

            @Override
            public Set<ModuleReference> findAll() {
                return Set.of(ref);
            }
        };
    }

    /** Define one single-module layer and answer a weak reference to its loader. */
    private static WeakReference<ClassLoader> defineLayer(String name, boolean provides) {
        String pkg = name + ".impl";
        ModuleDescriptor.Builder builder = ModuleDescriptor.newModule(name).packages(Set.of(pkg));
        if (provides) {
            builder.provides("java.lang.Runnable", List.of(pkg + ".Provider"));
        }
        ModuleDescriptor descriptor = builder.build();
        ModuleLayer boot = ModuleLayer.boot();
        Configuration cf =
                boot.configuration().resolve(finderFor(descriptor), ModuleFinder.of(), Set.of(name));
        ModuleLayer layer = boot.defineModulesWithOneLoader(cf, ClassLoader.getSystemClassLoader());
        ClassLoader loader = layer.findLoader(name);
        if (loader == null) {
            throw new AssertionError("layer " + name + " has no loader");
        }
        return new WeakReference<>(loader);
    }

    private static long live(List<WeakReference<ClassLoader>> refs) {
        return refs.stream().filter(r -> r.get() != null).count();
    }

    private static int gcAttempts = 0;

    private static void gcUntil(BooleanSupplier done) throws InterruptedException {
        for (int attempt = 0; attempt < GC_CAP && !done.getAsBoolean(); attempt++) {
            System.gc();
            gcAttempts++;
            byte[][] pressure = new byte[16][];
            for (int i = 0; i < pressure.length; i++) {
                pressure[i] = new byte[128 * 1024];
            }
            Thread.sleep(2);
        }
    }

    public static void main(String[] args) throws Exception {
        List<WeakReference<ClassLoader>> control = new ArrayList<>();
        List<WeakReference<ClassLoader>> services = new ArrayList<>();
        for (int i = 0; i < LAYERS; i++) {
            control.add(defineLayer("clvprobe.control" + i, false));
            services.add(defineLayer("clvprobe.services" + i, true));
        }
        gcUntil(() -> live(control) == 0 && live(services) == 0);
        long liveControl = live(control);
        long liveServices = live(services);
        System.out.println("control.live=" + liveControl + "/" + LAYERS);
        System.out.println("services.live=" + liveServices + "/" + LAYERS);
        System.out.println("gcAttempts=" + gcAttempts);
        if (liveControl == 0 && liveServices == 0) {
            System.out.println("RESULT ok");
        } else if (liveControl != 0) {
            System.out.println("RESULT FAIL control layers pinned too: not the ServicesCatalog row");
        } else {
            System.out.println("RESULT FAIL service-providing layers pinned (ServicesCatalog row)");
        }
    }
}
