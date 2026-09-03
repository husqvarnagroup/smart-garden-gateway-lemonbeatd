#!/bin/sh

# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

# Disable router solicitations is recommended
echo 0 > "/proc/sys/net/ipv6/conf/${1}/router_solicitations"

# Setting MTU is recommended
ip link set "${1}" mtu 2500

ip addr add fc00::6:100:0:0/64 dev "${1}"
