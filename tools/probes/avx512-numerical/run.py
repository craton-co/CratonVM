#!/usr/bin/env python3
"""Fresh HotSpot/CVM gates for all ten numerical families; retains evidence."""
import argparse,glob,json,os,subprocess,time,hashlib
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('vm');p.add_argument('--cm-root',default='/root/CVM/apps/commons-math');p.add_argument('--work-dir',required=True);p.add_argument('--coverage',action='store_true');a=p.parse_args()
repo=Path(__file__).resolve().parents[3];work=Path(a.work_dir).resolve();work.mkdir(parents=True,exist_ok=True);classes=work/'classes';classes.mkdir(exist_ok=True)
root=Path(a.cm_root)
cp=[str(root/m/'target'/d) for m in ['commons-math-core','commons-math-legacy-core','commons-math-legacy-exception','commons-math-legacy','commons-math-transform','commons-math-neuralnet'] for d in ['classes','test-classes']]
cp += [x for x in glob.glob(str(Path.home()/'.m2/repository/org/apache/commons/**/*.jar'),recursive=True) if not any(k in x for k in ['sources','javadoc'])]
for prefix in ['.m2/repository/junit/junit/4.13.2/','.m2/repository/org/hamcrest/hamcrest-core/1.3/','.gradle/caches/modules-2/files-2.1/junit/junit/4.13.2/','.gradle/caches/modules-2/files-2.1/org.hamcrest/hamcrest-core/1.3/']:
    cp += glob.glob(str(Path.home()/prefix)+'**/*.jar',recursive=True)
cp=':'.join([str(classes)]+[x for x in cp if Path(x).exists()]);(work/'classpath.txt').write_text(cp)
java=os.environ.get('JAVA','java');javac=os.environ.get('JAVAC','javac');vm=str(Path(a.vm).resolve())
subprocess.run([javac,'-cp',cp,'-d',str(classes),*[str(x) for x in Path(__file__).parent.glob('*.java')]],check=True)
cases=[('dfp','legacy.core.dfp.DfpTest'),('dfp-math','legacy.core.dfp.DfpMathTest'),('dfp-dec','legacy.core.dfp.DfpDecTest'),('arrays','legacy.core.MathArraysTest'),('sequence','legacy.core.IntegerSequenceTest'),('lu','legacy.linear.LUDecompositionTest'),('qr','legacy.linear.QRDecompositionTest'),('cholesky','legacy.linear.CholeskyDecompositionTest'),('eigen','legacy.linear.EigenDecompositionTest'),('som','neuralnet.sofm.KohonenUpdateActionTest')]
rows=[];coverage={};failures=[]
def run(label,command,flags='',debug=False):
    env=os.environ.copy();env['CRATONVM_JIT']=flags;env.pop('CRATONVM_DBG',None)
    if debug:env['CRATONVM_DBG']='jitc'
    start=time.monotonic()
    try:
        r=subprocess.run(command,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=180);rc=r.returncode;out=r.stdout;err=r.stderr
    except subprocess.TimeoutExpired as e:rc=124;out=e.stdout or b'';err=e.stderr or b''
    (work/(label+'.out')).write_bytes(out);(work/(label+'.err')).write_bytes(err)
    return rc,out,time.monotonic()-start
for family,short in cases:
    cls='org.apache.commons.math4.'+short
    refs={}
    for kind,main,arg in [('junit','NumericalJUnitProbe',cls),('numeric','NumericalProbe',family)]:
        rc,out,secs=run(family+'.hotspot.'+kind,[java,'-Xmx2g','-cp',cp,main,arg]);refs[kind]=out
        rows.append(dict(family=family,mode='hotspot',setting='reference',kind=kind,rc=rc,seconds=secs,match=rc==0))
        if rc:failures.append(family+'.hotspot.'+kind)
    for mode in ['jdk-only','compatible']:
        for setting,flags in [('default',''),('master-off','avx512=0'),('numerical-off','avx512-vector-loop=0,ir-vector-ordered-fp=0')]:
            for kind,main,arg in [('junit','NumericalJUnitProbe',cls),('numeric','NumericalProbe',family)]:
                label='.'.join([family,mode,setting,kind]);rc,out,secs=run(label,[vm,'--'+mode,'-Xmx2g','-cp',cp,main,arg],flags)
                ok=rc==0 and out==refs[kind];rows.append(dict(family=family,mode=mode,setting=setting,kind=kind,rc=rc,seconds=secs,match=ok))
                if not ok:failures.append(label)
            print(f'{family} {mode} {setting} '+('MATCH' if all(r['match'] for r in rows[-2:]) else 'FAIL'),flush=True)
        if a.coverage:
            label=family+'.'+mode+'.coverage';rc,out,secs=run(label,[vm,'--'+mode,'-Xmx2g','-cp',cp,'NumericalProbe',family],debug=True)
            lines=(work/(label+'.err')).read_text(errors='replace').splitlines();coverage[label]=sorted(set(x for x in lines if x.startswith('[ir-vec]') and 'bits=512' in x))
            if rc or out!=refs['numeric']:failures.append(label)
    (work/'results.json').write_text(json.dumps(dict(rows=rows,coverage=coverage,failures=failures),indent=2))
with open(vm,'rb') as f:digest=hashlib.file_digest(f,'sha256').hexdigest()
(work/'manifest.json').write_text(json.dumps(dict(vm=vm,sha256=digest,source=subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip(),dirty=bool(subprocess.check_output(['git','status','--porcelain'],cwd=repo,text=True)),classpath=cp),indent=2))
print('NUMERICAL_GATE_OK' if not failures else 'NUMERICAL_GATE_FAILED '+','.join(failures),flush=True)
raise SystemExit(bool(failures))
