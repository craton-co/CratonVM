import org.apache.commons.math4.legacy.core.MathArrays;
import org.apache.commons.math4.legacy.core.IntegerSequence;
import org.apache.commons.math4.legacy.core.dfp.*;
import org.apache.commons.math4.legacy.linear.*;
import org.apache.commons.math4.neuralnet.*;
import org.apache.commons.math4.neuralnet.sofm.*;

public final class NumericalProbe {
    static volatile long sink;
    static long hash=0xcbf29ce484222325L;
    static void mix(long x) { hash=(hash^x)*0x100000001b3L; }
    static void mix(double x){mix(Double.doubleToLongBits(x));}
    static void mix(double[] x){for(double v:x)mix(v);}
    static void mix(String x){for(int i=0;i<x.length();i++)mix(x.charAt(i));}
    static void mix(RealMatrix x){for(double[] r:x.getData())mix(r);}
    static double dot(double[] a,double[] b,double seed){double s=seed;for(int i=0;i<a.length;i++)s+=a[i]*b[i];return s;}
    static float dotf(float[] a,float[] b,float seed){float s=seed;for(int i=0;i<a.length;i++)s+=a[i]*b[i];return s;}
    static void dfp(String family){
        for(int precision:new int[]{20,80,160}) {
            DfpField f=new DfpField(precision); Dfp x=f.newDfp("1.234567890123456789"), y=f.newDfp("0.9876543210987654321");
            if(family.equals("dfp-dec")) x=new DfpDec(x);
            for(int r=0;r<24;r++) x=x.multiply(y).add(f.newDfp("0.0123456789")).subtract(y).divide(f.newDfp("1.001"));
            if(family.equals("dfp-math")){x=DfpMath.exp(y).add(DfpMath.log(y)).add(DfpMath.sin(y)).add(DfpMath.cos(y)).add(DfpMath.pow(y,7));}
            mix(x.toString());mix(f.getIEEEFlags());
        }
    }
    static void arrays(){
        for(int n:new int[]{0,1,7,8,9,15,16,17,31,32,33,257,4097}) {
            double[] a=new double[n],b=new double[n];for(int i=0;i<n;i++){a[i]=(i%13-6)*0.125;b[i]=1+(i%17)*0.0625;}
            double[] c=null;
            for(int r=0;r<128;r++){c=MathArrays.ebeAdd(a,b);c=MathArrays.ebeMultiply(c,b);MathArrays.scaleInPlace(0.125,c);sink=Double.doubleToRawLongBits(dot(a,b,-0.0));}
            mix(c);mix(dot(a,b,-0.0));mix(MathArrays.distance(a,b));
        }
        double[] a=new double[33],b=new double[33];float[] af=new float[33],bf=new float[33];
        for(int i=0;i<33;i++){a[i]=new double[]{1e20,1,-1e20,1}[i%4];b[i]=1;af[i]=(float)a[i];bf[i]=1;}
        for(int r=0;r<2000;r++){sink=Double.doubleToRawLongBits(dot(a,b,17));sink=Float.floatToRawIntBits(dotf(af,bf,-0.0f));}
        mix(dot(a,b,17));mix(Float.floatToRawIntBits(dotf(af,bf,-0.0f)));
    }
    static void sequence(){for(int r=0;r<128;r++){long s=0;for(int i:IntegerSequence.range(-17,257,3))s+=i;IntegerSequence.Incrementor inc=IntegerSequence.Incrementor.create().withStart(-5).withMaximalCount(257).withIncrement(2);while(inc.canIncrement())inc.increment();mix(s);mix(inc.getCount());}}
    static RealMatrix matrix(int n){double[][] a=new double[n][n];for(int i=0;i<n;i++)for(int j=0;j<n;j++)a[i][j]=i==j?32+i:(1+((i+j)%7))*0.03125;return new Array2DRowRealMatrix(a,false);}
    static void decomposition(String family){
        for(int n:new int[]{3,9,17,33}){
            RealMatrix a=matrix(n), rhs=matrix(n);
            for(int r=0;r<24;r++){
                if(family.equals("lu")){LUDecomposition d=new LUDecomposition(a);if(r==23){mix(d.getL());mix(d.getU());mix(d.getDeterminant());mix(d.getSolver().solve(rhs));}}
                else if(family.equals("qr")){QRDecomposition d=new QRDecomposition(a);if(r==23){mix(d.getQ());mix(d.getR());mix(d.getSolver().solve(rhs));}}
                else if(family.equals("cholesky")){CholeskyDecomposition d=new CholeskyDecomposition(a);if(r==23){mix(d.getL());mix(d.getDeterminant());mix(d.getSolver().solve(rhs));}}
                else {EigenDecomposition d=new EigenDecomposition(a);if(r==23){mix(d.getRealEigenvalues());mix(d.getV());mix(d.getSolver().solve(rhs));}}
            }
        }
    }
    static void som(){
        int dim=33;Network net=new Network(0,dim);Neuron[] neurons=new Neuron[3];
        for(int k=0;k<3;k++){double[] f=new double[dim];for(int i=0;i<dim;i++)f[i]=(k+1)*0.125+i*0.0078125;neurons[k]=net.getNeuron(net.createNeuron(f));}
        net.addLink(neurons[0],neurons[1]);net.addLink(neurons[1],neurons[0]);net.addLink(neurons[1],neurons[2]);net.addLink(neurons[2],neurons[1]);
        KohonenUpdateAction update=new KohonenUpdateAction(new EuclideanDistance(),LearningFactorFunctionFactory.exponentialDecay(0.5,0.05,1000),NeighbourhoodSizeFunctionFactory.exponentialDecay(2,1,1000));
        for(int r=0;r<128;r++){double[] sample=new double[dim];for(int i=0;i<dim;i++)sample[i]=(r%7)*0.0625+i*0.00390625;update.update(net,sample);}
        for(Neuron n:neurons)mix(n.getFeatures());
    }
    public static void main(String[] args){String f=args[0];switch(f){case "dfp":case "dfp-dec":case "dfp-math":dfp(f);break;case "arrays":arrays();break;case "sequence":sequence();break;case "som":som();break;default:decomposition(f);}
        System.out.println(f+" "+Long.toUnsignedString(hash,16));}
}
