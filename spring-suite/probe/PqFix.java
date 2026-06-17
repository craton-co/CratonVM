import java.util.*;
public class PqFix {
  public static void main(String[] a){
    // natural order (Integer)
    PriorityQueue<Integer> pq = new PriorityQueue<>();
    int[] in = {5,1,3,9,2,7,0,8,4,6};
    for(int x: in) pq.add(x);
    StringBuilder sb=new StringBuilder(); Integer prev=null; boolean sorted=true;
    while(!pq.isEmpty()){ int v=pq.poll(); if(prev!=null && v<prev) sorted=false; prev=v; sb.append(v).append(" "); }
    System.out.println("int natural: "+sb.toString().trim()+"  sorted="+sorted);
    // peek returns min
    PriorityQueue<Integer> pk = new PriorityQueue<>(); pk.add(8); pk.add(3); pk.add(5);
    System.out.println("peek min: "+pk.peek());
    // strings natural
    PriorityQueue<String> ps = new PriorityQueue<>();
    for(String s: new String[]{"pear","apple","mango","fig","banana"}) ps.add(s);
    StringBuilder ss=new StringBuilder(); while(!ps.isEmpty()) ss.append(ps.poll()).append(" ");
    System.out.println("string natural: "+ss.toString().trim());
    // reverse comparator
    PriorityQueue<Integer> pr = new PriorityQueue<>(Comparator.reverseOrder());
    for(int x: in) pr.add(x);
    StringBuilder sr=new StringBuilder(); while(!pr.isEmpty()) sr.append(pr.poll()).append(" ");
    System.out.println("int reverse-cmp: "+sr.toString().trim());
    // custom comparator (by abs distance from 5)
    PriorityQueue<Integer> pc = new PriorityQueue<>((x,y)->Integer.compare(Math.abs(x-5),Math.abs(y-5)));
    for(int x: in) pc.add(x);
    System.out.println("closest-to-5 first: "+pc.poll()+" "+pc.poll());
    System.out.println("DONE-PQFIX");
  }
}
