#!/bin/sh
d=$(dirname "$0")
case $(uname -m) in
  x86_64|amd64) a=x86_64 ;;
  aarch64|arm64) a=aarch64 ;;
  *) echo "imagegen-cc: unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac
exec "$d/imagegen-cc-$a" "$@"
