# qconnect

Standalone Qobuz Connect CLI client built on the `qconnect-*` Rust crates from
[`vicrodh/qbz`](https://github.com/vicrodh/qbz).

## Credentials

The CLI can connect in either of these ways:

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

This makes the process visible to other Qobuz Connect controllers and prints
remote renderer commands. It reports synthetic playback state but does not play
audio.

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
