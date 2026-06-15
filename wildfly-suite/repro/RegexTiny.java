public class RegexTiny {
  public static void main(String[] a) {
    String cls = "a.b";
    String s = null;
    for (int i = 0; i < 700; i++) s = cls.replaceAll("[.]", "/");
    System.out.println("res=" + s);
  }
}
