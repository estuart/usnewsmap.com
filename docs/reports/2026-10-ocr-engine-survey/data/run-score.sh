#!/bin/bash
# Scores every engine on node 2 (#296), in a python:3.12-slim container with wordfreq 3.1.1 and lxml.
# Code: repo/ (score.py as on ocr/engine-survey; ja-ocr/quality.py as of 5ced3c6).
cd /tmp
export XDG_RUNTIME_DIR=/run/user/1000
echo "$(date -u +%FT%TZ) score start" >> /srv/bulk/ocr-survey/status.txt
podman run --rm -v /srv/bulk/ocr-survey/sample:/sample:ro,z -v /srv/bulk/ocr-survey/repo:/repo:ro,z -v /srv/bulk/ocr-survey/score:/score:z docker.io/library/python:3.12-slim bash -c "pip install -q wordfreq==3.1.1 lxml && pip freeze > /score/pip-freeze.txt && python3 /repo/scripts/ocr-survey/score.py --sample /sample --work /score/work --out /score/data --timing /score/timing-node1 --timing /score/timing-node2 && python3 /repo/scripts/ocr-survey/tables.py --data /score/data > /score/tables.md"
rc=$?
echo "$(date -u +%FT%TZ) score done rc=$rc" >> /srv/bulk/ocr-survey/status.txt
exit $rc
