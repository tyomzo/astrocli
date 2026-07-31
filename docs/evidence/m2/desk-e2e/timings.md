# Desk E2E — preview latency

Driver: `gphoto2` · budget: 3.0 s · frames measured: 1

Measured from `capture.progress: saved` (the frame is durable — **after** the download) to `capture.progress: preview_ready` (cached and pushed). Both are arrival times in one monotonic `/ws` recording.

| frame | exposure (s) | saved at (s) | preview at (s) | latency (s) | |
|---|---|---|---|---|---|
| `light_00001` | 9.0 | 21.169 | 21.294 | **0.124** | ok |

**worst 0.124 s · median 0.124 s · best 0.124 s** against a 3.0 s budget (4 % of it at worst).

- live-view frames observed on `/ws/liveview`: 40
  - 5.1 fps over 7.7 s, mean 149 KB/frame
- `camera.status` events: 2
- alerts: 0

