# Desk soak

16 min planned · one capture every 60 s · 16 rounds completed

| | |
|---|---|
| rounds | 16 |
| lost frames | **6** |
| captures refused | **0** |
| field-node RSS | min 77 MB · median 94 MB · **peak 101 MB** |
| PRF-05 line | 512 MB — held (19 % of it at peak) |
| preview latency | median 0.05 s · worst 0.07 s |

## RSS over the run

```
  101 MB  ▁▄▃▇▆▂█▆▇▆▅▅▅▅▅▅
   77 MB  ^              ^
          round 1 round 16
```

The decode spikes are **inside** these numbers rather than excluded: each sample is taken at a fixed offset after that round's capture, so the sampler lands in the same phase every time instead of at a random point between them.

## Alerts

- `DEVICE_PROTOCOL` (warning) — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- `DEVICE_PROTOCOL` (warning) — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- `DEVICE_PROTOCOL` (warning) — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- `DEVICE_PROTOCOL` (warning) — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- `DEVICE_PROTOCOL` (warning) — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- `DEVICE_PROTOCOL` (warning) — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.

## Failures

- round 11 (t=600s): frame `light_00012` was accepted but no preview arrived in its slot
  - the node said so at t=631s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
  - the node said so at t=691s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- round 12 (t=660s): frame `light_00013` was accepted but no preview arrived in its slot
  - the node said so at t=691s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
  - the node said so at t=751s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- round 13 (t=720s): frame `light_00014` was accepted but no preview arrived in its slot
  - the node said so at t=751s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
  - the node said so at t=811s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- round 14 (t=780s): frame `light_00015` was accepted but no preview arrived in its slot
  - the node said so at t=811s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
  - the node said so at t=871s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- round 15 (t=840s): frame `light_00016` was accepted but no preview arrived in its slot
  - the node said so at t=871s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
  - the node said so at t=931s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.
- round 16 (t=900s): frame `light_00017` was accepted but no preview arrived in its slot
  - the node said so at t=931s: `DEVICE_PROTOCOL` — protocol error: the shutter closed on a 4 s bulb exposure but the camera had not announced a file 26 s later. A Canon with long-exposure noise reduction on shoots a matching dark frame first, which roughly doubles the wait — raise `camera.timeouts.capture_extra_seconds` or turn that off on the body.

