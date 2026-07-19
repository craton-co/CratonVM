public class BinT {
    static final class Node { Node l,r; Node(Node l,Node r){this.l=l;this.r=r;} }
    static Node make(int d){ return d==0? new Node(null,null) : new Node(make(d-1), make(d-1)); }
    static int check(Node n){ return n.l==null?1:1+check(n.l)+check(n.r); }
    public static void main(String[] a){
        int d=Integer.parseInt(a[0]); long t=System.currentTimeMillis(); long sum=0;
        for(int i=0;i<10;i++){ Node n=make(d); sum+=check(n); }
        System.out.println("BinT d="+d+" ms="+(System.currentTimeMillis()-t)+" sum="+sum);
    }
}
