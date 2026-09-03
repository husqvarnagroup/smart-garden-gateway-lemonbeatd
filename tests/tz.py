# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import os

TZFILE = "/tmp/lbtest_localtime"


def set_timezone(name):
    if os.path.islink(TZFILE):
        os.unlink(TZFILE)

    os.symlink(f"/usr/share/zoneinfo/{name}", TZFILE)
