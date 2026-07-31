#!/usr/bin/env python3
"""desk_timings.py — turn a `/ws` event recording into the M2-T05 preview-latency evidence.

Usage:
    scripts/lib/desk_timings.py --events events.jsonl --out timings.md [--json timings.json]
                                [--budget 3.0] [--driver gphoto2]

Exit: 0 = every frame met the budget, 1 = one did not, 2 = the recording is unusable.

# What is being measured, and why from these two events

The acceptance criterion is "previews <= 3 s after exposure end". The field node publishes
`capture.progress` with a `state`, and two of its states bracket exactly that interval:

    saved          the frame is durable on the node's disk (SDD §4.3)
    preview_ready  the preview is cached and has been pushed to `/ws/liveview`

`saved` is used as "exposure end" and it is deliberately the **pessimistic** choice: it
lands after the download, not when the shutter closed. On the R10 a 24 MP CR3 is a
1.5-2 s download, so every latency reported here has already spent that inside its
budget. Measuring instead from a shutter-close the node does not publish would flatter
the result by exactly the download time, which is the largest term.

Both timestamps are arrival times in one monotonic recording made by one subscriber, so
nothing here depends on the field node's clock agreeing with this machine's, and no
round trip of the measuring script is inside the number.
"""

import argparse
import json
import statistics
import sys


def load(path):
    events = []
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return events


def frame_of(event):
    data = event.get("data") or {}
    return data.get("frame_id") or data.get("frame") or data.get("id")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--events", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--json", default="")
    parser.add_argument("--budget", type=float, default=3.0)
    parser.add_argument("--driver", default="gphoto2")
    args = parser.parse_args()

    events = load(args.events)
    if not events:
        print(f"desk_timings: {args.events} holds no events", file=sys.stderr)
        return 2

    saved, ready, exposure = {}, {}, {}
    for event in events:
        if event.get("topic") != "capture.progress":
            continue
        data = event.get("data") or {}
        frame = frame_of(event)
        if not frame:
            continue
        state = data.get("state")
        if state == "saved":
            # First one wins: a republished stateful topic must not overwrite the arrival that
            # was actually the transition.
            saved.setdefault(frame, event["t"])
            if data.get("elapsed_s") is not None:
                exposure.setdefault(frame, data["elapsed_s"])
        elif state == "preview_ready":
            ready.setdefault(frame, event["t"])

    rows = []
    for frame in sorted(saved):
        if frame not in ready:
            rows.append((frame, exposure.get(frame), None, "NO PREVIEW"))
            continue
        latency = ready[frame] - saved[frame]
        verdict = "ok" if latency <= args.budget else "OVER BUDGET"
        rows.append((frame, exposure.get(frame), latency, verdict))

    latencies = [row[2] for row in rows if row[2] is not None]
    breaches = [row for row in rows if row[3] != "ok"]

    liveview = [e for e in events if e.get("topic") == "liveview.frame"]
    alerts = [e for e in events if e.get("topic") == "alert"]
    statuses = [e for e in events if e.get("topic") == "camera.status"]

    lines = []
    lines.append("# Desk E2E — preview latency\n")
    lines.append(f"Driver: `{args.driver}` · budget: {args.budget:.1f} s · "
                 f"frames measured: {len(rows)}\n")
    lines.append("Measured from `capture.progress: saved` (the frame is durable — **after** the "
                 "download) to `capture.progress: preview_ready` (cached and pushed). Both are "
                 "arrival times in one monotonic `/ws` recording.\n")
    lines.append("| frame | exposure (s) | saved at (s) | preview at (s) | latency (s) | |")
    lines.append("|---|---|---|---|---|---|")
    for frame, exposure_s, latency, verdict in rows:
        exposure_text = f"{exposure_s:.1f}" if exposure_s is not None else "—"
        latency_text = f"**{latency:.3f}**" if latency is not None else "—"
        lines.append(
            f"| `{frame}` | {exposure_text} | {saved[frame]:.3f} | "
            f"{ready.get(frame, float('nan')):.3f} | {latency_text} | {verdict} |"
        )
    lines.append("")

    if latencies:
        lines.append(
            f"**worst {max(latencies):.3f} s · median {statistics.median(latencies):.3f} s · "
            f"best {min(latencies):.3f} s** against a {args.budget:.1f} s budget "
            f"({max(latencies) / args.budget * 100:.0f} % of it at worst).\n"
        )

    lines.append(f"- live-view frames observed on `/ws/liveview`: {len(liveview)}")
    if liveview:
        span = liveview[-1]["t"] - liveview[0]["t"]
        rate = (len(liveview) - 1) / span if span > 0 else 0.0
        sizes = [e["data"]["bytes"] for e in liveview if "bytes" in (e.get("data") or {})]
        lines.append(f"  - {rate:.1f} fps over {span:.1f} s"
                     + (f", mean {statistics.mean(sizes) / 1024:.0f} KB/frame" if sizes else ""))
    lines.append(f"- `camera.status` events: {len(statuses)}")
    lines.append(f"- alerts: {len(alerts)}")
    for alert in alerts:
        data = alert.get("data") or {}
        lines.append(f"  - `{data.get('code')}` ({data.get('severity')}) — {data.get('message')}")
    lines.append("")

    if breaches:
        lines.append(f"**{len(breaches)} frame(s) missed the budget.**\n")

    with open(args.out, "w") as handle:
        handle.write("\n".join(lines) + "\n")

    if args.json:
        with open(args.json, "w") as handle:
            json.dump(
                {
                    "driver": args.driver,
                    "budget_s": args.budget,
                    "frames": [
                        {
                            "frame_id": frame,
                            "exposure_s": exposure_s,
                            "saved_t": saved[frame],
                            "preview_t": ready.get(frame),
                            "latency_s": latency,
                            "verdict": verdict,
                        }
                        for frame, exposure_s, latency, verdict in rows
                    ],
                    "worst_s": max(latencies) if latencies else None,
                    "median_s": statistics.median(latencies) if latencies else None,
                    "liveview_frames": len(liveview),
                    "alerts": [a.get("data") for a in alerts],
                },
                handle,
                indent=2,
            )

    if not rows:
        print("desk_timings: no captures found in the recording", file=sys.stderr)
        return 2
    return 1 if breaches else 0


if __name__ == "__main__":
    sys.exit(main())
