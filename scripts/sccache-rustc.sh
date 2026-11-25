#!/usr/bin/env bash
# Rustc wrapper used by `mk` tasks: route compiles through sccache when it is
# installed, and otherwise invoke rustc directly. Set EE_DISABLE_SCCACHE=1 to
# bypass sccache for one run.
set -euo pipefail

if [ "${EE_DISABLE_SCCACHE:-0}" != "1" ] && command -v sccache >/dev/null 2>&1; then
    exec sccache "$@"
fi

exec "$@"
