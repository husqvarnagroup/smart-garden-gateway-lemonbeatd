# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import base64
import enum
import io
import ipaddress
import json
import logging
import math
import os
import re
import selectors
import shutil
import socket
import subprocess
import threading
import time
import uuid
import xml
from scapy.all import IPv6, UDP, Raw

import crc
import pytest
from lemonbeat import lsdl_serializer

IFADDR_BASE = "fc00::6:6dff:fe6f:"
IFADDR_DEV0 = IFADDR_BASE + "1"
ADDR_MULTICAST = "ff02::1"
PPP_ADDR_LL_ZEPHYR = "fe80::1"
PPP_ADDR_LL_LINUX = "fe80::2"
PPP_ADDR_DEFAULT_GATEWAY = "fc00::6:100:0:0"

TESTDIR = os.path.dirname(os.path.realpath(__file__))
WORKDIR = os.path.join(TESTDIR, "work")

CRC16_XMODEM = crc.Configuration(
    width=16,
    polynomial=0x1021,
    init_value=0x0000,
    final_xor_value=0x0000,
    reverse_input=False,
    reverse_output=False,
)
CRC16CALC = crc.CrcCalculator(CRC16_XMODEM, True)


class NotFoundError(Exception):
    pass


def find(list, callback):
    for item in list:
        if callback(item):
            return item

    raise NotFoundError


# Source: https://stackoverflow.com/a/434328
def chunker(seq, size):
    return (seq[pos : pos + size] for pos in range(0, len(seq), size))


def logthread_entry(pipe):
    logger = logging.getLogger("lemonbeatd")

    for line in io.TextIOWrapper(pipe, encoding="utf-8"):
        if len(line) > 0 and line[-1] == "\n":
            line = line[:-1]

        logger.debug(line)


class UnixSocket:
    def __init__(self, path, selector):
        self.raw_socket = socket.socket(
            family=socket.AF_UNIX, type=socket.SOCK_STREAM, proto=0
        )
        self.raw_socket.setblocking(True)
        self._path = path
        self.raw_socket.connect(self._path)
        selector.register(self.raw_socket, selectors.EVENT_READ, None)
        self._receive_buffer = b""

    def __del__(self):
        assert self._receive_buffer == b"", "Unhandled data from socket (%s): %s" % (
            self._path,
            self._receive_buffer,
        )
        self.raw_socket.close()

    def send(self, data):
        self.raw_socket.send(data + b"\n")

    def recv(self):
        return self.raw_socket.recv(1024 * 1024 * 16)

    def recv_packet(self):
        # inspired by:
        # https://stackoverflow.com/a/67826680
        separator = b"\n"
        while separator not in self._receive_buffer:
            with selectors.DefaultSelector() as selector:
                selector.register(self.raw_socket, selectors.EVENT_READ, None)
                events = selector.select()
                assert len(events) == 1

                data = self.recv()
                logging.debug("Raw data on %s: %s", self._path, data)
                self._receive_buffer += data
        line, _sep, self._receive_buffer = self._receive_buffer.partition(separator)
        return line


class Lemonbeatd:
    def __init__(self, tcpserver):
        self.process = None
        self.ipc_cmd_sock = None
        self.ipc_event_sock = None
        self.selector = selectors.DefaultSelector()
        self.tcpserver = tcpserver

    def __del__(self):
        self.try_stop()

    def remove_workdir(self):
        if os.path.exists(WORKDIR):
            shutil.rmtree(WORKDIR)

    def start(self):
        logging.info("start lemonbeatd")

        env = os.environ.copy()
        # for extensive logging
        env[
            "RUST_LOG"
        ] = "info,sg-ipc=trace,lemonbeatd=trace,lsdl=trace,lwm2m=trace,systemd-async=trace,tokio-task-rpc=trace"
        # to test systemd ready signalling
        env["NOTIFY_SOCKET"] = "/tmp/lemonbeat_test.notify"

        self.process = subprocess.Popen(
            [
                os.path.join(TESTDIR, "../target/debug/lemonbeatd"),
                "--mac-address",
                "aa:bb:cc:dd:ee:ff",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            env=env,
        )

        self.logthread = threading.Thread(
            target=logthread_entry, args=[self.process.stdout]
        )
        self.logthread.start()

    def cmd_sock(self):
        if self.ipc_cmd_sock is not None:
            return self.ipc_cmd_sock

        if self.selector is None:
            self.selector = selectors.DefaultSelector()

        self.ipc_cmd_sock = UnixSocket("/tmp/lemonbeatd-command.ipc", self.selector)
        return self.ipc_cmd_sock

    def event_sock(self):
        if self.ipc_event_sock is not None:
            return self.ipc_event_sock

        if self.selector is None:
            self.selector = selectors.DefaultSelector()

        self.ipc_event_sock = UnixSocket("/tmp/lemonbeatd-event.ipc", self.selector)
        return self.ipc_event_sock

    def try_stop(self):
        if self.process is not None:
            self.stop()
        else:
            assert self.ipc_cmd_sock is None
            assert self.ipc_event_sock is None
            assert self.selector is None

    def stop(self):
        logging.info("stop lemonbeatd")

        if (self.ipc_cmd_sock is not None) or (self.ipc_event_sock is not None):
            time.sleep(0.1)
        has_pending = self.has_pending_data()

        if self.ipc_cmd_sock is not None:
            self.selector.unregister(self.ipc_cmd_sock.raw_socket)
            self.ipc_cmd_sock = None
        if self.ipc_event_sock is not None:
            self.selector.unregister(self.ipc_event_sock.raw_socket)
            self.ipc_event_sock = None

        # os.kill(self.process.pid, signal.SIGKILL)
        self.process.kill()
        res = self.process.wait()
        logging.info(f"lemonbeatd result = {res}")
        self.process = None

        if self.logthread.is_alive():
            self.logthread.join()
        self.logthread = None

        if self.selector:
            self.selector.close()
            self.selector = None

        assert not has_pending

        if self.tcpserver.has_connection():
            self.tcpserver.wait_for_disconnect()

    def has_pending_data(self):
        if self.selector is None:
            return False

        has_pending = False

        # the goal here is to receive and print all pending IPC events in case
        # there's more than one
        check_again = True
        while check_again:
            check_again = False

            ready_list = self.selector.select(0)
            for key, events in ready_list:
                if (
                    self.ipc_event_sock
                    and key.fd == self.ipc_event_sock.raw_socket.fileno()
                ):
                    if events & selectors.EVENT_READ:
                        data = self.ipc_event_sock.raw_socket.recv(1024 * 16 * 16)

                        try:
                            data_pretty = ""
                            for d in data.strip().split(b"\n"):
                                d = d.decode()
                                data_parsed = json.loads(d.strip())
                                data_pretty += (
                                    json.dumps(data_parsed, indent=4, sort_keys=True)
                                    + "\n"
                                )
                            logging.error(f"pending IPC event (parsed): {data_pretty}")
                        except:  # noqa: E722
                            logging.error(f"pending IPC event (raw): {data}")

                        has_pending = True
                        check_again = True
                    else:
                        raise Exception(
                            f"unsupported IPC event socket events: {events}"
                        )
                elif self.ipc_cmd_sock and (
                    key.fd == self.ipc_event_sock.raw_socket.fileno()
                    or key.fd == self.ipc_cmd_sock.raw_socket.fileno()
                ):
                    # we don't care. it we might have aborted during send or IPC on domain socket
                    # is currently doing it's part of the protocol.
                    pass
                else:
                    raise Exception(
                        f"pending data on unknown socket. key={key} events={events}"
                    )

        return has_pending


class NotifySocket:
    def __init__(self, path):
        try:
            os.remove(path)
        except FileNotFoundError:
            pass

        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.socket.bind(path)

        self.selector = selectors.DefaultSelector()
        self.selector.register(self.socket, selectors.EVENT_READ, None)

    def __del__(self):
        self.close()

    def close(self):
        if self.socket is None:
            return

        # just to verify there isn't data that we didn't test
        has_pending = self.has_pending_data()

        self.socket.close()
        self.socket = None

        self.selector.close()
        self.selector = None

        assert not has_pending

    def wait_for(self, s, timeout=None):
        if timeout is not None:
            ret = self.selector.select(timeout)
            if len(ret) == 0:
                raise TimeoutError()

            assert len(ret) == 1

        data, address = self.socket.recvfrom(8196)
        assert data == s

    def has_pending_data(self):
        if self.selector is None:
            return False

        ret = self.selector.select(0)

        if len(ret) > 0:
            logging.error(f"pending notify data: {ret}")
            return True
        else:
            return False


@enum.unique
class Service(enum.IntEnum):
    VALUE = 20000
    DEVICE_DESCRIPTION = 20001
    PUBLIC_KEY = 20002
    NETWORK_MANAGEMENT = 20003
    VALUE_DESCRIPTION = 20004
    SERVICE_DESCRIPTION = 20005
    MEMORY_INFORMATION = 20006
    PARTNER_INFORMATION = 20007
    ACTION = 20008
    CALCULATION = 20009
    TIMER = 20010
    CALENDAR = 20011
    STATE_MACHINE = 20012
    FIRMWARE_UPDATE = 20013
    CHANNEL_SCAN = 20014
    STATUS = 20015
    CONFIGURATION = 20016


@enum.unique
class ValueType(enum.IntEnum):
    GENERAL_PURPOSE = 17
    COUNTER = 18


@enum.unique
class Permission(enum.IntEnum):
    ReadOnly = 1
    ReadWrite = 2
    WriteOnly = 3


def xml_remove_blanks(node):
    for x in node.childNodes:
        if x.nodeType == xml.dom.minidom.Node.TEXT_NODE:
            if x.nodeValue:
                x.nodeValue = x.nodeValue.strip()
        elif x.nodeType == xml.dom.minidom.Node.ELEMENT_NODE:
            xml_remove_blanks(x)


def xml_remove_comments(node):
    if isinstance(node, xml.dom.minidom.Comment):
        node.parentNode.removeChild(node)
    else:
        for x in node.childNodes:
            xml_remove_comments(x)


def xml_sendto(ppp, type, bindaddr, addr, data, encrypt=True):
    logging.debug(
        f"[{str(type)}][{bindaddr}->{addr}][{encrypt}] sendto(generic): {data.toprettyxml()}"
    )

    tclass = 0
    if encrypt:
        tclass = 0x1C

    data = data.toxml()
    data = lsdl_serializer.compress(type.value, data.encode())

    ppp.send(
        IPv6(src=bindaddr, dst=addr, tc=tclass)
        / UDP(sport=21234, dport=type)
        / Raw(load=data)
    )


class DeviceDescription:
    def __init__(self):
        # meaningless
        self.type = 1
        # Seluxit
        self.manufacturer = 2
        self.hardware_version = "1.0"
        self.bootloader_version = "1.0"
        self.stack_version = "1.5.3"
        self.application_version = "1.0"
        # Lemonbeat
        self.protocol = 1
        # gateway
        self.product = 1
        self.included = False
        self.name = "Name"
        # always online
        self.radio_mode = 0
        self.wakeup_channel = 0
        self.channel_map = b"\x10\x08\x08\x04"

    def manufacturer_str(self):
        names = [
            None,
            "Rwe",
            "Seluxit",
            "Gardena",
            "Lemonbeat",
            "Alko",
            "BitB",
            "InnogyMetering",
            "Pikkerton",
        ]

        name = names[self.manufacturer]
        assert name is not None

        return name

    def _xml_info(self, doc, type_id, key, value):
        info = doc.createElement("info")
        info.setAttribute("type_id", str(type_id))
        info.setAttribute(key, value)
        return info

    def _xml_info_string(self, doc, type_id, value):
        return self._xml_info(doc, type_id, "string", str(value))

    def _xml_info_number(self, doc, type_id, value):
        return self._xml_info(doc, type_id, "number", str(value))

    def _xml_info_bool(self, doc, type_id, value):
        if value:
            value = "1"
        else:
            value = "0"
        return self._xml_info(doc, type_id, "number", value)

    def _xml_info_hex(self, doc, type_id, value):
        return self._xml_info(doc, type_id, "hex", value.hex())

    def xml(self):
        impl = xml.dom.minidom.getDOMImplementation()
        doc = impl.createDocument(None, "network", None)

        network = doc.documentElement
        network.setAttribute("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance")
        network.setAttribute("xmlns", "urn:device_descriptionxsd")
        network.setAttribute("version", "1")
        network.setAttribute(
            "xsi:noNamespaceSchemaLocation", "../../xsd/device_description.xsd"
        )

        device = doc.createElement("device")
        device.setAttribute("version", "1")
        network.appendChild(device)

        report = doc.createElement("device_description_report")
        device.appendChild(report)

        report.appendChild(self._xml_info_number(doc, 1, self.type))
        report.appendChild(self._xml_info_number(doc, 2, self.manufacturer))
        report.appendChild(self._xml_info_hex(doc, 3, self.sgtin))
        report.appendChild(self._xml_info_string(doc, 5, self.hardware_version))
        report.appendChild(self._xml_info_string(doc, 6, self.bootloader_version))
        report.appendChild(self._xml_info_string(doc, 7, self.stack_version))
        report.appendChild(self._xml_info_string(doc, 8, self.application_version))
        report.appendChild(self._xml_info_number(doc, 9, self.protocol))
        report.appendChild(self._xml_info_number(doc, 10, self.product))
        report.appendChild(self._xml_info_bool(doc, 11, self.included))
        report.appendChild(self._xml_info_string(doc, 12, self.name))
        report.appendChild(self._xml_info_number(doc, 13, self.radio_mode))
        report.appendChild(self._xml_info_number(doc, 16, self.wakeup_channel))
        report.appendChild(self._xml_info_hex(doc, 17, self.channel_map))

        return doc


class InclusionMessage:
    def __init__(self, blob):
        self.blob = blob

    @staticmethod
    def from_xml(data, gotosleep=None):
        gotosleep_str = ""
        if gotosleep is not None:
            gotosleep_str = f'go_to_sleep="{gotosleep}"'

        blob = (
            data.getElementsByTagName("device")[0]
            .getElementsByTagName("network_include")[0]
            .firstChild.nodeValue
        )
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
           <network xmlns="urn:network_managementxsd" version="1">
               <device {gotosleep_str} version="1">
                   <network_include>{blob}</network_include>
               </device>
           </network>
        """,
        )

        return InclusionMessage(blob)


class MemoryInformation:
    def __init__(self, num_values):
        self.slots = {
            # values
            "1": {"count": num_values, "free_count": 0}
        }

    def xml(self):
        impl = xml.dom.minidom.getDOMImplementation()
        doc = impl.createDocument(None, "network", None)

        network = doc.documentElement
        network.setAttribute("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance")
        network.setAttribute("xmlns", "urn:memory_informationxsd")
        network.setAttribute("version", "1")
        network.setAttribute(
            "xsi:noNamespaceSchemaLocation", "../../xsd/memory_information.xsd"
        )

        device = doc.createElement("device")
        device.setAttribute("version", "1")
        network.appendChild(device)

        report = doc.createElement("memory_information_report")
        device.appendChild(report)

        for key, value in self.slots.items():
            info = doc.createElement("memory_information")
            info.setAttribute("memory_id", str(key))
            info.setAttribute("count", str(value["count"]))
            info.setAttribute("free_count", str(value["free_count"]))
            report.appendChild(info)

        return doc


class Configuration:
    def __init__(self, status):
        self.status = status

    def xml(self):
        impl = xml.dom.minidom.getDOMImplementation()
        doc = impl.createDocument(None, "network", None)

        network = doc.documentElement
        network.setAttribute("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance")
        network.setAttribute("xmlns", "urn:configurationxsd")
        network.setAttribute("version", "1")
        network.setAttribute(
            "xsi:noNamespaceSchemaLocation", "../../xsd/configuration.xsd"
        )

        device = doc.createElement("device")
        device.setAttribute("version", "1")
        network.appendChild(device)

        report = doc.createElement("config_status_report")
        report.setAttribute("status", str(self.status))
        device.appendChild(report)

        return doc


class Value:
    def __init__(
        self, id, type_id, permission, persistent, type, typemeta, value, name=None
    ):
        self.id = id
        self.type_id = type_id
        self.permission = permission
        self.persistent = persistent
        self.type = type
        self.timestamp = 0
        self.name = name

        if self.type == "hex":
            self.max_length = typemeta["max_length"]
        elif self.type == "number":
            self.unit = typemeta["unit"]
            self.min = typemeta["min"]
            self.max = typemeta["max"]
            self.step = typemeta["step"]
        elif self.type == "string":
            self.max_length = typemeta["max_length"]
        else:
            raise Exception("unsupported type")

        self.set(value)

    def set(self, value):
        if self.type == "hex":
            self.value = bytes(value)
        elif self.type == "number":
            self.value = float(value)
        elif self.type == "string":
            self.value = str(value)
        else:
            raise Exception("unsupported type")

    def xml_value(self, doc):
        v = doc.createElement("value_report")
        v.setAttribute("value_id", str(self.id))
        v.setAttribute("timestamp", str(self.timestamp))

        if self.type == "hex":
            v.setAttribute("hexBinary", self.value.hex())
        elif self.type == "number":
            v.setAttribute(
                "number", "NaN" if math.isnan(self.value) else str(self.value)
            )
        elif self.type == "string":
            v.setAttribute("string", self.value)
        else:
            raise Exception("unsupported type")

        return v

    def xml_description(self, doc):
        v = doc.createElement("value_description")
        v.setAttribute("value_id", str(self.id))
        v.setAttribute("type_id", str(self.type_id.value))
        v.setAttribute("mode", str(int(self.permission)))

        if self.name is not None:
            v.setAttribute("name", self.name)

        if self.persistent:
            spersistent = "1"
        else:
            spersistent = "0"
        v.setAttribute("persistent", spersistent)

        if self.type == "hex":
            f = doc.createElement("hexBinary_format")
            f.setAttribute("max_length", str(self.max_length))
        elif self.type == "number":
            f = doc.createElement("number_format")
            f.setAttribute("unit", self.unit)
            f.setAttribute("min", str(self.min))
            f.setAttribute("max", str(self.max))
            f.setAttribute("step", str(self.step))
        elif self.type == "string":
            f = doc.createElement("string_format")
            f.setAttribute("max_length", str(self.max_length))
        else:
            raise Exception("unsupported type")

        v.appendChild(f)

        return v


class FirmwareUpdate:
    def __init__(self, blob, checksum, chunk_size):
        if checksum is None:
            checksum = CRC16CALC.calculate_checksum(blob)

        self.image = blob
        self.size = len(blob)
        self.checksum = checksum
        self.chunk_size = chunk_size


def xml_value_descriptions(values):
    impl = xml.dom.minidom.getDOMImplementation()
    doc = impl.createDocument(None, "network", None)

    network = doc.documentElement
    network.setAttribute("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance")
    network.setAttribute("xmlns", "urn:value_descriptionxsd")
    network.setAttribute("version", "1")
    network.setAttribute(
        "xsi:noNamespaceSchemaLocation", "../../xsd/value_description.xsd"
    )

    device = doc.createElement("device")
    device.setAttribute("version", "1")
    network.appendChild(device)

    report = doc.createElement("value_description_report")
    device.appendChild(report)

    for value in values:
        report.appendChild(value.xml_description(doc))

    return doc


def xml_values(values):
    impl = xml.dom.minidom.getDOMImplementation()
    doc = impl.createDocument(None, "network", None)

    network = doc.documentElement
    network.setAttribute("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance")
    network.setAttribute("xmlns", "urn:valuexsd")
    network.setAttribute("version", "1")
    network.setAttribute("xsi:noNamespaceSchemaLocation", "../../xsd/value.xsd")

    device = doc.createElement("device")
    device.setAttribute("version", "1")
    network.appendChild(device)

    for value in values:
        device.appendChild(value.xml_value(doc))

    return doc


def xml_calendar_timezone_report(offset):
    impl = xml.dom.minidom.getDOMImplementation()
    doc = impl.createDocument(None, "network", None)

    network = doc.documentElement
    network.setAttribute("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance")
    network.setAttribute("xmlns", "urn:calendarxsd")
    network.setAttribute("version", "1")
    network.setAttribute("xsi:noNamespaceSchemaLocation", "../../xsd/calendar.xsd")

    device = doc.createElement("device")
    device.setAttribute("version", "1")
    network.appendChild(device)

    report = doc.createElement("calendar_report_timezone")
    report.setAttribute("offset", str(offset))
    device.appendChild(report)

    return doc


def assert_xml(actual, needle):
    needle = xml.dom.minidom.parseString(needle)
    xml_remove_blanks(needle)
    xml_remove_comments(needle)

    assert actual.toxml() == needle.toxml(), f"\n{actual.toxml()}\nvs\n{needle.toxml()}"


class PartnerInformation:
    def __init__(self):
        # If we ever wanna support more stuff we'll have to store things here
        pass


class Device:
    def __init__(self, ppp, socket_cleanup, addr, dd, values):
        self.addr = addr
        self.dd = dd
        self.config = Configuration(0)
        self.values = values
        self.mi = MemoryInformation(len(self.values))
        self.utc_offset = 0
        self.ppp = ppp

        socket_cleanup.append(self)

    def __del__(self):
        self.close()

    def close(self):
        # just to verify there isn't data that we didn't test
        has_pending = self.has_pending_data()

        assert not has_pending

    def recvfrom(self, service):
        packet = self.ppp.recv_udp()
        logging.debug(f"received: {packet.show(dump=True)}")

        # TODO
        # assert packet[IPv6].src == PPP_ADDR_DEFAULT_GATEWAY
        assert packet[IPv6].dst == self.addr
        assert packet[UDP].dport == service

        address = (packet[IPv6].src, packet[UDP].sport)

        data = packet[Raw].load
        data = lsdl_serializer.decompress(service, data)
        data = xml.dom.minidom.parseString(data)
        xml_remove_blanks(data)
        xml_remove_comments(data)

        logging.debug(f"[{str(service)}][{address}] received: {data.toprettyxml()}")

        return (data, address)

    def sendto(self, service, addr, data, encrypt=True):
        logging.debug(f"[{str(service)}][{addr}] sendto(svc): {data.toprettyxml()}")
        data = data.toxml()
        data = lsdl_serializer.compress(service.value, data.encode())

        tclass = 0
        if encrypt:
            tclass = 0x1C

        self.ppp.send(
            IPv6(src=self.addr, dst=addr[0], tc=tclass)
            / UDP(sport=service, dport=addr[1])
            / Raw(load=data)
        )

    def gotosleep(self, value):
        if self.dd.radio_mode == 1:
            return value
        else:
            return None

    def gotosleep_str(self, value):
        gotosleep = self.gotosleep(value)

        gotosleep_str = ""
        if gotosleep is not None:
            gotosleep_str = f'go_to_sleep="{gotosleep}"'

        return gotosleep_str

    def value(self, name):
        return find(self.values, lambda v: v.name == name)

    def devdir(self):
        return os.path.join(WORKDIR, f"Device_descriptionID_{self.addr}")

    def devdir_exists(self):
        return os.path.exists(self.devdir())

    def dd_path(self):
        return os.path.join(self.devdir(), f"Device_descriptionID_{self.addr}.json")

    ## this does the same hacky conversion as the rust code
    #  TODO: adjust this as soon as we know how we actually want to do this.
    def bnwid(self):
        octets = bytearray(ipaddress.IPv6Address(self.addr).packed)

        octets[6] = 0x40 | (octets[6] & 0xF)
        octets[8] = 0x89
        octets[9] = 0xAB

        return uuid.UUID(bytes=bytes(octets))

    def assert_ipc_event_generic(self, event):
        assert event["entity"]["device"] == self.identifier()

        assert event["metadata"]["source"] == "lemonbeatd"
        assert isinstance(event["metadata"]["sequence"], int)
        assert event["metadata"]["sequence"] >= 0

    def ipc_read_resource(self, ipc_cmd_sock, resource):
        ipc_cmd_sock.send(
            json.dumps(
                [
                    {
                        "op": "read",
                        "entity": {
                            "device": f"{self.identifier()}",
                            "path": f"{resource}",
                        },
                    }
                ]
            ).encode()
        )

    def ipc_execute_resource(self, ipc_cmd_sock, resource):
        ipc_cmd_sock.send(
            json.dumps(
                [
                    {
                        "op": "execute",
                        "entity": {
                            "device": f"{self.identifier()}",
                            "path": f"{resource}",
                        },
                    }
                ]
            ).encode()
        )

    def assert_ipc_resource_update(
        self, ipc_event_sock, resource, type=None, value=None
    ):
        event = wait_for_ipc_sock_event(ipc_event_sock)
        self.assert_ipc_event_generic(event)
        assert event["entity"]["path"] == resource
        if type is not None:
            assert event["payload"][type] == value

    def assert_fota_event(self, ipc_event_sock, status, result, pkgver=None):
        event = wait_for_ipc_sock_event(ipc_event_sock)
        self.assert_ipc_event_generic(event)
        assert event["entity"]["path"] == "firmware_update/0"

        assert get_ipc_value_raw(event["payload"]["state"], "vi") == status
        assert get_ipc_value_raw(event["payload"]["update_result"], "vi") == result
        if pkgver:
            assert get_ipc_value_raw(event["payload"]["pkg_version"], "vs") == pkgver

    def assert_ddl_status_event(self, ipc_event_sock, val):
        event = wait_for_ipc_sock_event(ipc_event_sock)
        self.assert_ipc_event_generic(event)
        assert event["entity"]["path"] == "data_download/0/status"
        assert get_ipc_value(event, "payload", "vi", urn="urn:oma:lwm2m:x:28174") == val

    def has_pending_data(self):
        had_pending_data = False

        while True:
            try:
                packet = self.ppp.recv(blocking=False)
                if packet is None:
                    break

                logging.error(f"pending device data: {packet.show(dump=True)}")
                had_pending_data = True

            except:  # noqa: E722
                logging.exception("pending device data: FAILED TO READ")
                break

        return had_pending_data

    def assert_config_request(self, answer=True):
        logging.info("assert_config_request")

        data, addr = self.recvfrom(Service.CONFIGURATION)
        assert_xml(
            data,
            """<?xml version="1.0" ?>
               <network xmlns="urn:configurationxsd" version="1">
                   <device version="1">
                       <config_status_get/>
                   </device>
               </network>
            """,
        )

        if answer:
            self.sendto(Service.CONFIGURATION, addr, self.config.xml())

    def assert_devdesc_request(self):
        logging.info("assert_devdesc_request")

        gotosleep = self.gotosleep_str(20000)

        data, addr = self.recvfrom(Service.DEVICE_DESCRIPTION)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
            <network xmlns="urn:device_descriptionxsd" version="1">
                <device {gotosleep} version="1">
                    <device_description_get/>
                </device>
            </network>
            """,
        )
        self.sendto(
            Service.DEVICE_DESCRIPTION, addr, self.dd.xml(), encrypt=self.dd.included
        )

    def assert_exclusion_request(self):
        logging.info("assert_exclusion_request")

        data, addr = self.recvfrom(Service.DEVICE_DESCRIPTION)
        assert_xml(
            data,
            """<?xml version="1.0" ?>
            <network xmlns="urn:device_descriptionxsd" version="1">
                <device version="1">
                    <device_description_set>
                        <info number="0" type_id="11"/>
                    </device_description_set>
                </device>
            </network>
            """,
        )

    def announce_devdesc(self):
        xml_sendto(
            self.ppp,
            Service.DEVICE_DESCRIPTION,
            self.addr,
            ADDR_MULTICAST,
            self.dd.xml(),
            encrypt=self.dd.included,
        )

    def send_exclusion_confirmation(self):
        self.send_status(1, 101, 13)

    def send_status(self, level, type_id, code):
        impl = xml.dom.minidom.getDOMImplementation()
        doc = impl.createDocument(None, "network", None)

        network = doc.documentElement
        network.setAttribute("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance")
        network.setAttribute("xmlns", "urn:statusxsd")
        network.setAttribute("version", "1")
        network.setAttribute("xsi:noNamespaceSchemaLocation", "../../xsd/status.xsd")

        device = doc.createElement("device")
        device.setAttribute("version", "1")
        network.appendChild(device)

        report = doc.createElement("status_report")
        report.setAttribute("level", str(level))
        report.setAttribute("type_id", str(type_id))
        report.setAttribute("code", str(code))
        device.appendChild(report)

        xml_sendto(self.ppp, Service.STATUS, self.addr, ADDR_MULTICAST, doc)

    def assert_device_nonce_reset(self, tcpserver):
        tcpserver.handle_command(0x04, 6, b"\x6d\xff\xfe\x6f\x00\x01")

    def assert_inclusion(self, answer=True):
        logging.info(f"assert_inclusion(answer={answer})")

        data, addr = self.recvfrom(Service.NETWORK_MANAGEMENT)
        InclusionMessage.from_xml(data, self.gotosleep(20000))

        if answer:
            self.dd.included = True
            self.announce_devdesc()

    def assert_set_wakeup_channel(self):
        logging.info("assert_set_wakeup_channel")

        gotosleep = self.gotosleep_str(20000)

        # NOTE: we never test more than one device and lemonbeatd starts
        #       allocating from 1, so it's always gonna be channel 1.
        data, addr = self.recvfrom(Service.DEVICE_DESCRIPTION)

        device = data.getElementsByTagName("device")
        device = device.item(0)

        device_description = device.getElementsByTagName("device_description_set")
        device_description = device_description.item(0)

        info = device_description.getElementsByTagName("info")
        info = info.item(0)
        wakeup_channel = int(info.getAttribute("number"), 10)
        assert wakeup_channel >= 1
        assert wakeup_channel <= 30

        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:device_descriptionxsd" version="1">
                   <device {gotosleep} version="1">
                       <device_description_set>
                           <info number="{wakeup_channel}" type_id="16"/>
                       </device_description_set>
                   </device>
               </network>
            """,
        )

        self.dd.wakeup_channel = wakeup_channel

    def assert_meminfo_request(self, answer=True):
        logging.info("assert_meminfo_request")
        data, addr = self.recvfrom(Service.MEMORY_INFORMATION)

        gotosleep = self.gotosleep_str(20000)

        if answer:
            assert_xml(
                data,
                f"""<?xml version="1.0" ?>
                   <network xmlns="urn:memory_informationxsd" version="1">
                       <device {gotosleep} version="1">
                           <memory_information_get/>
                       </device>
                   </network>
                """,
            )
            self.sendto(Service.MEMORY_INFORMATION, addr, self.mi.xml())
        else:
            # two more retries expected
            self.recvfrom(Service.MEMORY_INFORMATION)
            self.recvfrom(Service.MEMORY_INFORMATION)

    def assert_valdesc_requests(self):
        logging.info("assert_valdesc_requests")

        gotosleep = self.gotosleep_str(20000)

        for values in chunker(self.values, 10):
            gets = []
            for value in values:
                gets.append(
                    f'<value_description_get value_description_id="{value.id}"/>'
                )

            data, addr = self.recvfrom(Service.VALUE_DESCRIPTION)
            assert_xml(
                data,
                f"""<?xml version="1.0" ?>
                   <network xmlns="urn:value_descriptionxsd" version="1">
                       <device {gotosleep} version="1">
                           {"".join(gets)}
                       </device>
                   </network>
                """,
            )
            self.sendto(Service.VALUE_DESCRIPTION, addr, xml_value_descriptions(values))

    def assert_val_set(self, name, expected_value, gotosleep=0):
        gotosleep = self.gotosleep_str(gotosleep)

        logging.info("assert_val_set")
        value = find(self.values, lambda v: v.name == name)

        if value.type == "hex":
            valattr = f'hexBinary="{bytes(expected_value).hex().upper()}"'
        elif value.type == "number":
            valattr = f'number="{expected_value}"'
        elif value.type == "string":
            valattr = f'string="{expected_value}"'
        else:
            raise Exception("unsupported type")

        data, addr = self.recvfrom(Service.VALUE)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:valuexsd" version="1">
                   <device {gotosleep} version="1">
                       <value_set {valattr}  timestamp="0" value_id="{value.id}"/>
                   </device>
               </network>
            """,
        )

    def assert_values_set(self, map):
        logging.info("assert_values_set")

        needle = """<?xml version="1.0" ?>
           <network xmlns="urn:valuexsd" version="1">
               <device version="1">
               </device>
           </network>
        """
        needle = xml.dom.minidom.parseString(needle)

        device = needle.getElementsByTagName("device")
        device = device.item(0)

        # append the `value_sets` sorted by `value_id`
        for name, expected_value in sorted(
            map.items(),
            key=lambda item: find(self.values, lambda v: v.name == item[0]).id,
        ):
            value = find(self.values, lambda v: v.name == name)

            value_set = needle.createElement("value_set")

            if value.type == "hex":
                value_set.setAttribute(
                    "hexBinary", f"{bytes(expected_value).hex().upper()}"
                )
            elif value.type == "number":
                value_set.setAttribute("number", f"{expected_value}")
            elif value.type == "string":
                value_set.setAttribute("string", f"{expected_value}")
            else:
                raise Exception("unsupported type")

            value_set.setAttribute("timestamp", "0")
            value_set.setAttribute("value_id", f"{value.id}")

            device.appendChild(value_set)

        xml_remove_blanks(needle)
        xml_remove_comments(needle)

        actual, addr = self.recvfrom(Service.VALUE)

        # the following sorts the `value_set`s by `value_id` so we can assert
        # the whole message to be equal. (dict/HashMap ordering is random)
        device = actual.getElementsByTagName("device")
        device = device.item(0)

        value_sets = device.getElementsByTagName("value_set")
        value_sets.sort(key=lambda x: int(x.attributes["value_id"].value))

        for value_set in value_sets:
            device.removeChild(value_set)

        for value_set in value_sets:
            device.appendChild(value_set)

        xml_remove_blanks(actual)
        xml_remove_comments(actual)

        assert (
            actual.toxml() == needle.toxml()
        ), f"\n{actual.toxml()}\nvs\n{needle.toxml()}"

    def assert_val_requests(self):
        logging.info("assert_val_requests")

        gotosleep = self.gotosleep_str(20000)

        data, addr = self.recvfrom(Service.VALUE)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:valuexsd" version="1">
                   <device {gotosleep} version="1">
                       <value_get />
                   </device>
               </network>
            """,
        )
        self.sendto(Service.VALUE, addr, xml_values(self.values))

    ## UTC001
    def assert_utc_update(self, offset=None, drop_status=False, final_gotosleep=0):
        logging.info("assert_utc_update")

        if offset is None:
            offset = 0

        gotosleep = self.gotosleep_str(20000)
        final_gotosleep = self.gotosleep_str(final_gotosleep)

        data, addr = self.recvfrom(Service.CALENDAR)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:calendarxsd" version="1">
                   <device {gotosleep} version="1">
                       <calendar_set_timezone offset="{offset}"/>
                   </device>
               </network>
            """,
        )

        if not drop_status:
            self.send_status(1, 13, 13)

        data, addr = self.recvfrom(Service.CONFIGURATION)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:configurationxsd" version="1">
                   <device {gotosleep} version="1">
                       <config_mode_set mode="2"/>
                   </device>
               </network>
            """,
        )

        data, addr = self.recvfrom(Service.CONFIGURATION)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:configurationxsd" version="1">
                   <device {gotosleep} version="1">
                       <config_status_get/>
                   </device>
               </network>
            """,
        )
        self.sendto(Service.CONFIGURATION, addr, self.config.xml())

        data, addr = self.recvfrom(Service.CALENDAR)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:calendarxsd" version="1">
                   <device {final_gotosleep} version="1">
                       <calendar_get_timezone/>
                   </device>
               </network>
            """,
        )
        self.sendto(Service.CALENDAR, addr, xml_calendar_timezone_report(offset))

    def send_fota_status(self, addr, status, offset):
        impl = xml.dom.minidom.getDOMImplementation()
        doc = impl.createDocument(None, "network", None)

        network = doc.documentElement
        network.setAttribute("xmlns", "urn:firmware_updatexsd")
        network.setAttribute("version", "1")

        device = doc.createElement("device")
        device.setAttribute("version", "1")
        network.appendChild(device)

        report = doc.createElement("firmware_report")
        report.setAttribute("expected_offset", offset)
        report.setAttribute("status", f"{status}")
        device.appendChild(report)

        self.sendto(Service.FIRMWARE_UPDATE, addr, network)

    def send_fota_info(self, addr, size, received_size, chunk_size):
        impl = xml.dom.minidom.getDOMImplementation()
        doc = impl.createDocument(None, "network", None)

        network = doc.documentElement
        network.setAttribute("xmlns", "urn:firmware_updatexsd")
        network.setAttribute("version", "1")

        device = doc.createElement("device")
        device.setAttribute("version", "1")
        network.appendChild(device)

        report = doc.createElement("firmware_information_report")
        report.setAttribute("size", f"{size}")
        report.setAttribute("firmware_id", "1")
        report.setAttribute("received_size", f"{received_size}")
        report.setAttribute("chunk_size", f"{chunk_size}")
        device.appendChild(report)

        self.sendto(Service.FIRMWARE_UPDATE, addr, network)

    def assert_firmware_init(self, firmware, id=1, offset="0"):
        logging.info("assert_firmware_init")

        data, addr = self.recvfrom(Service.FIRMWARE_UPDATE)
        assert_xml(
            data,
            f"""<?xml version="1.0" ?>
               <network xmlns="urn:firmware_updatexsd" version="1">
                   <device version="1">
                       <firmware_init checksum="{firmware.checksum:04X}" firmware_id="{id}" size="{firmware.size}"/>
                   </device>
               </network>
            """,
        )

        self.send_fota_status(addr, 1, offset)

    def assert_firmware_get_info(self, firmware, received_size):
        logging.info("assert_firmware_get_info")

        data, addr = self.recvfrom(Service.FIRMWARE_UPDATE)
        assert_firmware_info_get(data)
        self.send_fota_info(addr, firmware.size, received_size, firmware.chunk_size)

    def assert_firmware_data(
        self, firmware, received_chunk=0, lost_status_report=False
    ):
        logging.info("assert_firmware_data")

        received_data_offset = received_chunk * firmware.chunk_size
        received_data = firmware.image[
            received_chunk
            * firmware.chunk_size : (received_chunk + 1)
            * firmware.chunk_size
        ]
        expected_data_offset = min(
            (received_chunk + 1) * firmware.chunk_size, firmware.size
        )

        firmware_data_xml = f"""<?xml version="1.0" ?>
                   <network xmlns="urn:firmware_updatexsd" version="1">
                       <device version="1">
                            <firmware_data offset="{received_data_offset}">
                                <chunk>{bytes(received_data).hex().upper()}</chunk>
                            </firmware_data>
                       </device>
                   </network>
                """

        data, addr = self.recvfrom(Service.FIRMWARE_UPDATE)
        assert_xml(data, firmware_data_xml)

        if lost_status_report:
            # doing nothing to simulate lost status update in the air

            # lemonbeatd resends data after 10 seconds
            data, addr = self.recvfrom(Service.FIRMWARE_UPDATE)
            assert_xml(data, firmware_data_xml)

            last_chunk = expected_data_offset == firmware.size

            if last_chunk:
                # responding with not ok as firmware upload was completed
                self.send_fota_status(addr, 2, f"{expected_data_offset}")
            else:
                # responding with wrong chunk offset as device is expecting the following chunk
                self.send_fota_status(addr, 6, f"{expected_data_offset}")

        else:
            self.send_fota_status(addr, 1, f"{expected_data_offset}")

    def assert_firmware_update_start_with_status(
        self, firmware, status, update_start_confirmation=True, excludes_device=True
    ):
        logging.info("assert_firmware_update_start")

        data, addr = self.recvfrom(Service.FIRMWARE_UPDATE)
        assert_xml(
            data,
            """<?xml version="1.0" ?>
               <network xmlns="urn:firmware_updatexsd" version="1">
                   <device version="1">
                        <firmware_update_start/>
                   </device>
               </network>
            """,
        )
        if update_start_confirmation:
            self.send_fota_status(addr, status, f"{firmware.size}")

        self.dd.included = not excludes_device

    def send_value_update(self, name):
        value = find(self.values, lambda x: x.name == name)
        self.sendto(Service.VALUE, (ADDR_MULTICAST, Service.VALUE), xml_values([value]))

    def send_value_updates(self, names):
        values = []
        for name in names:
            values.append(find(self.values, lambda x: x.name == name))

        self.sendto(Service.VALUE, (ADDR_MULTICAST, Service.VALUE), xml_values(values))

    def identifier(self):
        return self.dd.sgtin.hex().upper()


def assert_firmware_info_get(data):
    assert_xml(
        data,
        """<?xml version="1.0" ?>
           <network xmlns="urn:firmware_updatexsd" version="1">
               <device version="1">
                    <firmware_information_get/>
               </device>
           </network>
        """,
    )


def get_ipc_value_raw(o, type):
    val = o[type]
    ts = o.get("ts", None)
    if ts is None:
        assert o == {type: val}
    else:
        assert o == {type: val, "ts": ts}

    return val


def get_ipc_value(payload, name, type, urn=None):
    o = payload[name]

    if urn is not None:
        assert o["_urn"] == urn
        del o["_urn"]

    return get_ipc_value_raw(o, type)


def assert_non_pretty_json(raw):
    assert b"\n" not in raw, raw
    assert b"\r" not in raw, raw


def wait_for_ipc_sock(sock: UnixSocket):
    data = sock.recv_packet()
    data = data.decode()
    parsed = json.loads(data)
    logging.debug(f"received IPC json: {parsed}")
    assert_non_pretty_json(parsed)

    return parsed


def wait_for_ipc_sock_event(sock):
    parsed = wait_for_ipc_sock(sock)
    assert type(parsed) is list
    assert len(parsed) == 1
    return parsed[0]


def ipc_fota_init(id, ipc_cmd_sock, payload):
    payload = base64.b64encode(payload)
    payload = payload.decode("UTF-8")

    ipc_cmd_sock.send(
        json.dumps(
            [
                {
                    "op": "write",
                    "entity": {
                        "device": f"{id}",
                        "path": "firmware_update/0/package",
                    },
                    "payload": {"vo": f"{payload}"},
                }
            ]
        ).encode()
    )


def ipc_ddl_init(id, ipc_cmd_sock, slot, data, content_tag, checksum=None):
    if checksum is None:
        checksum = CRC16CALC.calculate_checksum(data)

    data = base64.b64encode(bytes(data))
    data = data.decode("UTF-8")

    ipc_cmd_sock.send(
        json.dumps(
            [
                {
                    "op": "write",
                    "entity": {
                        "device": f"{id}",
                        "path": "data_download/0",
                    },
                    "payload": {
                        "data": {"vo": f"{data}"},
                        "slot": {"vi": slot},
                        "checksum": {"vi": checksum},
                        "content_tag": {"vi": content_tag},
                    },
                }
            ]
        ).encode()
    )


def assert_includable_device(ipc_event_sock, dev, op, started, completed, error):
    idev = wait_for_ipc_sock_event(ipc_event_sock)

    path = idev["entity"]["path"]
    id = re.fullmatch("includable_device/([0-9]+)", path).group(1)

    assert idev["entity"]["service"] == "lemonbeatd"

    assert idev["metadata"]["source"] == "lemonbeatd"
    assert isinstance(idev["metadata"]["sequence"], int)
    assert idev["metadata"]["sequence"] >= 0

    assert idev["op"] == op

    payload = idev["payload"]
    assert get_ipc_value(payload, "identifier", "vs") == dev.identifier()
    assert get_ipc_value(payload, "protocol", "vi") == 2
    assert get_ipc_value(payload, "inclusion_started", "vb") == started
    assert get_ipc_value(payload, "inclusion_completed", "vb") == completed
    assert get_ipc_value(payload, "inclusion_error", "vi") == error

    return id


def assert_ipc_endpoint(dev, ipc_event_sock, update_result=0, utc_offset=""):
    idev = wait_for_ipc_sock_event(ipc_event_sock)
    payload = idev["payload"]

    dev.assert_ipc_event_generic(idev)
    assert idev["entity"]["path"] == ""

    assert idev["op"] == "update"

    assert_ipc_endpoint_payload(dev, payload, update_result)


def assert_ipc_endpoint_payload(dev, payload, update_result=0, utc_offset=""):
    device = payload["device"]
    assert len(device) == 2
    assert device["_urn"] == "urn:oma:lwm2m:oma:3:1.1"
    device = device["0"]
    assert len(device) == 10

    model, serial, device_type = sgtin_decompose(dev.dd.sgtin)
    assert get_ipc_value(device, "device_type", "vs") == device_type
    assert get_ipc_value(device, "error_code", "ai") == [0]
    assert (
        get_ipc_value(device, "firmware_version", "vs")
        == f"{dev.dd.bootloader_version}-{dev.dd.stack_version}"
    )
    assert get_ipc_value(device, "hardware_version", "vs") == dev.dd.hardware_version
    assert get_ipc_value(device, "manufacturer", "vs") == dev.dd.manufacturer_str()
    assert get_ipc_value(device, "model_number", "vs") == model
    assert get_ipc_value(device, "serial_number", "vs") == serial.zfill(8)
    assert get_ipc_value(device, "software_version", "vs") == dev.dd.application_version
    assert get_ipc_value(device, "supported_binding_and_modes", "vs") == "U"
    assert get_ipc_value(device, "utc_offset", "vs") == utc_offset

    fwupd = payload["firmware_update"]
    assert len(fwupd) == 2
    assert fwupd["_urn"] == "urn:oma:lwm2m:oma:5:1.1"
    fwupd = fwupd["0"]
    assert len(fwupd) == 5
    assert get_ipc_value(fwupd, "firmware_update_delivery_method", "vi") == 1
    assert get_ipc_value(fwupd, "package_uri", "vs") == ""
    assert get_ipc_value(fwupd, "state", "vi") == 0
    assert get_ipc_value(fwupd, "update_result", "vi") == update_result
    assert get_ipc_value(fwupd, "pkg_version", "vs") == ""

    lb = payload["lemonbeat"]
    assert len(lb) == 2
    assert lb["_urn"] == "urn:oma:lwm2m:x:31000"
    lb = lb["0"]

    num = 0
    for value in dev.values:
        if value.permission == Permission.WriteOnly:
            continue

        num += 1

        if value.type == "number":
            if pytest.approx(value.step) == 1:
                assert get_ipc_value(lb, value.name, "vi") == pytest.approx(value.value)
            else:
                assert get_ipc_value(lb, value.name, "vf") == pytest.approx(value.value)
        elif value.type == "string":
            assert get_ipc_value(lb, value.name, "vs") == value.value
        elif value.type == "hex":
            assert (
                get_ipc_value(lb, value.name, "vo")
                == base64.b64encode(value.value).decode()
            )
        else:
            raise Exception("unsupported value type")

    assert len(lb) == num


def assert_exclusion(ipc_event_sock, dev):
    idev = wait_for_ipc_sock_event(ipc_event_sock)
    path = idev["entity"]["path"]

    assert path == ""
    assert idev["op"] == "delete"
    assert idev["entity"]["device"] == dev.identifier()

    assert not dev.devdir_exists()


def assert_connection_status_change(ipc_event_sock, online):
    event = wait_for_ipc_sock_event(ipc_event_sock)
    assert event["entity"]["path"] == "connection_status/0/online"
    assert event["payload"]["vb"] == online


def assert_utc_offset_change(ipc_event_sock, utc_offset):
    event = wait_for_ipc_sock_event(ipc_event_sock)
    assert event["entity"]["path"] == "device/0/utc_offset"
    assert event["op"] == "update"
    assert event["payload"]["vs"] == utc_offset


def include_radiomodule(tcpserver, notify_socket):
    tcpserver.wait_for_client()

    # get app version
    tcpserver.handle_command(0x0D, answer=b"\x01\x00\x051.2.3")

    # set MAC address
    tcpserver.handle_command(0x02, 6, b"\xaa\xbb\xcc\xdd\xee\xff")

    # set network key
    network_key = tcpserver.handle_command(0x01, 16)
    logging.debug(f"network key: {network_key}")
    assert len(network_key) == 16

    # set antenna diversity mode
    tcpserver.handle_command(0x08, 1, b"\x01")

    # get TX MAC sequence counter
    tcpserver.handle_command(
        0x0E, 0, answer=b"\x01\x00\x08\x00\x04\x00\x00\x00\x00\x00\x00"
    )

    notify_socket.wait_for(b"READY=1")


def assert_reinclude_radiomodule(tcpserver, notify_socket):
    include_radiomodule(tcpserver, notify_socket)


def include_device(
    tcpserver,
    ipc_cmd_sock,
    ipc_event_sock,
    notify_socket,
    socket_cleanup,
    dev,
    num_ipc_includes=1,
    answer_inclusion=True,
):
    logging.info(f"include_device answer={answer_inclusion}")

    dev.announce_devdesc()
    id = assert_includable_device(ipc_event_sock, dev, "update", False, False, 0)
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

    for _ in range(num_ipc_includes):
        logging.info(f"sending IPC include json: {include_json}")
        ipc_cmd_sock.send(include_json)

        response = wait_for_ipc_sock(ipc_cmd_sock)

        logging.debug(
            f"IPC command response: {json.dumps(response, indent=4, sort_keys=True)}"
        )

        assert len(response) == 1
        response = response[0]
        assert response["entity"]["service"] == "lemonbeatd"
        assert response["entity"]["path"] == path
        assert response["success"] is True

    id2 = assert_includable_device(ipc_event_sock, dev, "update", True, False, 0)
    assert id == id2

    dev.assert_device_nonce_reset(tcpserver)

    if answer_inclusion:
        dev.assert_inclusion(True)
    else:
        dev.assert_inclusion(False)
        dev.assert_inclusion(False)
        dev.assert_inclusion(False)

        dev.dd.included = True

        dev.assert_devdesc_request()
        dev.announce_devdesc()

    if dev.dd.radio_mode == 1:
        dev.assert_set_wakeup_channel()
        dev.assert_devdesc_request()

    dev.assert_meminfo_request()
    dev.assert_valdesc_requests()
    dev.assert_val_requests()

    id2 = assert_includable_device(ipc_event_sock, dev, "update", True, True, 0)
    assert id == id2

    id2 = assert_includable_device(ipc_event_sock, dev, "delete", True, True, 0)
    assert id == id2

    assert_ipc_endpoint(dev, ipc_event_sock)

    assert dev.devdir_exists()

    dev.assert_utc_update()
    assert_utc_offset_change(ipc_event_sock, "UTC+00:00")

    # DCS010: newly included device does not trigger ping


def exclude_device(ipc_cmd_sock, dev, device_online, success):
    dev.ipc_execute_resource(ipc_cmd_sock, "device/0/factory_reset")

    if success:
        dev.assert_exclusion_request()
        if device_online:
            dev.send_exclusion_confirmation()
            dev.dd.included = False
            dev.announce_devdesc()
        else:
            dev.assert_exclusion_request()
            dev.assert_exclusion_request()

    response = wait_for_ipc_sock(ipc_cmd_sock)

    logging.debug(
        f"exclusion command response: {json.dumps(response, indent=4, sort_keys=True)}"
    )

    assert len(response) == 1
    response = response[0]
    assert response["entity"]["device"] == dev.identifier()
    assert response["entity"]["path"] == "device/0/factory_reset"
    assert response["success"] == success

    # NOTE: with the way lemonbeatd works right now we don't expect an
    #       includable device for the first announcement because it got
    #       processed by the now deleted device.


## Currently, this creates a SG Power
def make_simple_device(ppp, socket_cleanup, addr):
    dd = DeviceDescription()
    dd.sgtin = b"\x30\x34\xf8\xee\x90\x22\x73\xc0\x00\x00\x96\x6a"
    dd.product = 7
    dd.manufacturer = 3
    values = [
        Value(
            1,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadOnly,
            False,
            "number",
            {
                "unit": "%",
                "min": 0.0,
                "max": 100.0,
                "step": 1.0,
            },
            30,
            name="rf_link_quality",
        ),
        Value(
            2,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadOnly,
            False,
            "hex",
            {
                "max_length": 32,
            },
            [
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
            ],
            name="fatal_error_log",
        ),
        Value(
            3,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadOnly,
            False,
            "number",
            {
                "unit": "",
                "min": 0.0,
                "max": 2.0,
                "step": 1.0,
            },
            0,
            name="error",
        ),
        Value(
            4,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadWrite,
            False,
            "number",
            {
                "unit": "",
                "min": 0.0,
                "max": 39.0,
                "step": 1.0,
            },
            1,
            name="command",
        ),
        Value(
            5,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadWrite,
            False,
            "number",
            {
                "unit": "s",
                "min": -16777215.0,
                "max": 16777216.0,
                "step": 1.0,
            },
            16777216,
            name="power_timer",
        ),
        Value(
            6,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadWrite,
            True,
            "hex",
            {
                "max_length": 6,
            },
            [],
            name="action_paused_until_1",
        ),
        Value(
            7,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadWrite,
            False,
            "hex",
            {
                "max_length": 252,
            },
            [],
            name="schedule_config",
        ),
        Value(
            8,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadOnly,
            False,
            "hex",
            {
                "max_length": 108,
            },
            [],
            name="schedule_state",
        ),
        Value(
            9,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadWrite,
            False,
            "hex",
            {
                "max_length": 36,
            },
            [],
            name="schedule_state_control_skip",
        ),
        Value(
            10,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadWrite,
            False,
            "hex",
            {
                "max_length": 108,
            },
            [],
            name="schedule_state_control_shorten",
        ),
        Value(
            11,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadWrite,
            True,
            "number",
            {
                "unit": "s",
                "min": 1.0,
                "max": 3600.0,
                "step": 1.0,
            },
            480,
            name="be_decision_time",
        ),
        Value(
            12,
            ValueType.GENERAL_PURPOSE,
            Permission.ReadOnly,
            False,
            "number",
            {
                "unit": "",
                "min": 0.0,
                "max": 1.0,
                "step": 0.1,
            },
            30,
            name="threshold",
        ),
        Value(
            13,
            ValueType.GENERAL_PURPOSE,
            Permission.WriteOnly,
            False,
            "hex",
            {
                "max_length": 8,
            },
            [],
            name="data_download_int",
        ),
    ]
    return Device(ppp, socket_cleanup, addr, dd, values)


def make_cbtl(ppp, socket_cleanup, addr):
    dev = make_simple_device(ppp, socket_cleanup, addr)

    dev.dd.product = 10
    dev.dd.sgtin = b"\x30\x35\xc3\x3a\x88\x34\xb9\x2f\x14\x7b\x80\xe8"

    return dev


def sgtin_decompose(sgtin):
    map = {
        18869: "Water Control",
        18845: "Sensor",
        6146: "Robotics Lawnmower",
        22538: "Automatic Home and Garden Pump",
        31653: "Irrigation Control",
        35279: "Power Adapter",
        19040: "Sensor",
        29694: "Robotics Lawnmower",
        53988: "Robotics Lawnmower",
        21869: "Gateway",
        46350: "Gateway",
    }

    serial = int.from_bytes(sgtin, byteorder="big") & (2**38 - 1)
    model = int.from_bytes(sgtin, byteorder="big") >> 38 & (2**20 - 1)
    device_type = map[model]

    return str(model), str(serial), device_type
