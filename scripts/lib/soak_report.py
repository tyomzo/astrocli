#!/usr/bin/env python3
"""soak_report.py — turn a soak's samples into the M2-T05 evidence page.

Usage:
    scripts/lib/soak_report.py --samples samples.csv --events events.jsonl --out soak.md
                               [--minutes 120] [--interval 60] [--rss-limit 512]

Exit: 0 = clean, 1 = a lost frame, a wedge or an RSS breach.

The RSS plot is ASCII on purpose. A PNG in `docs/evidence/` is a binary blob in a git
diff that nobody can review and every future reader has to download something to open;
forty characters of sparkline in a Markdown table are legible in a terminal, in a diff,
and on a phone at the desk — which is where this gets read.
"""

import argparse
import csv
import json
import statistics
import sys

BLOCKS = "▁▂▃▄▅▆▇█"


def sparkline(values):
    if not values:
        return ""
    low, high = min(values), max(values)
    if high == low:
        return BLOCKS[0] * len(values)
    span = high - low
    return "".join(BLOCKS[min(int((v - low) / span * (len(BLOCKS) - 1)), len(BLOCKS) - 1)]
                   for v in values)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", required=True)
    parser.add_argument("--events", default="")
    parser.add_argument("--out", required=True)
    parser.add_argument("--minutes", type=int, default=120)
    parser.add_argument("--interval", type=int, default=60)
    parser.add_argument("--rss-limit", type=int, default=512)
    args = parser.parse_args()

    with open(args.samples) as handle:
        rows = list(csv.DictReader(handle))
    if not rows:
        print("soak_report: no samples", file=sys.stderr)
        return 1

    rss = [int(r["rss_mb"]) for r in rows if r.get("rss_mb", "").isdigit()]
    previews = [float(r["preview_s"]) for r in rows if r.get("preview_s")]
    lost = [r for r in rows if r["verdict"] == "LOST"]
    refused = [r for r in rows if r["verdict"] == "CAPTURE REFUSED"]
    breaches = [v for v in rss if v > args.rss_limit]

    alerts = []
    if args.events:
        try:
            with open(args.events) as handle:
                for line in handle:
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if event.get("topic") == "alert":
                        alerts.append(event.get("data") or {})
        except OSError:
            pass

    lines = []
    lines.append("# Desk soak\n")
    lines.append(
        f"{args.minutes} min planned · one capture every {args.interval} s · "
        f"{len(rows)} rounds completed\n"
    )

    lines.append("| | |")
    lines.append("|---|---|")
    lines.append(f"| rounds | {len(rows)} |")
    lines.append(f"| lost frames | **{len(lost)}** |")
    lines.append(f"| captures refused | **{len(refused)}** |")
    if rss:
        lines.append(
            f"| field-node RSS | min {min(rss)} MB · median {int(statistics.median(rss))} MB · "
            f"**peak {max(rss)} MB** |"
        )
        lines.append(f"| PRF-05 line | {args.rss_limit} MB — "
                     f"{'**breached**' if breaches else 'held'} "
                     f"({max(rss) * 100 // args.rss_limit} % of it at peak) |")
    if previews:
        lines.append(
            f"| preview latency | median {statistics.median(previews):.2f} s · "
            f"worst {max(previews):.2f} s |"
        )
    lines.append("")

    if rss:
        lines.append("## RSS over the run\n")
        lines.append("```")
        lines.append(f"{max(rss):>5} MB  {sparkline(rss)}")
        lines.append(f"{min(rss):>5} MB  "
                     f"{'^':<1}{' ' * (max(len(rss) - 2, 0))}{'^' if len(rss) > 1 else ''}")
        lines.append(f"          round 1{' ' * max(len(rss) - 16, 1)}round {len(rows)}")
        lines.append("```\n")
        lines.append(
            "The decode spikes are **inside** these numbers rather than excluded: each sample is "
            "taken at a fixed offset after that round's capture, so the sampler lands in the same "
            "phase every time instead of at a random point between them.\n"
        )

    if alerts:
        lines.append("## Alerts\n")
        for alert in alerts:
            lines.append(f"- `{alert.get('code')}` ({alert.get('severity')}) — "
                         f"{alert.get('message')}")
        lines.append("")
    else:
        lines.append("No alerts were published during the run.\n")

    if lost or refused or breaches:
        lines.append("## Failures\n")
        for row in lost:
            lines.append(f"- round {row['round']} (t={row['t_s']}s): frame `{row['frame_id']}` "
                         "was accepted but no preview arrived in its slot")
        for row in refused:
            lines.append(f"- round {row['round']} (t={row['t_s']}s): the capture was refused")
        if breaches:
            lines.append(f"- RSS exceeded {args.rss_limit} MB on {len(breaches)} sample(s), "
                         f"peaking at {max(breaches)} MB")
        lines.append("")

    with open(args.out, "w") as handle:
        handle.write("\n".join(lines) + "\n")

    return 1 if (lost or refused or breaches) else 0


if __name__ == "__main__":
    sys.exit(main())
