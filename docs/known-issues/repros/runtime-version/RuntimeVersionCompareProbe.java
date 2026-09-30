public class RuntimeVersionCompareProbe {
  static void eq(String a, String b) {
    Runtime.Version x = Runtime.Version.parse(a), y = Runtime.Version.parse(b);
    System.out.println("eq("+a+","+b+")="+x.equals(y)+" cmp="+Integer.signum(x.compareTo(y))
      +" hashEq="+(x.hashCode()==y.hashCode())+" eqIgnOpt="+x.equalsIgnoreOptional(y));
  }
  public static void main(String[] a) {
    eq("25.0.1+9","25.0.1+9");
    eq("25.0.1+9","25.0.1+10");
    eq("25.0.1+9","25.0.1");
    eq("17.0.2-ea+7-abc","17.0.2-ea+7-abc");
    eq("8","8");
    Runtime.Version v = Runtime.version();
    System.out.println("version-vs-parse(runtime.version) equals="
      + v.equals(Runtime.Version.parse(System.getProperty("java.runtime.version"))));
    System.out.println("toString==runtime.version prop: " + v.toString().equals(System.getProperty("java.runtime.version")));
  }
}
