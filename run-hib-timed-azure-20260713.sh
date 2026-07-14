#!/usr/bin/env bash
set -euo pipefail
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
timeout 160 /data/data/cratonvm-hib-longtail-target-20260713-azure-004/release/cvhib-longtail-residuals-20260713-azure-004 @common.args MethodRunner org.hibernate.orm.test.function.json.JsonArrayUnnestTest testUnnest 2>&1 \
  | awk '{ print strftime("%s"), $0; fflush(); }' \
  > hib-longtail-json-timed-20260713-azure-004.log
