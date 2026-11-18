# Sensors: named channels, GPU processes, containers, model server, anomaly glow

Telemetry draws one labelled ridge per channel. This package adds channels that
are not one of the fixed kinds (CPU°, LOAD, VRAM and so on), fills them from
two opt-in sources, says who holds each card's memory, and makes a ridge's label
pulse when its live value rises clearly above its own recent range.

## Named extra channels

A named channel carries its own name (up to 12 columns), unit and scale (a full
scale value, or "scale to the largest value in view", linear or logarithmic).
Readings travel in the snapshot as an optional `extra` list, so they also cross
the `--probe` wire: a viewer of this build reads them from a newer probe, and
an older viewer ignores them. The probe protocol version is unchanged (v1).

- At most **6** named ridges are drawn, live ones first, so the fixed channels
  stay readable. When more exist the status line says `+N extra ridges not shown`.
- A named channel keeps its history while its source is silent (gaps, as for
  any channel) and is forgotten once that history has slid off the 2 h window.
  At most 24 are remembered.
- Names and units from any source, including a remote probe, are cleaned of
  terminal controls and invisible characters and cut to length before they key
  a channel or reach the screen.

## Per-process GPU memory

On the existing GPU cadence (every sensor poll while the view is on screen,
every 10 s off screen, the `--gpu-interval` period in a probe), SystemScape asks

```
nvidia-smi --query-compute-apps=pid,used_memory,gpu_uuid --format=csv,noheader,nounits
```

maps each card uuid to its index (`--query-gpu=index,uuid`, asked once and
again only when an unknown card appears), resolves each PID to a name from
`/proc/<pid>/comm`, and shows the three largest consumers per card beside that
card's VRAM ridge, for example `VRAM1  32/48G  llama-server 31G · pid 48213 1.5G`.
In the flat view the same text follows the sparkline when the pane has room
after it.

Limits:

- **Inside a container, PIDs from other PID namespaces may not resolve.** Such
  a process is shown as `pid N`. Depending on the driver, nvidia-smi inside a
  container may list no processes at all, and then nothing is shown. When the
  driver reports host PIDs, a number can also match an unrelated process in the
  container's own namespace, and that process's name would be shown. Read
  names from inside a container as a hint, not as proof.
- Memory nvidia-smi reports as `[N/A]` is left out of the text, not guessed.
- The query is bounded like the card query: killed after 1.5 s, output capped.

## Container ridges: `--containers`

Two ridges, `<name> cpu` and `<name> mem`, for each of the three busiest
containers by CPU, from

```
docker stats --no-stream --format '{{json .}}'
```

run as an argv (never a shell string) on a background thread every 10 s while
the view is on screen, and **not at all while it is off screen**. The frame loop
never waits on docker: answers are left in a shared slot that the sensor thread
merges into the next snapshot, and an answer more than 30 s old is not drawn.

- CPU is on a log scale whose full height is every core busy (docker counts 100%
  per core, so one container can read 484%). Memory is on a log scale up to the
  container's limit.
- Container names are cut to 8 columns in the ridge label.
- docker missing, a denied socket, an error, or no answer within 8 s each give no
  ridges and one status note (`containers: docker not found`,
  `containers: permission denied …`, `containers: docker stats did not answer`).
  None of them is an error and none of them hangs the view.

## Model-server ridges: `--llama-metrics URL`

**This is the one feature in SystemScape that makes a network request. It runs
only when this flag is given.** With it, SystemScape fetches a llama.cpp
server's Prometheus endpoint (`llama-server --metrics`) and draws:

| Ridge      | From                                                                   |
|------------|------------------------------------------------------------------------|
| `LLM tok`  | change in `llamacpp:tokens_predicted_total` over wall time between polls (tokens/s, idle time counts) |
| `LLM busy` | `llamacpp:requests_processing`                                         |
| `LLM wait` | `llamacpp:requests_deferred`                                           |

Every other metric is ignored. The first poll gives no token rate, and a counter
that went backwards (the server restarted) gives none for that poll.

- Plain `http://host[:port][/path]` only. `https://` is refused, as are
  credentials in the URL and paths with spaces or control characters. An empty
  path means `/metrics`.
- `std::net::TcpStream` with HTTP/1.0 and `Connection: close`. Each address gets
  a 2 s connect timeout, the whole request a 4 s deadline, and the body a
  256 KiB cap. Reading stops at `Content-Length`. A chunked answer is refused.
- It is polled on a background thread every 5 s on screen and every 15 s off
  screen (like the slowed nvidia-smi query), so the 2 h history keeps filling.
  Name resolution uses the system resolver on that thread.
- Failures give a status note (`llama: cannot connect`, `llama: timed out`,
  `llama: HTTP 404`) and no ridges.

## Anomaly glow

A ridge's label pulses between alarm red and near white on a half-second beat,
and its live value is prefixed `▲`, when the channel's current reading is above
its own band. The same marker and pulse appear in the flat sparkline view.

The band (`Channel::band`) is computed in normalised height (0 to 1), so the
same rule fits every unit. It is the mean ± 3 standard deviations of the
channel's finite history slots, with the deviation floored at 4% of full height
so that a flat channel's noise does not flag. It is judged only once at least 12
slots (30 min) hold readings.

Only **upward** excursions flag (`Channel::anomalous`). History slots hold
150 s peaks while the live value is an instant reading, so an instant below the
band of peaks is the normal case. A channel that drops to zero (a service that
died) is therefore not flagged.

## `--demo`

The demo shows two synthetic named channels, `LLM tok` and `LLM busy`. `LLM tok`
reads 96/s against a 2 h history near 30/s, so its label pulses and shows `▲`.
The demo also shows synthetic GPU processes on cards 0 and 1, one of them an
unresolved PID. The demo never runs docker and never connects anywhere, so
`--containers` and `--llama-metrics` are refused with `--demo`.

## Flags and combinations

- `--containers` and `--llama-metrics URL` apply to the local machine's view and
  may be combined with each other and with `--hosts`.
- Both are refused with `--probe` and with `--demo`.
- A remote probe's GPU process list (names resolved on that machine) is carried
  on the wire and shown for that host.

## What this does not claim

- Process names inside containers can be missing or wrong (see above).
- Container CPU and memory are docker's own figures from one `--no-stream`
  sample, not a continuous measurement.
- The token rate is throughput averaged over each poll interval, not
  per-request generation speed.
- The anomaly marker is a statistical hint against this channel's own last two
  hours, not an alert rule. It does not persist or notify.

## Help lines and key-table rows to add

Help, usage line:

```
Usage: systemscape [--demo | --hosts FILE] [--containers] [--llama-metrics URL]
```

Help, options:

```
  --containers  ridges for the busiest containers' CPU and memory (docker stats; on screen only)
  --llama-metrics URL  a llama.cpp server's /metrics over plain http:// (the only network request)
```

Help, telemetry paragraph:

```
           Named ridges (containers, model server) are capped at six; ▲ and a pulsing
           label mark a value above its own 2h band. VRAM labels name the largest processes.
```

README key-table row: none. This package adds no keys.

README feature rows:

| Feature | Flag |
|---|---|
| Top VRAM processes per card beside each VRAM ridge | always (nvidia-smi) |
| Busiest containers' CPU and memory ridges | `--containers` |
| llama.cpp tokens/s, busy and waiting requests | `--llama-metrics URL` |
| Labels pulse and show ▲ when a value leaves its 2h band | always |
