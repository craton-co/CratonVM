import java.util.*;
public class Pq {
  public static void main(String[] a){
    PriorityQueue<Integer> pq = new PriorityQueue<>(4);
    pq.add(5); pq.add(1); pq.add(3); pq.add(9); pq.add(2);
    StringBuilder sb=new StringBuilder();
    while(!pq.isEmpty()) sb.append(pq.poll()).append(" ");
    System.out.println("poll order: "+sb.toString().trim());
    PriorityQueue<Integer> pq2 = new PriorityQueue<>(); // no-cap ctor
    pq2.add(5); pq2.add(1); pq2.add(3);
    System.out.println("nocap poll: "+pq2.poll()+" "+pq2.poll()+" "+pq2.poll());
  }
}
