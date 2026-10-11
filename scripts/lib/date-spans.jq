# The share of searches by date span and by decade partitions touched (#123),
# for scripts/searches.sh. Input: the array of search-log records
# (crates/usnm-api/src/searchlog.rs). `--arg by span` counts by date span,
# `--arg by decades` by partitions touched. Output: tab-separated rows of
# aggregates only: no query text ever reaches it.
#
# A record's `from` and `to` are already clamped to the corpus, so a search
# over the whole range (the site's default) has the earliest `from` and the
# latest `to` of its index version and corpus: those are counted as "whole
# range" in both tables, whatever their span. Spans count inclusive days: "a
# year or less" is up to 366 days (one calendar year), "1-10 years" up to
# 3,653 (one calendar decade).
#
# Partitions are bucketed as usnm_core::decade::of_year: one per decade from
# 1820, with every year before 1820 in the 1820 partition.

# Days since 1970-01-01 of a YYYY-MM-DD date (proleptic Gregorian), without
# the platform's mktime, which not every jq handles before 1970.
def day_number:
  split("-") | map(tonumber) | .[0] as $y0 | .[1] as $m | .[2] as $d
  | (if $m <= 2 then $y0 - 1 else $y0 end) as $y
  | ($y / 400 | floor) as $era
  | ($y - $era * 400) as $yoe
  | (((153 * (if $m > 2 then $m - 3 else $m + 9 end) + 2) / 5 | floor) + $d - 1) as $doy
  | ($yoe * 365 + ($yoe / 4 | floor) - ($yoe / 100 | floor) + $doy) as $doe
  | $era * 146097 + $doe - 719468;

def partition: (.[0:4] | tonumber) as $y | ([($y / 10 | floor) * 10, 1820] | max);

def share($n; $total):
  if $total == 0 then "-" else "\(($n * 1000 / $total | round) / 10)%" end;

def table($first; $order):
  length as $total
  | (group_by(.k) | map({key: .[0].k, value: length}) | from_entries) as $n
  | [$first, "searches", "share"],
    ($order[] | [., $n[.] // 0, share($n[.] // 0; $total)])
  | map(tostring) | @tsv;

map(select((.from | type) == "string" and (.to | type) == "string"))
| group_by([.index_version, .corpus])
| map((map(.from) | min) as $lo | (map(.to) | max) as $hi
      | map(. + {whole: (.from == $lo and .to == $hi),
                 days: ((.to | day_number) - (.from | day_number) + 1),
                 decades: (((.to | partition) - (.from | partition)) / 10 + 1)}))
| add // []
| if $by == "span" then
    map(.k = if .whole then "whole range"
             elif .days > 3653 then "over 10 years"
             elif .days > 366 then "1-10 years"
             else "a year or less" end)
    | table("span"; ["whole range", "over 10 years", "1-10 years", "a year or less"])
  else
    map(.k = if .whole then "whole range"
             elif .decades <= 3 then "\(.decades)"
             elif .decades <= 5 then "4-5"
             elif .decades <= 10 then "6-10"
             else "11 or more" end)
    | table("decades"; ["whole range", "1", "2", "3", "4-5", "6-10", "11 or more"])
  end
