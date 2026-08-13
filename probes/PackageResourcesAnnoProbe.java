import java.lang.annotation.Annotation;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Optional;

import org.junit.platform.commons.support.AnnotationSupport;

/**
 * Replays {@code ResourcesExtension#packageResourcesOf}: the method-level
 * {@code @WithPackageResources} plus the class-level one, in that order. When
 * the two lookups answer with the same annotation, the extension copies the same
 * package resource into the temp root twice and the second copy fails with
 * {@code FileAlreadyExistsException}.
 *
 * Pass the test class name; every declared method is reported.
 */
public final class PackageResourcesAnnoProbe {

	@SuppressWarnings("unchecked")
	public static void main(String[] args) throws Exception {
		String className = (args.length > 0) ? args[0] : "org.springframework.boot.ssl.pem.PemContentTests";
		Class<?> testClass = Class.forName(className);
		Class<? extends Annotation> withPackageResources = (Class<? extends Annotation>) Class
				.forName("org.springframework.boot.testsupport.classpath.resources.WithPackageResources");

		System.out.println("class=" + testClass.getName());
		System.out.println("  class getAnnotations()=" + describe(testClass.getAnnotations()));
		System.out.println("  class getDeclaredAnnotations()=" + describe(testClass.getDeclaredAnnotations()));
		Optional<? extends Annotation> onClass = AnnotationSupport.findAnnotation(testClass, withPackageResources,
				List.of());
		System.out.println("  AnnotationSupport.findAnnotation(class)=" + onClass.map(Object::toString).orElse("<empty>"));

		for (Method method : testClass.getDeclaredMethods()) {
			if (method.isSynthetic()) {
				continue;
			}
			Optional<? extends Annotation> onMethod = AnnotationSupport.findAnnotation(method, withPackageResources);
			if (onMethod.isEmpty() && onClass.isEmpty()) {
				continue;
			}
			List<Annotation> collected = new ArrayList<>();
			onMethod.ifPresent(collected::add);
			onClass.ifPresent(collected::add);
			System.out.println("  method=" + method.getName() + " declared=" + describe(method.getDeclaredAnnotations())
					+ " collected=" + collected.size() + " -> " + collected);
		}
	}

	private static String describe(Annotation[] annotations) {
		List<String> names = new ArrayList<>();
		for (Annotation annotation : annotations) {
			names.add(annotation.annotationType().getSimpleName());
		}
		return names.toString() + "(len=" + annotations.length + ")";
	}

}
