import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Random;

import org.springframework.boot.loader.zip.ZipContent;

public class RunVirtualZip {
    public static void main(String[] args) throws Exception {
        Class<?> testClass = Class.forName("org.springframework.boot.loader.zip.VirtualZipPerformanceTests");
        var constructor = testClass.getDeclaredConstructor();
        constructor.setAccessible(true);
        Object test = constructor.newInstance();
        Field temp = testClass.getDeclaredField("temp");
        temp.setAccessible(true);
        Path dir = Files.createTempDirectory("virtual-zip-bench-");
        try {
            temp.set(test, dir);
            Method sequential = testClass.getDeclaredMethod("sequentialReadPerformance");
            sequential.setAccessible(true);
            sequential.invoke(test);
            long started = System.nanoTime();
            int count = 0;
            try (ZipContent zip = ZipContent.open(dir.resolve("test.zip"), "test/")) {
                Random random = new Random(1);
                for (int i = 0; i < 100_000; i++) {
                    if (zip.getEntry(random.nextInt(50) + ".dat") == null) throw new AssertionError("missing entry");
                    count++;
                }
            }
            System.out.println("RANDOM_MS=" + (System.nanoTime() - started) / 1_000_000);
            System.out.println("CHECKSUM=" + count);
            System.out.println("VIRTUAL_ZIP_OK");
        } finally {
            try (var files = Files.list(dir)) {
                for (Path file : files.toList()) Files.delete(file);
            }
            Files.delete(dir);
        }
    }
}
