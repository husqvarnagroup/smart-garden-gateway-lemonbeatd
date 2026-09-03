# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import lbtest
import json


def test_wakeup(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    dev0.dd.radio_mode = 1
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

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
    ipc_cmd_sock.send(request_json)

    duration = 20000
    duration_raw = duration.to_bytes(4, "little")

    channel = dev0.dd.wakeup_channel
    channel_raw = channel.to_bytes(1, "little")

    tcpserver.handle_command(
        0x03, 11, duration_raw + b"\x6D\xFF\xFE\x6F\x00\x01" + channel_raw
    )
    dev0.send_status(1, 101, 12)
    dev0.assert_val_set("command", 11.0)

    ipc_cmd_sock.send(request_json)
    tcpserver.handle_command(
        0x03, 11, duration_raw + b"\x6D\xFF\xFE\x6F\x00\x01" + channel_raw
    )
    dev0.send_status(1, 101, 12)
    dev0.assert_val_set("command", 11.0)
