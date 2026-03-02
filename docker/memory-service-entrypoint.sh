#!/bin/sh
set -eu

memory-migrator

memory-worker &
worker_pid=$!

memory-api &
api_pid=$!

stopping=0

stop_all() {
  kill -TERM "$api_pid" "$worker_pid" 2>/dev/null || true
}

on_signal() {
  stopping=1
  stop_all
}

trap on_signal INT TERM

while :; do
  [ "$stopping" -eq 1 ] && break

  api_alive=1
  worker_alive=1
  kill -0 "$api_pid" 2>/dev/null || api_alive=0
  kill -0 "$worker_pid" 2>/dev/null || worker_alive=0

  [ "$api_alive" -eq 1 ] && [ "$worker_alive" -eq 1 ] || break
  sleep 0.2
done

stop_all

set +e
wait "$api_pid"
api_status=$?
wait "$worker_pid"
worker_status=$?
set -e

if [ "$api_status" -ne 0 ]; then
  exit "$api_status"
fi

exit "$worker_status"
