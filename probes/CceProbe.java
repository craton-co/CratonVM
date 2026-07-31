package org.springframework.context.annotation;

import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.security.ProtectionDomain;
import java.security.SecureClassLoader;

import org.springframework.core.OverridingClassLoader;
import org.springframework.core.SmartClassLoader;
import org.springframework.util.StreamUtils;

/**
 * Prints, for each of the four class loaders the ConfigurationClassEnhancerTests
 * uses, which loader the enhanced class actually ends up in. Ground truth for
 * the CratonVM native enhancer.
 */
public class CceProbe {

	public static void main(String[] args) throws Exception {
		System.out.println("PROBE smartOverriding=" + (new OverridingClassLoader(CceProbe.class.getClassLoader()) instanceof SmartClassLoader));
		run("public", ConfigurationClassEnhancerTests.MyConfigWithPublicClass.class);
		run("nonPublicClass", ConfigurationClassEnhancerTests.MyConfigWithNonPublicClass.class);
		run("nonPublicCtor", ConfigurationClassEnhancerTests.MyConfigWithNonPublicConstructor.class);
		run("nonPublicMethod", ConfigurationClassEnhancerTests.MyConfigWithNonPublicMethod.class);
	}

	private static void run(String label, Class<?> config) {
		ClassLoader app = CceProbe.class.getClassLoader();
		one(label, "URLClassLoader", config, new URLClassLoader(new URL[0], app), app);
		one(label, "OverridingClassLoader", config, new OverridingClassLoader(app), app);
		one(label, "CustomSmartClassLoader", config, new CustomSmartClassLoader(app), app);
		one(label, "BasicSmartClassLoader", config, new BasicSmartClassLoader(app), app);
	}

	private static void one(String label, String loaderName, Class<?> config, ClassLoader cl, ClassLoader app) {
		try {
			ConfigurationClassEnhancer enhancer = new ConfigurationClassEnhancer();
			Class<?> enhanced = enhancer.enhance(config, cl);
			String where = (enhanced.getClassLoader() == cl ? "SELF"
					: enhanced.getClassLoader() == cl.getParent() ? "PARENT"
					: String.valueOf(enhanced.getClassLoader()));
			String own;
			try { own = String.valueOf(cl.loadClass(config.getName()) != config); } catch (Throwable t) { own = "ERR"; }
			System.out.println("PROBE " + label + " " + loaderName + " ownCopy=" + own + " -> " + where
					+ " name=" + enhanced.getName()
					+ " assignable=" + config.isAssignableFrom(enhanced));
		}
		catch (Throwable ex) {
			System.out.println("PROBE " + label + " " + loaderName + " -> THREW " + ex);
		}
	}

	static class CustomSmartClassLoader extends SecureClassLoader implements SmartClassLoader {

		CustomSmartClassLoader(ClassLoader parent) {
			super(parent);
		}

		@Override
		protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
			if (name.contains("MyConfig")) {
				String path = name.replace('.', '/').concat(".class");
				try (InputStream in = super.getResourceAsStream(path)) {
					byte[] bytes = StreamUtils.copyToByteArray(in);
					if (bytes.length > 0) {
						return defineClass(name, bytes, 0, bytes.length);
					}
				}
				catch (IOException ex) {
					throw new IllegalStateException(ex);
				}
			}
			return super.loadClass(name, resolve);
		}

		@Override
		public boolean isClassReloadable(Class<?> clazz) {
			return clazz.getName().contains("MyConfig");
		}

		@Override
		public ClassLoader getOriginalClassLoader() {
			return getParent();
		}

		@Override
		public Class<?> publicDefineClass(String name, byte[] b, ProtectionDomain protectionDomain) {
			return defineClass(name, b, 0, b.length, protectionDomain);
		}
	}

	static class BasicSmartClassLoader extends SecureClassLoader implements SmartClassLoader {

		BasicSmartClassLoader(ClassLoader parent) {
			super(parent);
		}

		@Override
		public Class<?> publicDefineClass(String name, byte[] b, ProtectionDomain protectionDomain) {
			return defineClass(name, b, 0, b.length, protectionDomain);
		}
	}
}
