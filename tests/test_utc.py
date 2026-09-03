# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

# TODO:
# - test that during setting the utc offset, if the final get-timezone fails,
#   the device gets flagged as offline, and after a value update it goes back
#   online and immediately tries to set the offset again.

import time

import pytest

import lbtest
import tz

import pytz
from datetime import datetime, timedelta


## UTC002
@pytest.mark.parametrize("drop_status", [True, False])
def test_update_event(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, dbussvc, drop_status
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    # Previously we used 'US/Central' here, which was a symlink to 'America/Chicago'.
    # That symlink is not installed by default on Ubuntu 24.4 anymore.
    # https://askubuntu.com/a/1528517
    tz.set_timezone("America/Chicago")
    dbussvc.timedate.PropertiesChanged()
    utcoffset = datetime.now(tz=pytz.timezone("America/Chicago")).utcoffset()
    # utcoffset also returns positive values for tz west of gmt
    expected_offset = utcoffset.seconds - 24 * 60 * 60
    dev0.assert_utc_update(expected_offset, drop_status)

    lbtest.assert_utc_offset_change(ipc_event_sock, _format_offset(utcoffset))

    # give lemonbeatd time to handle the answer and write the device description
    time.sleep(0.1)

    # UTC008: verify that lemonbeatd doesn't set the timezone if it didn't change
    lemonbeatd.stop()
    lemonbeatd.start()
    lbtest.assert_reinclude_radiomodule(tcpserver, notify_socket)
    dev0.assert_config_request()


## UTC003
@pytest.mark.timeout(20)
def test_set_when_online(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, dbussvc
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    lemonbeatd.stop()
    lemonbeatd.start()
    lbtest.assert_reinclude_radiomodule(tcpserver, notify_socket)

    # this should get dev0 offline because we don't answer to the ping
    # we expect 3 attempts
    dev0.assert_config_request(answer=False)
    dev0.assert_config_request(answer=False)
    dev0.assert_config_request(answer=False)
    time.sleep(6)

    tz.set_timezone("Europe/Berlin")
    dbussvc.timedate.PropertiesChanged()

    # the device is offline, we don't expect an UTC update
    time.sleep(0.1)
    assert not dev0.has_pending_data()

    ipc_event_sock = lemonbeatd.event_sock()

    # this brings it back online
    dev0.announce_devdesc()

    utcoffset = datetime.now(tz=pytz.timezone("Europe/Berlin")).utcoffset()
    dev0.assert_utc_update(utcoffset.seconds)

    # Lemonbeatd should send out three IPC events (utc update, connection status change, device update) but
    # order is not deterministic
    for i in range(3):
        event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
        assert event["entity"]["path"] in [
            "connection_status/0/online",
            "device/0/utc_offset",
            "device/0",
        ]


## UTC004
def test_update_event_change_while_stopped(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    lemonbeatd.stop()
    tz.set_timezone("Europe/Berlin")
    lemonbeatd.start()
    lbtest.assert_reinclude_radiomodule(tcpserver, notify_socket)

    dev0.assert_config_request()

    ipc_event_sock = lemonbeatd.event_sock()
    utcoffset = datetime.now(tz=pytz.timezone("Europe/Berlin")).utcoffset()
    dev0.assert_utc_update(utcoffset.seconds)

    lbtest.assert_utc_offset_change(ipc_event_sock, _format_offset(utcoffset))


## UTC006
def test_update_event_same_offset(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, dbussvc
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    tz.set_timezone("Europe/Berlin")
    dbussvc.timedate.PropertiesChanged()
    utcoffset = datetime.now(tz=pytz.timezone("Europe/Berlin")).utcoffset()
    dev0.assert_utc_update(utcoffset.seconds)

    lbtest.assert_utc_offset_change(ipc_event_sock, _format_offset(utcoffset))

    tz.set_timezone("Europe/Zurich")
    dbussvc.timedate.PropertiesChanged()


# copied from datetime - not a public method
def _format_offset(utcoffset):
    s = ""
    if utcoffset is not None:
        if utcoffset.days < 0:
            sign = "-"
            utcoffset = -utcoffset
        else:
            sign = "+"
        hh, mm = divmod(utcoffset, timedelta(hours=1))
        mm, ss = divmod(mm, timedelta(minutes=1))
        s += "UTC%s%02d:%02d" % (sign, hh, mm)
    return s
