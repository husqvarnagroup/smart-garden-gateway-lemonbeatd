# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import lbtest
import time


def test_connection_check_offline_online(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    dev0.ipc_execute_resource(ipc_cmd_sock, "connection_status/0/check")
    dev0.assert_config_request(False)
    dev0.assert_config_request(False)
    dev0.assert_config_request(False)
    lbtest.wait_for_ipc_sock(ipc_cmd_sock)

    # DCS003: failed command brings device offline
    # DCS009: check offline device
    lbtest.assert_connection_status_change(ipc_event_sock, False)

    dev0.ipc_execute_resource(ipc_cmd_sock, "connection_status/0/check")
    dev0.assert_config_request(True)
    lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    # DCS001: successful command brings device online
    lbtest.assert_connection_status_change(ipc_event_sock, True)


def test_connection_status_messages(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    dev0.ipc_read_resource(ipc_cmd_sock, "connection_status/0/online")
    dev0.assert_ipc_resource_update(
        ipc_cmd_sock, "connection_status/0/online", "vb", True
    )

    lemonbeatd.stop()
    lemonbeatd.start()
    lbtest.assert_reinclude_radiomodule(tcpserver, notify_socket)
    ipc_cmd_sock = lemonbeatd.cmd_sock()

    # DCS005: online persisted over restart (no connection_status change published if device online)
    # DCS007: restart triggers ping
    dev0.assert_config_request()
    time.sleep(0.1)
    assert not dev0.has_pending_data()

    dev0.ipc_execute_resource(ipc_cmd_sock, "connection_status/0/check")
    dev0.assert_config_request(False)
    dev0.assert_config_request(False)
    dev0.assert_config_request(False)
    lbtest.wait_for_ipc_sock(ipc_cmd_sock)

    dev0.ipc_read_resource(ipc_cmd_sock, "connection_status/0/online")
    dev0.assert_ipc_resource_update(
        ipc_cmd_sock, "connection_status/0/online", "vb", False
    )

    lemonbeatd.stop()
    lemonbeatd.start()
    lbtest.assert_reinclude_radiomodule(tcpserver, notify_socket)
    ipc_event_sock = lemonbeatd.event_sock()

    # DCS004: offline persisted over restart (no connection_status change published if device offline)
    # DCS007: restart triggers ping
    dev0.assert_config_request(False)
    dev0.assert_config_request(False)
    dev0.assert_config_request(False)
    time.sleep(0.1)
    assert not dev0.has_pending_data()

    # DCS002: value update brings device online
    dev0.send_value_update("command")
    dev0.assert_ipc_resource_update(
        ipc_event_sock, "connection_status/0/online", "vb", True
    )
    dev0.assert_ipc_resource_update(ipc_event_sock, "lemonbeat/0")
