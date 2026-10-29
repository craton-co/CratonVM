#!/bin/bash
# usage: cold.sh <outdir> <rounds> <class...>
# configs come from /root/p5kit/cold.conf: lines "name|command prefix (class appended)"
# per round, every config runs once per class (interleaved); records wall ms, instructions:u,
# task-clock ms, max RSS KiB; checks stdout+rc against HotSpot.
O=$1; R=$2; shift 2; mkdir -p $O; CP=/root/p5kit/probes/cls
HS=/usr/lib/jvm/java-25-openjdk-amd64/bin/java
for c in "$@"; do $HS -cp $CP $c > $O/$c.ref 2>&1; echo "rc=$?" >> $O/$c.ref; done
for r in $(seq $R); do
  while IFS='|' read -r name cmd; do
    [ -z "$name" ] && continue
    for c in "$@"; do
      cmdl="${cmd//@CP@/$CP}"; cmdl="${cmdl//@C@/$c}"
      s=$(date +%s%N)
      /usr/bin/time -f "%M" -o $O/rss.tmp perf stat -x, -o $O/perf.tmp -e instructions:u,task-clock bash -c "exec $cmdl" > $O/out.tmp 2>&1; rc=$?
      e=$(date +%s%N); echo "rc=$rc" >> $O/out.tmp
      ok=OK; cmp -s $O/out.tmp $O/$c.ref || ok=DIFF
      ins=$(grep instructions $O/perf.tmp | cut -d, -f1); tc=$(grep task-clock $O/perf.tmp | cut -d, -f1)
      echo "$name $c $(( (e-s)/1000 )) $ins $tc $(tail -1 $O/rss.tmp) $ok" >> $O/raw.txt
      [ $ok = DIFF ] && cp $O/out.tmp $O/$name.$c.diffout
    done
  done < /root/p5kit/cold.conf
done
# summary: name class wall_min_ms wall_med_ms instr_med_M task_med_ms rss_med_MiB verdict
awk '{k=$1" "$2; n[k]++; w[k,n[k]]=$3/1000; i[k,n[k]]=$4/1e6; t[k,n[k]]=$5; m[k,n[k]]=$6/1024; if($7!="OK")bad[k]++}
function med(a,k,c,  x,j,v,tmp){for(j=1;j<=c;j++)v[j]=a[k,j]; for(x=1;x<=c;x++)for(j=x+1;j<=c;j++)if(v[j]<v[x]){tmp=v[x];v[x]=v[j];v[j]=tmp}; return v[int((c+1)/2)]}
function mn(a,k,c,  j,r){r=a[k,1];for(j=2;j<=c;j++)if(a[k,j]<r)r=a[k,j];return r}
END{for(k in n){printf "%s %.1f %.1f %.1f %.1f %.1f %s\n",k,mn(w,k,n[k]),med(w,k,n[k]),med(i,k,n[k]),med(t,k,n[k]),med(m,k,n[k]),(bad[k]?"DIFF"bad[k]:"SAME")}}' $O/raw.txt | sort > $O/summary.txt
cat $O/summary.txt
