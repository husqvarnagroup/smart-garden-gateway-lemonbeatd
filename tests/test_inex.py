# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import pytest

import lbtest
import json


# DIN001, DIN002
@pytest.mark.parametrize("device_online_after_inclusion", [False, True])
def test_device_inclusion_exclusion(
    ppp,
    tcpserver,
    lemonbeatd,
    notify_socket,
    socket_cleanup,
    device_online_after_inclusion,
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )
    lbtest.exclude_device(ipc_cmd_sock, dev0, device_online_after_inclusion, True)

    if not device_online_after_inclusion:
        lbtest.assert_connection_status_change(ipc_event_sock, False)

    lbtest.assert_exclusion(ipc_event_sock, dev0)
    if device_online_after_inclusion:
        lbtest.assert_includable_device(ipc_event_sock, dev0, "update", False, False, 0)

    # This should fail now because the device shouldn't be found
    # TODO: test other requests first to make sure the request doesn't fail due
    #       to some other bug
    lbtest.exclude_device(ipc_cmd_sock, dev0, False, False)


# DIN004
def test_device_inclusion_lost_first_confirmation(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver,
        ipc_cmd_sock,
        ipc_event_sock,
        notify_socket,
        socket_cleanup,
        dev0,
        answer_inclusion=False,
    )


# DIN006
def test_device_inclusion_failed_post_inclusion(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)

    dev0.announce_devdesc()
    id = lbtest.assert_includable_device(
        ipc_event_sock, dev0, "update", False, False, 0
    )
    path = f"includable_device/{id}/include"
    include_json = json.dumps(
        [
            {
                "op": "update",
                "entity": {
                    "path": path,
                    "service": "lemonbeatd",
                },
            }
        ]
    ).encode()

    ipc_cmd_sock.send(include_json)
    ipc_cmd_sock.recv()

    id2 = lbtest.assert_includable_device(
        ipc_event_sock, dev0, "update", True, False, 0
    )
    assert id == id2

    dev0.assert_device_nonce_reset(tcpserver)
    dev0.assert_inclusion()
    dev0.assert_meminfo_request(False)

    dev0.assert_exclusion_request()
    dev0.send_exclusion_confirmation()

    lbtest.assert_includable_device(ipc_event_sock, dev0, "update", True, True, 1)

    dev0.dd.included = False
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )


# DIN007
def test_device_double_inclusion(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver,
        ipc_cmd_sock,
        ipc_event_sock,
        notify_socket,
        socket_cleanup,
        dev0,
        num_ipc_includes=2,
    )


def test_reinclusion_after_manual_factory_reset(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    # simulate device factory reset
    dev0.dd.included = False
    dev0.announce_devdesc()

    lbtest.assert_exclusion(ipc_event_sock, dev0)

    # this is a second announcement that would only happen after 10s lemonbeatd
    # is currently unable to delete a device and create an includable device
    # with only one announcement.
    # TODO: remove this line once that's fixed
    dev0.announce_devdesc()

    lbtest.assert_includable_device(ipc_event_sock, dev0, "update", False, False, 0)

    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )
