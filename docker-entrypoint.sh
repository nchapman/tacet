#!/bin/sh
set -eu

DATA_DIR="${TACET_DATA_DIR:-/data}"

if [ "$(id -u)" = "0" ]; then
  PUID="${PUID:-1000}"
  PGID="${PGID:-1000}"

  if [ -n "$DATA_DIR" ]; then
    mkdir -p "$DATA_DIR"
    chown -R "${PUID}:${PGID}" "$DATA_DIR"
  fi

  exec gosu "${PUID}:${PGID}" /usr/local/bin/tacet "$@"
fi

exec /usr/local/bin/tacet "$@"
