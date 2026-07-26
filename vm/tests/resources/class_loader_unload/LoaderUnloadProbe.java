import java.io.ObjectStreamClass;
import java.lang.management.ClassLoadingMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

public final class LoaderUnloadProbe {
    private static final class BytesLoader extends ClassLoader {
        private final byte[] bytes;

        BytesLoader(byte[] bytes) {
            super(null);
            this.bytes = bytes;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals("unloadprobe.LoaderUnloadPayload")) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    private static long exercise(
            byte[] bytes,
            int round,
            List<WeakReference<ClassLoader>> loaders,
            List<WeakReference<Class<?>>> classes) throws Exception {
        BytesLoader loader = new BytesLoader(bytes);
        Class<?> type = Class.forName("unloadprobe.LoaderUnloadPayload", true, loader);
        Object instance = type.getConstructor().newInstance();
        Method hot = type.getMethod("hot", int.class);
        Method staticHot = type.getMethod("staticHot", int.class);
        long checksum = 0;
        for (int i = 0; i < 700; i++) {
            checksum += (Integer) hot.invoke(instance, 20);
        }
        checksum += (Integer) staticHot.invoke(null, round);
        if (ObjectStreamClass.lookup(type) == null) {
            throw new AssertionError("missing serialization descriptor");
        }
        loaders.add(new WeakReference<>(loader));
        classes.add(new WeakReference<>(type));
        return checksum;
    }

    public static void main(String[] args) throws Exception {
        byte[] bytes = Files.readAllBytes(Path.of(args[0]));
        int rounds = Integer.parseInt(args.length > 1 ? args[1] : "12");
        ClassLoadingMXBean bean = ManagementFactory.getClassLoadingMXBean();
        List<WeakReference<ClassLoader>> loaders = new ArrayList<>();
        List<WeakReference<Class<?>>> classes = new ArrayList<>();

        // Warm up one-time management, reflection, and serialization classes
        // before measuring the bounded live-class count.
        long checksum = exercise(bytes, -1, loaders, classes);
        for (int attempt = 0; attempt < 30; attempt++) {
            System.gc();
            Thread.sleep(2);
        }
        loaders.clear();
        classes.clear();
        long unloadedBefore = bean.getUnloadedClassCount();
        int loadedBefore = bean.getLoadedClassCount();

        for (int round = 0; round < rounds; round++) {
            checksum += exercise(bytes, round, loaders, classes);
            for (int attempt = 0; attempt < 12; attempt++) {
                System.gc();
                byte[][] pressure = new byte[8][];
                for (int i = 0; i < pressure.length; i++) {
                    pressure[i] = new byte[128 * 1024];
                }
                Thread.sleep(2);
            }
        }

        for (int attempt = 0; attempt < 80; attempt++) {
            System.gc();
            byte[][] pressure = new byte[16][];
            for (int i = 0; i < pressure.length; i++) {
                pressure[i] = new byte[128 * 1024];
            }
            Thread.sleep(2);
        }

        long liveLoaders = loaders.stream().filter(ref -> ref.get() != null).count();
        long liveClasses = classes.stream().filter(ref -> ref.get() != null).count();
        long unloadedDelta = bean.getUnloadedClassCount() - unloadedBefore;
        int loadedAfter = bean.getLoadedClassCount();
        boolean ok = liveLoaders == 0
                && liveClasses == 0
                && unloadedDelta >= rounds
                && loadedAfter <= loadedBefore + 16;
        System.out.println("UNLOAD rounds=" + rounds
                + " liveLoaders=" + liveLoaders
                + " liveClasses=" + liveClasses
                + " unloadedDelta=" + unloadedDelta
                + " loadedBefore=" + loadedBefore
                + " loadedAfter=" + loadedAfter
                + " checksum=" + checksum
                + " ok=" + ok);
        if (!ok) {
            throw new AssertionError("class-loader unloading failed");
        }
    }
}
