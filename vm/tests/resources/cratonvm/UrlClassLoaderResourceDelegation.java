package cratonvm;

import java.net.URL;
import java.net.URLClassLoader;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Regression for URLClassLoader's parent-first singular resource lookup.
 *
 * A child with no local URLs must still find a resource supplied solely by a
 * URLClassLoader parent. Spring Boot uses this shape when a test-specific
 * resource class loader is wrapped by HideDataScriptClassLoader.
 */
public final class UrlClassLoaderResourceDelegation {

    public static void main(String[] args) throws Exception {
        Path root = Files.createTempDirectory("cratonvm-ucl-parent");
        Path resource = root.resolve("parent-only.txt");
        Files.writeString(resource, "parent-resource", StandardCharsets.UTF_8);
        try (URLClassLoader parent = new URLClassLoader(new URL[] { root.toUri().toURL() },
                UrlClassLoaderResourceDelegation.class.getClassLoader());
                URLClassLoader child = new URLClassLoader(new URL[0], parent)) {
            URL found = child.getResource("parent-only.txt");
            if (found == null || !"parent-resource".equals(Files.readString(Path.of(found.toURI())))) {
                throw new AssertionError("child did not return the parent resource: " + found);
            }
            try (InputStream stream = child.getResourceAsStream("parent-only.txt")) {
                String content = (stream != null) ? new String(stream.readAllBytes(), StandardCharsets.UTF_8) : null;
                if (!"parent-resource".equals(content)) {
                    throw new AssertionError("child stream did not return the parent resource: " + content);
                }
            }
        }
        finally {
            Files.deleteIfExists(resource);
            Files.deleteIfExists(root);
        }
        System.out.println("URL_CLASSLOADER_PARENT_RESOURCE_OK");
    }

}
