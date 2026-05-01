# qconnect

Standalone Qobuz Connect CLI client built on the `qconnect-*` Rust crates from
[`vicrodh/qbz`](https://github.com/vicrodh/qbz).

## Install

```sh
cargo install --git https://github.com/ahcm/qconnect
```

## Linux Audio Builds

For local audio playback on Linux, prefer a normal glibc/dynamically linked
build made on a compatible distro:

```sh
cargo build --release
```

Avoid shipping Alpine/musl/static binaries as general-purpose audio builds.
ALSA often resolves the default device through runtime-loaded modules such as
PulseAudio/PipeWire config plugins. Static musl builds can link `libasound` but
still fail at runtime with messages like `Dynamic loading not supported` or
`Unknown PCM default` when those ALSA modules cannot be loaded on the target
system.

Musl/static builds are still reasonable for state/control use:

```sh
qconnect serve --no-audio
```

For audio on a target host, use `qconnect audio-devices` and run a glibc build
with a listed backend/device id when needed.

## Credentials

The easiest setup on a desktop machine is browser OAuth:

```sh
qconnect login
```

This opens Qobuz in your browser, captures the localhost callback, and saves the
discovered app id plus `QOBUZ_USER_AUTH_TOKEN` under your qconnect config
directory for later commands. Use `qconnect login --print-token` if you need the
raw token for another environment.

On a headless machine, paste credentials directly:

```sh
qconnect login --manual
```

or use noninteractive arguments:

```sh
qconnect login --app-id '...' --user-auth-token '...'
```

The CLI can also connect with explicit environment variables:

```sh
export QOBUZ_APP_ID=...
export QOBUZ_USER_AUTH_TOKEN=...
qconnect status
```

or with already-created Qobuz Connect websocket credentials:

```sh
qconnect --endpoint 'wss://...' --jwt-qws '...' status
```

`QOBUZ_APP_ID` and `QOBUZ_USER_AUTH_TOKEN` are used to call
`https://www.qobuz.com/api.json/0.2/qws/createToken`, which returns the Connect
websocket endpoint and JWT.

## Commands

Run as a headless Qobuz Connect renderer:

```sh
qconnect --device-name 'qconnect cli' serve
```

This makes the process visible to other Qobuz Connect controllers and plays
audio locally. Use `serve --no-audio` for state-only rendering.

List Linux audio backend/device ids:

```sh
qconnect audio-devices
qconnect audio-devices --audio-backend alsa
```

Select a backend/device explicitly:

```sh
qconnect serve --audio-backend alsa --audio-device hw:0,0
```

Inspect state:

```sh
qconnect status
qconnect queue --wait-secs 8
```

Control the active renderer:

```sh
qconnect load 123456 234567 --start-index 0
qconnect add 345678
qconnect play
qconnect pause
qconnect volume --set 65
qconnect shuffle on
qconnect repeat all
```

Use `--json` for machine-readable event output.
