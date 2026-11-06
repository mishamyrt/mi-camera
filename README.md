# Mi Camera Server

RTSP video, PTZ controls, and auto tracking for the Xiaomi MJSXJ09CM on stock
firmware `4.3.4_0384` (SigmaStar Infinity6B0, SC3335). Runs on the camera and
streams hardware-encoded HEVC without transcoding.

## Workspace

- [mi-camera-video](crates/mi-camera-video): frame capture, HEVC helpers, and stream subscriptions.
- [mi-camera-rtsp](crates/mi-camera-rtsp): RTSP/RTP server using video hubs.
- [mi-camera-control](crates/mi-camera-control): PTZ and persistent auto tracking HTTP API.
- [mi-camera](crates/mi-camera): `mi-camera-server` binary that starts and connects the services.

Run `cargo test --locked` to test all workspace crates, or add `-p CRATE_NAME`
to build or test one crate.

## Usage

Replace `CAMERA_IP` with your camera's address.

| Endpoint | Purpose |
|---|---|
| `rtsp://CAMERA_IP:8554/main` | Main stream, 2304×1296 |
| `rtsp://CAMERA_IP:8554/sub` | Substream, 640×360 |
| `GET http://CAMERA_IP:8080/health` | Health check |
| `GET http://CAMERA_IP:8080/ptz/status` | PTZ position |
| `POST http://CAMERA_IP:8080/ptz/{direction}` | Move left, right, up, or down |
| `GET http://CAMERA_IP:8080/tracking` | Auto tracking status |
| `POST http://CAMERA_IP:8080/tracking` | Set auto tracking: body `on` or `off` |

Use **RTSP over TCP** and a player with HEVC decoding support:

```sh
ffplay -rtsp_transport tcp rtsp://CAMERA_IP:8554/main
curl -X POST http://CAMERA_IP:8080/ptz/left
curl -X POST --data off http://CAMERA_IP:8080/tracking
```

RTSP and HTTP each allow up to four concurrent clients. Video capture reconnects
after two seconds without a frame. Controller operations run one at a time.

Each PTZ request performs one short movement. Position values are normalized
from 1–101, not degrees. Auto tracking settings persist on the SD card across restarts.

## Home Assistant

Add the **Generic Camera** integration with the RTSP URL and TCP transport.
Ensure Home Assistant's internal URL is reachable by viewing clients.

Optional controls and dashboard:

- [mi_camera.yaml](home-assistant/mi_camera.yaml): PTZ buttons and auto tracking switch.
- [dashboard.json](home-assistant/dashboard.json): video and controls.

Update the camera IP in the package and camera entity ID in the dashboard
for your installation.

## Build

Requires Rust and Zig. Run from the project root:

```sh
rustup target add armv7-unknown-linux-musleabihf
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="$PWD/scripts/arm-linker.sh" \
RUSTFLAGS='-C link-self-contained=no' \
cargo build --locked --release --target armv7-unknown-linux-musleabihf
cargo test --locked
```

Output: `target/armv7-unknown-linux-musleabihf/release/mi-camera-server`
(static ARMv7 binary).

## Deployment

Place the binary at `/mnt/sdcard/mi-camera-server/mi-camera-server`.
For autostart with the stock `perp` service manager, install:

- [rc.main](scripts/rc.main) at `/mnt/data/etc/perp/mi-camera-server/rc.main`.
- [rc.perp](scripts/rc.perp) at `/mnt/data/etc/perp/.boot/rc.perp` (back up the original).

Keep the scripts executable and the SD card inserted. Logs are written to
`/tmp/mi-camera-server.log`. To update the binary, stop the service, replace
it, and start the service again:

```sh
PERP_BASE=/etc/perp perpctl d mi-camera-server
# Replace the binary here.
PERP_BASE=/etc/perp perpctl u mi-camera-server
```

## Limitations

- No audio, ONVIF, or authentication. Restrict access to a trusted local network.
- Keep the stock `fetch_av`, `mortoxd`, and `miio_devicekit` processes running.
- The live test, [test-live.py](scripts/test-live.py), **physically moves the camera**.
