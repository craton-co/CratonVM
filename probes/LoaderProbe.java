public class LoaderProbe {
  static final String N = "org.springframework.batch.support.SerializationUtilsTests$Foo";
  static void t(String label, ClassLoader cl) {
    try { Class<?> c = Class.forName(N, false, cl); System.out.println(label + " -> " + c + "  loader=" + c.getClassLoader() + " synthetic=" + c.isSynthetic()); }
    catch (Throwable e) { System.out.println(label + " -> threw " + e.getClass().getSimpleName()); }
  }
  public static void main(String[] a) throws Exception {
    t("null (bootstrap)      ", null);
    t("app                   ", LoaderProbe.class.getClassLoader());
    t("platform              ", ClassLoader.getPlatformClassLoader());
    t("systemClassLoader     ", ClassLoader.getSystemClassLoader());
    t("TCCL                  ", Thread.currentThread().getContextClassLoader());
    try {
      Class<?> vm = Class.forName("jdk.internal.misc.VM");
      var m = vm.getDeclaredMethod("latestUserDefinedLoader");
      m.setAccessible(true);
      Object l = m.invoke(null);
      System.out.println("latestUserDefinedLoader = " + l);
      t("latestUserDefinedLoader", (ClassLoader) l);
    } catch (Throwable e) { System.out.println("VM.latestUserDefinedLoader: " + e); }
  }
}
