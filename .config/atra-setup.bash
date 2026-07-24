#!/usr/bin/env bash
set -euo pipefail

"${ATRA_BINARY:-atra}" runner launch \
  --name host \
  --description "Run commands directly in the workspace host environment" \
  --approval ask
