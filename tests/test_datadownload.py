# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import base64
import os

import lbtest


# LUP001
def test_data_download(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_cbtl(ppp, socket_cleanup, lbtest.IFADDR_DEV0)

    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    firmware = lbtest.FirmwareUpdate(os.urandom(10), None, 256)

    lbtest.ipc_ddl_init(
        dev0.identifier(), ipc_cmd_sock, 258, firmware.image, 0xAABBCCDD
    )

    # lemonbeatd must provide metadata to the device
    data_download_int = [0x00, 0x00, 0x01, 0x02, 0xAA, 0xBB, 0xCC, 0xDD]
    dev0.assert_val_set("data_download_int", data_download_int)
    dev0.value("data_download_int").set(data_download_int)
    dev0.send_value_update("data_download_int")

    # firmware init and verification
    dev0.assert_firmware_init(firmware, id=258)
    dev0.assert_firmware_get_info(firmware, 0)

    # assert response to the IPC request
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is True

    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    dev0.assert_ipc_event_generic(event)
    assert event["entity"]["path"] == "data_download/0"

    payload = event["payload"]
    len(payload) == 4
    assert lbtest.get_ipc_value_raw(payload["status"], "vi") == 1
    assert lbtest.get_ipc_value_raw(payload["slot"], "vi") == 258
    assert lbtest.get_ipc_value_raw(payload["checksum"], "vi") == firmware.checksum
    assert lbtest.get_ipc_value_raw(payload["content_tag"], "vi") == 0xAABBCCDD

    # this value update was triggered during the IPC call
    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    dev0.assert_ipc_event_generic(event)
    assert event["entity"]["path"] == "lemonbeat/0"
    payload = event["payload"]
    assert (
        lbtest.get_ipc_value_raw(payload["data_download_int"], "vo")
        == base64.b64encode(bytes(data_download_int)).decode()
    )

    # actual upload
    dev0.assert_firmware_data(firmware)

    dev0.assert_ddl_status_event(ipc_event_sock, 2)

    # activation
    dev0.assert_firmware_update_start_with_status(firmware, 1, excludes_device=False)

    dev0.assert_ddl_status_event(ipc_event_sock, 3)
