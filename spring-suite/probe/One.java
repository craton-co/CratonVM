public class One {
  public static void main(String[] a){
    int M = Integer.MAX_VALUE;
    try {
      switch(a[0]){
        case "hashmap": new java.util.HashMap<>(M); break;
        case "hashset": new java.util.HashSet<>(M); break;
        case "lhm": new java.util.LinkedHashMap<>(M); break;
        case "deque": new java.util.ArrayDeque<>(M); break;
        case "pq": new java.util.PriorityQueue<>(M); break;
        case "vector": new java.util.Vector<>(M); break;
        case "sb": new StringBuilder(M); break;
      }
      System.out.println(a[0]+": NO THROW");
    } catch (Throwable e){ System.out.println(a[0]+": "+e.getClass().getSimpleName()+": "+e.getMessage()); }
    System.out.println("END");
  }
}
