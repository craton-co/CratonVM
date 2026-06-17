public class Fam {
  static void t(String n, Runnable r){ try { r.run(); System.out.println(n+": NO THROW"); } catch (Throwable e){ System.out.println(n+": "+e.getClass().getSimpleName()+": "+e.getMessage()); } }
  public static void main(String[] a){
    int M = Integer.MAX_VALUE;
    t("HashMap(int)", () -> { new java.util.HashMap<>(M); });
    t("HashSet(int)", () -> { new java.util.HashSet<>(M); });
    t("LinkedHashMap(int)", () -> { new java.util.LinkedHashMap<>(M); });
    t("ArrayDeque(int)", () -> { new java.util.ArrayDeque<>(M); });
    t("PriorityQueue(int)", () -> { new java.util.PriorityQueue<>(M); });
    t("Vector(int)", () -> { new java.util.Vector<>(M); });
    t("StringBuilder(int)", () -> { new StringBuilder(M); });
    System.out.println("DONE-FAM");
  }
}
