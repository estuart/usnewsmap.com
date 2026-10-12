#!/bin/bash
# Rescores round 1 on node 2 (#296, #370) with score.py's same-pages LoC baseline fix, and runs dedup.py.
# Same container as run-score.sh, with lxml pinned and Pillow added for dedup.py; inputs are copies of /srv/bulk/ocr-survey/{sample,score}.
R=$HOME/survey-review
echo "$(date -u +%FT%TZ) score-r2 start" >> $R/status.txt
podman run --rm -v $R/sample:/sample:ro,z -v $R/repo:/repo:ro,z -v $R/score:/score:ro,z -v $R/out:/out:z \
  docker.io/library/python:3.12-slim bash -c "pip install -q wordfreq==3.1.1 lxml==6.1.3 pillow && pip freeze > /out/pip-freeze.txt \
  && python3 /repo/scripts/ocr-survey/score.py --sample /sample --work /score/work --out /out/data --timing /score/timing-node1 --timing /score/timing-node2 > /out/score.log \
  && python3 /repo/scripts/ocr-survey/tables.py --data /out/data > /out/tables.md \
  && python3 /repo/docs/reports/2026-10-ocr-engine-survey/dedup.py --sample /sample --work /score/work --data /out/data > /out/dedup.md"
rc=$?
echo "$(date -u +%FT%TZ) score-r2 done rc=$rc" >> $R/status.txt
exit $rc
