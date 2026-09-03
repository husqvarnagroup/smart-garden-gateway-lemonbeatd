# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import lbtest


## DIN002
def test_dd_triggers_device_object_update(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    dev0.dd.application_version = "2.4.1"
    dev0.dd.bootloader_version = "4.0.0"
    dev0.dd.stack_version = "1.5.3"
    dev0.dd.hardware_version = "0.3.5"
    dev0.dd.manufacturer = 3
    dev0.dd.product = 2
    dev0.dd.type = 1
    dev0.dd.sgtin = b"\x30\x34\xF8\xEE\x90\x1E\xE9\x40\x00\x00\x87\x71"

    # this brings it back online
    dev0.announce_devdesc()

    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    assert event["payload"]["software_version"]["vs"] == dev0.dd.application_version
    assert (
        event["payload"]["firmware_version"]["vs"]
        == f"{dev0.dd.bootloader_version}-{dev0.dd.stack_version}"
    )
    assert event["payload"]["hardware_version"]["vs"] == dev0.dd.hardware_version
    assert event["payload"]["manufacturer"]["vs"] == "Gardena"

    model, serial, device_type = lbtest.sgtin_decompose(dev0.dd.sgtin)
    assert event["payload"]["device_type"]["vs"] == device_type
    assert event["payload"]["serial_number"]["vs"] == serial.zfill(8)
    assert event["payload"]["model_number"]["vs"] == model

    assert event["entity"]["path"] == "device/0"
