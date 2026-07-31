#!/usr/bin/env python3
"""pull_report.py — the recovery timings from a cable-pull recording. M2-T05 / T-CAM-1.

Usage:
    scripts/lib/pull_report.py --events events.jsonl --out cable-pull.md [--working 0|1]
                               [--budget 30]

Exit: 0 = recovered to a working capture inside the budget, 1 = it did not.

Every offset is measured from **the first evidence the node had that anything was wrong** —
the first `camera.status` with `connected: false`, or the first `CAMERA_RECONNECTING`
alert, whichever arrived first. Timing from when the operator's hand moved would be a
kinder number and a meaningless one: nothing observes that.
"""

import argparse
import json
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--events", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--budget", type=float, default=30.0)
    parser.add_argument("--working", type=int, default=0)
    args = parser.parse_args()

    events = []
    try:
        with open(args.events) as handle:
            for line in handle:
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except OSError as error:
        print(f"pull_report: {error}", file=sys.stderr)
        return 1

    fault_at = None
    restored_at = None
    faulted_at = None
    timeline = []

    for event in events:
        topic = event.get("topic")
        data = event.get("data") or {}
        t = event.get("t")

        if topic == "camera.status":
            connected = data.get("connected")
            timeline.append((t, "camera.status", f"connected={connected}"))
            if connected is False and fault_at is None:
                fault_at = t
            if connected is True and fault_at is not None and restored_at is None:
                restored_at = t
        elif topic == "alert":
            code = data.get("code", "")
            timeline.append((t, f"alert/{code}", f"{data.get('severity')}: {data.get('message')}"))
            if code == "CAMERA_RECONNECTING":
                if data.get("severity") == "warning" and fault_at is None:
                    fault_at = t
                if data.get("severity") == "info" and restored_at is None:
                    restored_at = t
            if code == "CAMERA_LINK_FAULTED":
                faulted_at = t

    lines = ["# Cable pull — recovery timing (T-CAM-1)\n"]

    if fault_at is None:
        lines.append("**No fault was observed.** Either the cable was not pulled during the "
                     "watch window, or the node never noticed — which would itself be the "
                     "finding. The raw recording is in `events.jsonl`.\n")
    else:
        lines.append(f"- fault first visible to the node at **t = {fault_at:.2f} s** "
                     "(the watcher's own clock)")
        if restored_at is not None:
            recovery = restored_at - fault_at
            verdict = "**inside**" if recovery <= args.budget else "**OUTSIDE**"
            lines.append(f"- link reported back at t = {restored_at:.2f} s")
            lines.append(f"- **recovery took {recovery:.2f} s**, {verdict} REL-03's "
                         f"{args.budget:.0f} s\n")
        elif faulted_at is not None:
            lines.append(f"- recovery gave up at t = {faulted_at:.2f} s "
                         "(`CAMERA_LINK_FAULTED`)\n")
        else:
            lines.append("- the link never came back during the watch window\n")

    lines.append(f"- a capture after the watch window: "
                 f"**{'succeeded' if args.working else 'did not succeed'}** — this, not the "
                 "badge, is T-CAM-1's actual criterion\n")

    lines.append("## Timeline\n")
    lines.append("| t (s) | what | detail |")
    lines.append("|---|---|---|")
    for t, what, detail in timeline:
        detail = str(detail).replace("|", "\\|")
        lines.append(f"| {t:.2f} | `{what}` | {detail} |")
    lines.append("")

    gvfs = [d for _, w, d in timeline if "gvfs" in str(d).lower() or "gio mount" in str(d).lower()]
    if gvfs:
        lines.append("## gvfs took the claim\n")
        lines.append("The desktop's gvfs auto-mounted the camera on re-enumeration and held the "
                     "USB claim — the failure M2-T01 measured at 80 s. The driver named the "
                     "releasing command; run it and the camera comes back:\n")
        for detail in gvfs:
            lines.append(f"> {detail}\n")

    with open(args.out, "w") as handle:
        handle.write("\n".join(lines) + "\n")

    recovered = restored_at is not None and (restored_at - fault_at) <= args.budget
    return 0 if (recovered and args.working) else 1


if __name__ == "__main__":
    sys.exit(main())
