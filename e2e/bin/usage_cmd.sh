#!/usr/bin/env bash
# CSM_USAGE_CMD target for the e2e harness: cats whatever fixture
# CSM_USAGE_FIXTURE currently points at. Never touches the network.
set -euo pipefail
cat "$CSM_USAGE_FIXTURE"
