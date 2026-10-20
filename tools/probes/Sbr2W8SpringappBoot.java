import java.beans.PropertyDescriptor;
import java.net.URL;
import java.util.*;

import org.springframework.beans.BeanUtils;
import org.springframework.beans.BeanWrapper;
import org.springframework.beans.BeanWrapperImpl;
import org.springframework.boot.Banner;
import org.springframework.boot.WebApplicationType;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.util.ClassUtils;
import org.springframework.util.ResourceUtils;

/**
 * SBR2 W8 SPRINGAPP: the Spring-class rows (ClassUtils, ResourceUtils, GenericTypeAwarePropertyDescriptor,
 * SpringApplicationBannerPrinter) over the real framework jars in apps/spring-boot/cratonvm-suite/lib.
 * Run: -cp "<probe classes>;<lib>/*" (banner.txt is looked up on the class path / working directory).
 */
public class Sbr2W8SpringappBoot {
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "EX " + ex(e)); } }

    public static class Gen<X extends Number> {
        private X value; private List<String> names; private boolean flag;
        public X getValue() { return value; }
        public void setValue(X v) { value = v; }
        public List<String> getNames() { return names; }
        public void setNames(List<String> n) { names = n; }
        public boolean isFlag() { return flag; }
        public void setFlag(boolean f) { flag = f; }
        public String getReadOnly() { return "ro"; }
        public void setWriteOnly(String s) {}
    }
    public static class IntGen extends Gen<Integer> {}
    public static class Holder { public Holder() {} }

    @Configuration
    public static class Cfg {
        @Bean public Holder holder() { return new Holder(); }
        @Bean public IntGen intGen() { IntGen g = new IntGen(); g.setValue(3); return g; }
    }

    public static void main(String[] a) throws Throwable {
        ClassLoader cl = Sbr2W8SpringappBoot.class.getClassLoader();
        // ClassUtils.forName / isPresent
        for (String n : new String[] { "java.lang.String", "int", "int[]", "java.lang.String[]", "[Ljava.lang.String;", "[[I", "java.util.Map.Entry", "java.util.Map$Entry",
                "no.such.Clazz", "java.util.concurrent.ConcurrentHashMap.KeySetView", "Sbr2W8SpringappBoot.Gen", "Sbr2W8SpringappBoot$Gen", "Sbr2W8SpringappBoot.Gen[]",
                "jakarta.faces.context.FacesContext", "org.springframework.boot.SpringApplication", "org.springframework.util.ClassUtils", "long", "boolean[][]", "void", "", "a.b.C.D" }) {
            t("forName[" + n + "]", () -> ClassUtils.forName(n, cl).getName());
            t("forName null loader[" + n + "]", () -> ClassUtils.forName(n, null).getName());
            t("isPresent[" + n + "]", () -> ClassUtils.isPresent(n, cl));
        }
        t("forName null", () -> ClassUtils.forName(null, cl));
        t("isPresent null loader", () -> ClassUtils.isPresent("java.lang.String", null));
        t("forName loader identity", () -> ClassUtils.forName("Sbr2W8SpringappBoot", cl) == Sbr2W8SpringappBoot.class);
        t("resolveClassName", () -> ClassUtils.resolveClassName("java.util.List", cl).getName());
        t("resolveClassName missing", () -> ClassUtils.resolveClassName("x.Y", cl));
        t("forName inner", () -> ClassUtils.forName("java.util.Map.Entry", cl).getName());
        t("getShortName", () -> ClassUtils.getShortName("java.util.Map$Entry"));

        // ResourceUtils
        t("isFileURL file", () -> ResourceUtils.isFileURL(new URL("file:/tmp/x")));
        t("isFileURL vfs", () -> ResourceUtils.isFileURL(new URL("file:/a/b.jar")));
        t("isFileURL jar", () -> ResourceUtils.isFileURL(new URL("jar:file:/a/b.jar!/x")));
        t("isFileURL http", () -> ResourceUtils.isFileURL(new URL("http://h/x")));
        t("isFileURL null", () -> ResourceUtils.isFileURL(null));
        t("isJarURL jar", () -> ResourceUtils.isJarURL(new URL("jar:file:/a/b.jar!/x")));
        t("isJarURL file", () -> ResourceUtils.isJarURL(new URL("file:/a/b.jar")));
        t("isJarURL http", () -> ResourceUtils.isJarURL(new URL("http://h/x")));
        t("isJarURL null", () -> ResourceUtils.isJarURL(null));
        t("isJarFileURL", () -> ResourceUtils.isJarFileURL(new URL("file:/a/b.jar")) + "," + ResourceUtils.isJarFileURL(new URL("file:/a/b.txt")));
        t("isUrl", () -> ResourceUtils.isUrl("classpath:x") + "," + ResourceUtils.isUrl("file:/x") + "," + ResourceUtils.isUrl("nope"));
        t("useCaches file", () -> { var c = new URL("file:/tmp/x").openConnection(); ResourceUtils.useCachesIfNecessary(c); return c.getUseCaches(); });
        t("useCaches jar", () -> { var c = new URL("jar:file:/tmp/x.jar!/q").openConnection(); ResourceUtils.useCachesIfNecessary(c); return c.getUseCaches(); });
        t("useCaches http", () -> { var c = new URL("http://localhost:1/x").openConnection(); ResourceUtils.useCachesIfNecessary(c); return c.getUseCaches(); });
        t("useCaches null", () -> { ResourceUtils.useCachesIfNecessary(null); return "ok"; });
        t("getURL classpath", () -> ResourceUtils.getURL("classpath:java/lang/Object.class").getProtocol());
        t("getURL classpath own", () -> String.valueOf(ResourceUtils.getURL("classpath:Sbr2W8SpringappBoot.class").getProtocol()));
        t("getURL missing", () -> ResourceUtils.getURL("classpath:no/such/file.txt"));
        t("getFile", () -> ResourceUtils.getFile("file:/tmp/zzz").getName());

        // GenericTypeAwarePropertyDescriptor via BeanUtils / BeanWrapper
        t("pds IntGen", () -> { PropertyDescriptor[] pds = BeanUtils.getPropertyDescriptors(IntGen.class); StringBuilder sb = new StringBuilder(); Arrays.sort(pds, Comparator.comparing(PropertyDescriptor::getName)); for (PropertyDescriptor p : pds) sb.append(p.getName()).append(':').append(p.getPropertyType() == null ? null : p.getPropertyType().getSimpleName()).append(':').append(p.getReadMethod() != null).append(':').append(p.getWriteMethod() != null).append(' '); return sb; });
        t("pds Gen", () -> { PropertyDescriptor[] pds = BeanUtils.getPropertyDescriptors(Gen.class); StringBuilder sb = new StringBuilder(); Arrays.sort(pds, Comparator.comparing(PropertyDescriptor::getName)); for (PropertyDescriptor p : pds) sb.append(p.getName()).append(':').append(p.getPropertyType() == null ? null : p.getPropertyType().getSimpleName()).append(':').append(p.getReadMethod() != null ? p.getReadMethod().getName() : null).append(':').append(p.getWriteMethod() != null ? p.getWriteMethod().getName() : null).append(' '); return sb; });
        t("pd class", () -> BeanUtils.getPropertyDescriptor(IntGen.class, "value").getClass().getName());
        t("pd value type", () -> BeanUtils.getPropertyDescriptor(IntGen.class, "value").getPropertyType().getName());
        t("pd write", () -> BeanUtils.getPropertyDescriptor(IntGen.class, "value").getWriteMethod().getParameterTypes()[0].getName());
        t("bw set", () -> { BeanWrapper bw = new BeanWrapperImpl(new IntGen()); bw.setPropertyValue("value", "42"); bw.setPropertyValue("flag", "true"); bw.setPropertyValue("names", List.of("x")); return bw.getPropertyValue("value") + "," + bw.getPropertyValue("flag") + "," + bw.getPropertyValue("names"); });
        t("bw writable", () -> { BeanWrapper bw = new BeanWrapperImpl(new IntGen()); return bw.isWritableProperty("value") + "," + bw.isReadableProperty("value") + "," + bw.isWritableProperty("readOnly") + "," + bw.isReadableProperty("writeOnly") + "," + bw.isWritableProperty("nosuch"); });
        t("bw type", () -> new BeanWrapperImpl(new IntGen()).getPropertyType("value").getName());
        t("bw bad", () -> { new BeanWrapperImpl(new IntGen()).setPropertyValue("readOnly", "x"); return "ok"; });
        t("bw convert", () -> { new BeanWrapperImpl(new IntGen()).setPropertyValue("value", "notanumber"); return "ok"; });
        t("bw typedesc", () -> new BeanWrapperImpl(new IntGen()).getPropertyTypeDescriptor("value").getType().getName());
        t("bw nested", () -> { BeanWrapper bw = new BeanWrapperImpl(new IntGen()); bw.setPropertyValue("names", new ArrayList<>(List.of("a"))); return bw.getPropertyValue("names[0]"); });

        // Spring application: banner, factories, contexts
        t("spring run", () -> {
            try (ConfigurableApplicationContext ctx = new SpringApplicationBuilder(Cfg.class).web(WebApplicationType.NONE).bannerMode(Banner.Mode.CONSOLE).logStartupInfo(false).run()) {
                return ctx.getBeanFactory().getBeanDefinitionNames().length > 3 && ctx.getBean(IntGen.class).getValue() == 3;
            }
        });
        t("spring run banner off", () -> {
            try (ConfigurableApplicationContext ctx = new SpringApplicationBuilder(Cfg.class).web(WebApplicationType.NONE).bannerMode(Banner.Mode.OFF).logStartupInfo(false).run()) {
                return ctx.containsBean("holder");
            }
        });
        t("spring run custom banner", () -> {
            try (ConfigurableApplicationContext ctx = new SpringApplicationBuilder(Cfg.class).web(WebApplicationType.NONE).bannerMode(Banner.Mode.CONSOLE).logStartupInfo(false)
                    .properties("spring.banner.location=classpath:sbr2w8-banner.txt").run()) {
                return ctx.containsBean("holder");
            }
        });
        t("spring run missing banner", () -> {
            try (ConfigurableApplicationContext ctx = new SpringApplicationBuilder(Cfg.class).web(WebApplicationType.NONE).bannerMode(Banner.Mode.CONSOLE).logStartupInfo(false)
                    .properties("spring.banner.location=classpath:no-such-banner.txt").run()) {
                return ctx.containsBean("holder");
            }
        });
        t("spring run banner charset", () -> {
            try (ConfigurableApplicationContext ctx = new SpringApplicationBuilder(Cfg.class).web(WebApplicationType.NONE).bannerMode(Banner.Mode.CONSOLE).logStartupInfo(false)
                    .properties("spring.banner.location=classpath:sbr2w8-banner.txt", "spring.banner.charset=UTF-8", "app.title=TitleX").run()) {
                return ctx.containsBean("holder");
            }
        });
    }
}
