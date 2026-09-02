public class DecodedShare {
    static int sw(int n){int a=0;for(int i=0;i<n;i++){switch(i&7){case 0:a+=1;break;case 1:a+=2;break;
      case 2:a+=3;break;case 3:a+=4;break;case 4:a+=5;break;case 5:a+=6;break;case 6:a+=7;break;default:a+=8;}}return a;}
    static int lsw(int n){int a=0;for(int i=0;i<n;i++){switch(i&255){case 0:a+=1;break;case 17:a+=2;break;
      case 130:a+=3;break;case 200:a+=4;break;default:a+=5;}}return a;}
    static int arr(int n){byte[] b=new byte[64];int a=0;for(int i=0;i<n;i++){b[i&63]=(byte)i;a+=b[(i+1)&63];}return a;}
    static int alloc(int n){int a=0;for(int i=0;i<n;i++){int[] x=new int[4];x[0]=i;a+=x[0];}return a;}
    public static void main(String[] x){
        int n=x.length>0?Integer.parseInt(x[0]):300000;
        long t; int s=0;
        t=System.nanoTime(); s+=sw(n);   System.out.println("tableswitch  "+(System.nanoTime()-t)/(double)n);
        t=System.nanoTime(); s+=lsw(n);  System.out.println("lookupswitch "+(System.nanoTime()-t)/(double)n);
        t=System.nanoTime(); s+=arr(n);  System.out.println("bastore/baload "+(System.nanoTime()-t)/(double)n);
        t=System.nanoTime(); s+=alloc(n);System.out.println("newarray     "+(System.nanoTime()-t)/(double)n);
        if(s==42) System.out.println("x");
    }
}
