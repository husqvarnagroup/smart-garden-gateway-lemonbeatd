# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later
#
FROM python:3.14-slim-trixie

RUN apt update && \
    apt install --yes --no-install-recommends \
    dbus-daemon \
    inotify-tools \
    iproute2 \
    locales \
    python3 \
    python3-dev \
    python3-poetry && \
    rm -rf /var/lib/apt/lists/* && \
    apt clean
