#!/bin/sh

# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

pppd local crtscts nodetach debug /dev/bnw-gateway-dongle-ppp +ipv6 noauth noip
