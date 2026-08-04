#!/bin/sh
# Fake npx: resolves commands from local bin directories without contacting npm.
# Intended for offline Pi ACP fixture testing only.
#
# Supported syntax:
#   npx [--yes|-y] [--offline] [--package <spec>]... <command> [args...]
#
# The fake npx strips all flags, finds the first non-flag argument as the
# command, and searches PATH for an executable with that name. It never
# contacts any npm registry or cache.

CMD=""
ARGS=""
SKIP_NEXT=false
FOUND_CMD=false

for arg in "$@"; do
  if $SKIP_NEXT; then
    SKIP_NEXT=false
    continue
  fi
  case "$arg" in
    --package)
      SKIP_NEXT=true
      continue
      ;;
    --package=*)
      continue
      ;;
    --yes|-y|--offline)
      continue
      ;;
    --)
      continue
      ;;
    -*)
      if $FOUND_CMD; then
        ARGS="$ARGS $arg"
      fi
      continue
      ;;
    *)
      if ! $FOUND_CMD; then
        CMD="$arg"
        FOUND_CMD=true
      else
        ARGS="$ARGS $arg"
      fi
      ;;
  esac
done

if [ -z "$CMD" ]; then
  echo "fake-npx: no command specified" >&2
  exit 1
fi

SAVE_IFS="$IFS"
IFS=":"
for dir in $PATH; do
  if [ -x "$dir/$CMD" ]; then
    IFS="$SAVE_IFS"
    exec "$dir/$CMD" $ARGS
  fi
done
IFS="$SAVE_IFS"

echo "fake-npx: command not found: $CMD" >&2
exit 1
