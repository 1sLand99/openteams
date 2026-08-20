#!/bin/sh
# Fake npx: resolves npm package commands to repository-local fake binaries
# without contacting npm or the network. For offline member-scoped MCP E2E.
#
# Supported syntax:
#   npx [-y|--yes|--offline] [--package <spec>] <command> [args...]
#   npx [-y|--yes] <package-spec> [args...]          (npm shorthand)
#
# Known package specs are mapped to the local fake binaries installed on PATH:
#   @anthropic-ai/claude-code@*   -> claude
#   @openai/codex@*               -> codex
#   opencode-ai@*                 -> opencode
#   pi-acp@*                      -> pi-acp
#   @earendil-works/pi-coding-agent@* -> pi
#
# The fake never writes to any npm cache or reads a registry.

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

case "$CMD" in
  @anthropic-ai/claude-code@*)
    CMD="claude"
    ;;
  @openai/codex@*)
    CMD="codex"
    ;;
  opencode-ai@*)
    CMD="opencode"
    ;;
  @earendil-works/pi-coding-agent@*)
    CMD="pi"
    ;;
  *)
    # Strip an optional trailing @version from a bare package name.
    case "$CMD" in
      *@*)
        CMD="${CMD%%@*}"
        ;;
    esac
    ;;
esac

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
