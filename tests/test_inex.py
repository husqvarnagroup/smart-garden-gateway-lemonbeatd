# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import pytest
import logging
import time

import lbtest
import json

logging.basicConfig(level=logging.INFO)


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
    logging.info(f"test_device_inclusion_exclusion: device_online_after_inclusion={device_online_after_inclusion}")
    lbtest.include_radiomodule(tcpserver, notify_socket)
    logging.info("test_device_inclusion_exclusion: radio module included")

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    logging.info("test_device_inclusion_exclusion: device created, including device")
    time.sleep(0.5)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )
    logging.info("test_device_inclusion_exclusion: device included, waiting before exclusion")
    time.sleep(0.5)  # Allow lemonbeatd to settle after inclusion

    time.sleep(0.5)
    lbtest.exclude_device(ipc_cmd_sock, dev0, device_online_after_inclusion, True)
    logging.info("test_device_inclusion_exclusion: device excluded")
    time.sleep(0.5)  # Allow state transitions to complete

    if not device_online_after_inclusion:
        logging.info("test_device_inclusion_exclusion: checking connection status change")
        lbtest.assert_connection_status_change(ipc_event_sock, False)
        time.sleep(0.1)

    logging.info("test_device_inclusion_exclusion: asserting exclusion")
    lbtest.assert_exclusion(ipc_event_sock, dev0)
    time.sleep(0.5)
    if device_online_after_inclusion:
        dev0.announce_devdesc()
        logging.info("test_device_inclusion_exclusion: asserting includable device after online exclusion")
        lbtest.assert_includable_device(ipc_event_sock, dev0, "update", False, False, 0)

    # This should fail now because the device shouldn't be found
    # TODO: test other requests first to make sure the request doesn't fail due
    #       to some other bug
    logging.info("test_device_inclusion_exclusion: testing double exclude (should fail)")
    lbtest.exclude_device(ipc_cmd_sock, dev0, False, False)
    logging.info("test_device_inclusion_exclusion: test complete")


# DIN004
def test_device_inclusion_lost_first_confirmation(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    logging.info("test_device_inclusion_lost_first_confirmation: starting")
    lbtest.include_radiomodule(tcpserver, notify_socket)
    time.sleep(0.1)  # Let radio module settle

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    logging.info("test_device_inclusion_lost_first_confirmation: including device with answer_inclusion=False")
    lbtest.include_device(
        tcpserver,
        ipc_cmd_sock,
        ipc_event_sock,
        notify_socket,
        socket_cleanup,
        dev0,
        answer_inclusion=False,
    )
    logging.info("test_device_inclusion_lost_first_confirmation: test complete")


# DIN006
def test_device_inclusion_failed_post_inclusion(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    logging.info("test_device_inclusion_failed_post_inclusion: starting")
    lbtest.include_radiomodule(tcpserver, notify_socket)
    time.sleep(0.1)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    logging.info("test_device_inclusion_failed_post_inclusion: announcing device")

    dev0.announce_devdesc()
    time.sleep(0.1)  # Let device announcement propagate

    logging.info("test_device_inclusion_failed_post_inclusion: asserting includable device")
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

    logging.info("test_device_inclusion_failed_post_inclusion: sending include command")
    ipc_cmd_sock.send(include_json)
    ipc_cmd_sock.recv()
    time.sleep(0.1)  # Let command propagate

    logging.info("test_device_inclusion_failed_post_inclusion: asserting inclusion started")
    id2 = lbtest.assert_includable_device(
        ipc_event_sock, dev0, "update", True, False, 0
    )
    assert id == id2

    logging.info("test_device_inclusion_failed_post_inclusion: checking nonce reset and inclusion")
    dev0.assert_device_nonce_reset(tcpserver)
    dev0.assert_inclusion()
    dev0.assert_meminfo_request(False)
    time.sleep(0.1)  # Let meminfo requests settle

    logging.info("test_device_inclusion_failed_post_inclusion: handling exclusion request (simulating failure)")
    dev0.assert_exclusion_request()
    dev0.send_exclusion_confirmation()
    time.sleep(0.1)  # Let exclusion complete

    logging.info("test_device_inclusion_failed_post_inclusion: asserting inclusion failed with error")
    lbtest.assert_includable_device(ipc_event_sock, dev0, "update", True, True, 1)
    time.sleep(0.2)  # Let state settle

    logging.info("test_device_inclusion_failed_post_inclusion: re-including device")
    dev0.dd.included = False
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )
    logging.info("test_device_inclusion_failed_post_inclusion: test complete")


# DIN007
def test_device_double_inclusion(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    logging.info("test_device_double_inclusion: starting")
    lbtest.include_radiomodule(tcpserver, notify_socket)
    time.sleep(0.1)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    logging.info("test_device_double_inclusion: including device with num_ipc_includes=2")
    lbtest.include_device(
        tcpserver,
        ipc_cmd_sock,
        ipc_event_sock,
        notify_socket,
        socket_cleanup,
        dev0,
        num_ipc_includes=2,
    )
    logging.info("test_device_double_inclusion: test complete")


def test_reinclusion_after_manual_factory_reset(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    logging.info("test_reinclusion_after_manual_factory_reset: starting")
    lbtest.include_radiomodule(tcpserver, notify_socket)
    time.sleep(0.1)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    logging.info("test_reinclusion_after_manual_factory_reset: initial inclusion")
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )
    time.sleep(0.2)  # Let inclusion complete

    # simulate device factory reset
    logging.info("test_reinclusion_after_manual_factory_reset: simulating device factory reset")
    dev0.dd.included = False
    dev0.announce_devdesc()
    time.sleep(0.1)  # Let announcement propagate

    logging.info("test_reinclusion_after_manual_factory_reset: asserting exclusion")
    lbtest.assert_exclusion(ipc_event_sock, dev0)
    time.sleep(0.1)

    # this is a second announcement that would only happen after 10s lemonbeatd
    # is currently unable to delete a device and create an includable device
    # with only one announcement.
    # TODO: remove this line once that's fixed
    logging.info("test_reinclusion_after_manual_factory_reset: sending second announcement")
    dev0.announce_devdesc()
    time.sleep(0.1)

    logging.info("test_reinclusion_after_manual_factory_reset: asserting includable device again")
    lbtest.assert_includable_device(ipc_event_sock, dev0, "update", False, False, 0)

    logging.info("test_reinclusion_after_manual_factory_reset: re-including device")
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )
    logging.info("test_reinclusion_after_manual_factory_reset: test complete")
