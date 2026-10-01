#!/bin/bash
# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

SCRIPT_DIR=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )
WORKING_DIR="${SCRIPT_DIR}/.."

if [ ! -f /.dockerenv ]; then
    echo This script is intended to be run inside a docker container
    exit 1
fi 

# we need dbus for systemd-timesyncd emulation
if [ -f '/tmp/lbtest_dbus' ]; then
    rm /tmp/lbtest_dbus
fi

dbus-daemon --session --nopidfile --nofork --address unix:path=/tmp/lbtest_dbus &

# Source: https://unix.stackexchange.com/a/185370
while read -r i; do if [ "$i" = "lbtest_dbus" ]; then break; fi; done \
    < <(inotifywait  -e create,open --format '%f' --quiet /tmp --monitor)

# to make it use our emulated systemd-timesyncd
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/tmp/lbtest_dbus
# so we can change it's timezone
export TZ=/tmp/lbtest_localtime


cd "${WORKING_DIR}"

poetry install

timeout -s INT 10m poetry run pytest -s --log-cli-level=DEBUG --full-trace --junit-xml="test_results_component.xml" --reruns=2
