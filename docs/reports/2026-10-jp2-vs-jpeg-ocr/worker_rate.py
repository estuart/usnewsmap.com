"""Worker journal entries an hour on node 3 around the run (#296).

Counts every journal entry in each window, whatever its outcome, so the
rate is attempts finished, not pages that got text.
"""
import json,datetime,sys
def p(s): return datetime.datetime.fromisoformat(s[:26]+"+00:00")
rs=[]
for l in open("/var/lib/usnm-ocr/journal.jsonl"):
    try:r=json.loads(l)
    except:continue
    rs.append((p(r["finished_at"]),r["secs"]))
for a,b in (("22:00","22:28"),("22:28","23:15"),("23:16","24:00")):
    ha,hm=map(int,a.split(":"));hb,hmb=map(int,b.split(":"))
    d=datetime.datetime(2026,10,12,tzinfo=datetime.timezone.utc)
    lo=d+datetime.timedelta(hours=ha,minutes=hm); hi=d+datetime.timedelta(hours=hb,minutes=hmb)
    s=[x for x in rs if lo<=x[0]<hi]
    print(a,b,len(s),"pages", round(len(s)/((min(hi,datetime.datetime.now(datetime.timezone.utc))-lo).total_seconds()/3600),1),"/h mean secs",round(sum(x[1] for x in s)/max(1,len(s))))
