// aaload against iaload, same loop shape, same length, both arrays small
// enough to be cache-resident so this is path cost and not memory.
public class RefLoad {
    static int ctl(int n, int[] ia, Object[] oa){int s=0;for(int i=0;i<n;i++){s+=i;}return s;}
    static int iload(int n, int[] ia, Object[] oa){int s=0;int m=ia.length-1;for(int i=0;i<n;i++){s+=ia[i&m];}return s;}
    static int aload(int n, int[] ia, Object[] oa){int s=0;int m=ia.length-1;for(int i=0;i<n;i++){if(oa[i&m]!=null)s++;}return s;}
    static int iload2(int n,int[] ia,Object[] oa){int s=0;int m=ia.length-1;for(int i=0;i<n;i++){if(ia[i&m]!=0)s++;}return s;}
    public static void main(String[] g){
        int n=g.length>0?Integer.parseInt(g[0]):2000000, r=g.length>1?Integer.parseInt(g[1]):5;
        int[] ia=new int[1024]; Object[] oa=new Object[1024];
        for(int k=0;k<1024;k++){ia[k]=k+1;oa[k]=new Object();}
        String[] nm={"control","iaload","aaload","iaload_ifne"};
        double[] m=new double[4]; for(int i=0;i<4;i++)m[i]=1e18;
        int sink=0; long t;
        for(int p=0;p<r;p++){boolean f=(p&1)==0;
            for(int k=0;k<4;k++){int j=f?k:3-k; t=System.nanoTime();
                switch(j){case 0:sink+=ctl(n,ia,oa);break;case 1:sink+=iload(n,ia,oa);break;
                          case 2:sink+=aload(n,ia,oa);break;default:sink+=iload2(n,ia,oa);break;}
                double d=(System.nanoTime()-t)/(double)n; if(d<m[j])m[j]=d;}}
        for(int i=0;i<4;i++)System.out.println(nm[i]+"\t"+Math.round(m[i]*100)/100.0+"\tover_control="+(Math.round((m[i]-m[0])*100)/100.0));
        System.out.println("aaload_over_iaload_ifne = "+(Math.round((m[2]-m[3])*100)/100.0));
        if(sink==42)System.out.println("x");
    }
}
