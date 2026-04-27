# qconnect

Standalone Qobuz Connect CLI client built on the `qconnect-*` Rust crates from
[`vicrodh/qbz`](https://github.com/vicrodh/qbz).

## Credentials

The easiest setup is browser OAuth:

```sh
qconnect login
```

This opens Qobuz in your browser, captures the localhost callback, and saves the
discovered app id plus `QOBUZ_USER_AUTH_TOKEN` under your qconnect config
directory for later commands. Use `qconnect login --print-token` if you need the
raw token for another environment.

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
