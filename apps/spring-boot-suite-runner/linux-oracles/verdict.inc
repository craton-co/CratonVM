verdict() {  # verdict <rc> <resultline> <label>
  local rc="$1" res="$2" label="$3" f cf t
  if [ "$rc" = "124" ]; then echo "TIMEOUT $label"; return; fi
  if [ -z "$res" ]; then echo "NORESULT rc=$rc $label"; return; fi
  # Extract fields individually. A substring test on "failed=0" also matches
  # inside "containersFailed=0", which made a LOADFAIL report PASS.
  f=$(echo "$res"  | grep -oE '(^| )failed=[0-9]+'      | grep -oE '[0-9]+')
  cf=$(echo "$res" | grep -oE 'containersFailed=[0-9]+' | grep -oE '[0-9]+')
  t=$(echo "$res"  | grep -oE 'tests=[0-9]+'            | grep -oE '[0-9]+')
  if [ -z "$f" ] || [ -z "$cf" ] || [ -z "$t" ]; then echo "VERDICT-UNPARSED $res"
  elif [ "$t" -eq 0 ]; then echo "FAIL(vacuous-0-tests) $res"
  elif [ "$f" -eq 0 ] && [ "$cf" -eq 0 ]; then echo "PASS $res"
  else echo "FAIL $res"; fi
}
