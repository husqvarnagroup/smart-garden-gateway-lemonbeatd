# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import lbtest
import json
import logging

logging.basicConfig(level=logging.INFO)


def test_wakeup(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    logging.info("test_wakeup: starting")
    lbtest.include_radiomodule(tcpserver, notify_socket)
    logging.info("test_wakeup: radio module included")

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    dev0.dd.radio_mode = 1
    logging.info("test_wakeup: device created, radio_mode=1")

    logging.info("test_wakeup: including device...")
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )
    logging.info("test_wakeup: device included")

    request_json = json.dumps(
        [
            {
                "op": "write",
                "entity": {
                    "device": f"{dev0.identifier()}",
                    "path": "lemonbeat/0/command",
                },
                "payload": {
                    "vi": 11,
                },
            }
        ]
    ).encode()

    logging.info("test_wakeup: first command cycle starting")
    ipc_cmd_sock.send(request_json)

    duration = 20000
    duration_raw = duration.to_bytes(4, "little")

    channel = dev0.dd.wakeup_channel
    channel_raw = channel.to_bytes(1, "little")

    logging.info("test_wakeup: sending command to tcpserver")
    tcpserver.handle_command(
        0x03, 11, duration_raw + b"\x6D\xFF\xFE\x6F\x00\x01" + channel_raw
    )
    logging.info("test_wakeup: sending status")
    dev0.send_status(1, 101, 12)
    logging.info("test_wakeup: asserting val_set for first cycle")
    dev0.assert_val_set("command", 11.0)
    logging.info("test_wakeup: first command cycle complete")

    logging.info("test_wakeup: second command cycle starting")
    ipc_cmd_sock.send(request_json)
    tcpserver.handle_command(
        0x03, 11, duration_raw + b"\x6D\xFF\xFE\x6F\x00\x01" + channel_raw
    )
    dev0.send_status(1, 101, 12)
    logging.info("test_wakeup: asserting val_set for second cycle")
    dev0.assert_val_set("command", 11.0)
    logging.info("test_wakeup: second command cycle complete")
