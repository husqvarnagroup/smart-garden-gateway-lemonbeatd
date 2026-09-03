# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import base64
import json
import os

import pytest

import lbtest


# REP001
def test_value_update(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    dev0.value("command").set(10)
    dev0.send_value_update("command")

    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    dev0.assert_ipc_event_generic(event)
    # NOTE: there should be no trailing `/0`
    assert event["entity"]["path"] == "lemonbeat/0"

    payload = event["payload"]
    assert len(payload) == 2
    assert payload["_urn"] == "urn:oma:lwm2m:x:31000"
    assert (
        lbtest.get_ipc_value_raw(payload[f"{dev0.values[3].name}"], "vi")
        == dev0.values[3].value
    )


def test_value_update_burst(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    # TODO: increase to 5000 and ensure that it still passes
    number_of_packets = 50

    for num in range(number_of_packets):
        dev0.value("command").set(num)
        dev0.send_value_update("command")

    for num in range(number_of_packets):
        event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
        dev0.assert_ipc_event_generic(event)
        # NOTE: there should be no trailing `/0`
        assert event["entity"]["path"] == "lemonbeat/0"

        payload = event["payload"]
        assert len(payload) == 2
        assert payload["_urn"] == "urn:oma:lwm2m:x:31000"
        assert lbtest.get_ipc_value_raw(payload[f"{dev0.values[3].name}"], "vi") == num


def test_multi_value_update(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    dev0.value("rf_link_quality").set(10)
    dev0.value("error").set(11)
    dev0.send_value_updates(["rf_link_quality", "error"])

    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    dev0.assert_ipc_event_generic(event)
    assert event["entity"]["path"] == "lemonbeat/0"
    payload = event["payload"]
    assert len(payload) == 3
    assert payload["_urn"] == "urn:oma:lwm2m:x:31000"

    assert (
        lbtest.get_ipc_value_raw(payload["rf_link_quality"], "vi")
        == dev0.values[0].value
    )
    assert lbtest.get_ipc_value_raw(payload["error"], "vi") == dev0.values[2].value


# REP003
@pytest.mark.parametrize(
    "value_name,value_type", [("command", "vi"), ("threshold", "vf")]
)
def test_uninitialised_value_update(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, value_name, value_type
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    dev0.value(value_name).set("NaN")
    dev0.send_value_update(value_name)

    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    dev0.assert_ipc_event_generic(event)
    # NOTE: there should be no trailing `/0`
    assert event["entity"]["path"] == "lemonbeat/0"

    payload = event["payload"]
    assert len(payload) == 2
    assert payload["_urn"] == "urn:oma:lwm2m:x:31000"
    assert lbtest.get_ipc_value_raw(payload[value_name], value_type) is None


def test_malformed_request(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()

    request_json = json.dumps(
        [
            {
                "op": "malformed",
            }
        ]
    ).encode()

    ipc_cmd_sock.send(request_json)
    response = json.loads(ipc_cmd_sock.recv())
    assert not response["success"]
    assert response["payload"]["vs"] == "could not parse request"
    assert response["metadata"]["error_source"] == "lemonbeatd"


def test_zero_maxlength_binary(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    dev0.value("data_download_int").max_length = 0
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    data_raw = os.urandom(20)
    data_b64 = base64.b64encode(data_raw).decode("UTF-8")

    request_json = json.dumps(
        [
            {
                "op": "write",
                "entity": {
                    "device": f"{dev0.identifier()}",
                    "path": "lemonbeat/0/data_download_int/0",
                },
                "payload": {
                    "vo": f"{data_b64}",
                },
            }
        ]
    ).encode()

    ipc_cmd_sock.send(request_json)

    dev0.assert_val_set("data_download_int", data_raw)
    dev0.value("data_download_int").set(data_raw)
    dev0.send_value_update("data_download_int")

    response = json.loads(ipc_cmd_sock.recv())
    assert len(response) == 1
    response = response[0]
    assert response["success"]

    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    dev0.assert_ipc_event_generic(event)
    assert event["entity"]["path"] == "lemonbeat/0"

    payload = event["payload"]
    assert len(payload) == 2
    assert payload["_urn"] == "urn:oma:lwm2m:x:31000"
    assert lbtest.get_ipc_value_raw(payload["data_download_int"], "vo") == data_b64


def test_status_update(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    dev0.ipc_read_resource(ipc_cmd_sock, "lemonbeat_status_message/0")

    # we never sent a status, the object shouldn't exist yet
    response = json.loads(ipc_cmd_sock.recv())
    assert len(response) == 1
    response = response[0]
    assert not response["success"]

    dev0.send_status(1, 101, 11)

    event = lbtest.wait_for_ipc_sock_event(ipc_event_sock)
    dev0.assert_ipc_event_generic(event)
    assert event["entity"]["path"] == "lemonbeat_status_message/0"

    payload = event["payload"]
    assert len(payload) == 5
    assert payload["_urn"] == "urn:oma:lwm2m:x:28173"
    assert lbtest.get_ipc_value_raw(payload["level"], "vi") == 1
    assert lbtest.get_ipc_value_raw(payload["type"], "vi") == 101
    assert lbtest.get_ipc_value_raw(payload["code"], "vi") == 11
    assert lbtest.get_ipc_value_raw(payload["data"], "vs") == ""


def test_write_multi_value(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    data_raw = os.urandom(2)
    data_b64 = base64.b64encode(data_raw).decode("UTF-8")

    request_json = json.dumps(
        [
            {
                "op": "write",
                "entity": {
                    "device": f"{dev0.identifier()}",
                    "path": "lemonbeat/0",
                },
                "payload": {
                    "data_download_int": {
                        "vo": f"{data_b64}",
                    },
                    "command": {
                        "vi": 11,
                    },
                    "power_timer": {
                        "vi": 42,
                    },
                },
            }
        ]
    ).encode()

    ipc_cmd_sock.send(request_json)

    dev0.assert_values_set(
        {
            "data_download_int": data_raw,
            "command": 11.0,
            "power_timer": 42.0,
        }
    )

    response = json.loads(ipc_cmd_sock.recv())
    assert len(response) == 1
    response = response[0]
    assert response["success"]


def test_get_all_devices(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    request_json = json.dumps(
        [
            {
                "op": "read",
                "entity": {
                    "service": "lemonbeatd",
                    "path": "devices",
                },
            }
        ]
    ).encode()
    ipc_cmd_sock.send(request_json)

    response = json.loads(ipc_cmd_sock.recv())
    assert len(response) == 1

    response = response[0]
    assert response["success"]

    payload = response["payload"]
    assert len(payload) == 1

    endpoint = payload[dev0.identifier()]
    lbtest.assert_ipc_endpoint_payload(dev0, endpoint, utc_offset="UTC+00:00")


def test_repair(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    request_json = json.dumps(
        [
            {
                "op": "update",
                "entity": {
                    "service": "lemonbeatd",
                    "path": f"device/{dev0.identifier()}/repair",
                },
            }
        ]
    ).encode()
    ipc_cmd_sock.send(request_json)

    dev0.assert_meminfo_request()
    dev0.assert_valdesc_requests()
    dev0.assert_val_requests()

    response = json.loads(ipc_cmd_sock.recv())
    assert len(response) == 1

    response = response[0]
    assert response["success"]
