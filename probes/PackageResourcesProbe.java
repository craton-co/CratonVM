import java.io.IOException;
import java.net.URI;
import java.net.URL;
import java.nio.file.FileSystemNotFoundException;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Collections;
import java.util.List;

/**
 * Replays what Spring Boot's test-support
 * {@code org.springframework.boot.testsupport.classpath.resources.Resources#addPackage}
 * does for {@code @WithPackageResources}: enumerate the package directory on the
 * classpath, and copy each named resource out to a fresh temp root.
 *
 * Every SSL/PEM/JKS test class in the Spring Boot suite goes through this before
 * a single assertion runs, so a defect here fails the whole cluster at once.
 *
 * Run with the package's own resource root on the classpath, e.g.
 *   -cp .../core/spring-boot/build/resources/test
 * and pass the package name plus the resource names to copy.
 */
public final class PackageResourcesProbe {

	public static void main(String[] args) throws Exception {
		String packageName = (args.length > 0) ? args[0] : "org.springframework.boot.ssl.pem";
		String[] names = (args.length > 1) ? args[1].split(",") : new String[] { "test-cert.pem", "test-key.pem" };

		Path root = Files.createTempDirectory("package-resources-probe");
		System.out.println("root=" + root);

		ClassLoader loader = PackageResourcesProbe.class.getClassLoader();
		List<URL> urls = Collections.list(loader.getResources(packageName.replace(".", "/")));
		System.out.println("urls=" + urls.size());
		for (URL url : urls) {
			System.out.println("  url=" + url);
			URI uri = url.toURI();
			Path packagePath;
			try {
				packagePath = Paths.get(uri);
			}
			catch (FileSystemNotFoundException ex) {
				FileSystems.newFileSystem(uri, Collections.emptyMap());
				packagePath = Paths.get(uri);
			}
			System.out.println("  packagePath=" + packagePath);
			for (String name : names) {
				copyOne(packagePath, root, name);
			}
		}
	}

	private static void copyOne(Path packagePath, Path root, String name) {
		Path source = packagePath.resolve(name);
		boolean exists = Files.exists(source);
		boolean directory = Files.isDirectory(source);
		System.out.println("  " + name + ": exists=" + exists + " isDirectory=" + directory);
		if (!exists || directory) {
			return;
		}
		Path target = root.resolve(name);
		try {
			Path parent = target.getParent();
			if (!Files.isDirectory(parent)) {
				Files.createDirectories(parent);
			}
			Files.copy(source, target);
			System.out.println("    copied bytes=" + Files.size(target) + " readable="
					+ Files.isReadable(target) + " target=" + target);
			System.out.println("    firstLine=" + Files.readAllLines(target).get(0));
		}
		catch (IOException ex) {
			System.out.println("    FAILED " + ex.getClass().getName() + ": " + ex.getMessage());
		}
	}

}
