#!/bin/sh
set -eu
export ZIG_GLOBAL_CACHE_DIR="${TMPDIR:-/tmp}/mi-camera-zig-global"
export ZIG_LOCAL_CACHE_DIR="${TMPDIR:-/tmp}/mi-camera-zig-local"
exec zig cc -target arm-linux-musleabihf "$@"
