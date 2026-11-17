#!/bin/sh
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
# install.sh: builds xcomm, links this directory as a herdr plugin and
# starts its hotplug watcher. Undo with `herdr plugin unlink
# cantona.herdr-xcomm` (and `xcomm watch stop`).
set -eu
CDPATH='' cd -- "$(dirname -- "$0")/.."
sh scripts/build.sh
herdr plugin link "$(pwd)"
herdr plugin action invoke --plugin cantona.herdr-xcomm watch-start
