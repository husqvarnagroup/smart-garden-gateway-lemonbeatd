# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import logging
import threading
import time
import random
from scapy.all import TCP, IPv6, ICMPv6ND_RS, Raw, UDP
import selectors

import dbus
import dbus.mainloop.glib
import dbus.service
import pytest
from gi.repository import GLib
import tz
import gc
from scapy.all import TunTapInterface
import subprocess

pytest.register_assert_rewrite("lbtest")
import lbtest  # noqa: E402


def exec(cmd):
    subprocess.run(cmd, check=True)


def filter_accept_all(item):
    return True


class Ppp:
    def __init__(self, name):
        self.interface = TunTapInterface(name, mode_tun=True)
        exec(
            ["ip", "addr", "add", lbtest.PPP_ADDR_DEFAULT_GATEWAY + "/64", "dev", name]
        )
        exec(
            [
                "ip",
                "addr",
                "add",
                lbtest.PPP_ADDR_LL_LINUX + "/32",
                "peer",
                lbtest.PPP_ADDR_LL_ZEPHYR + "/32",
                "dev",
                name,
            ]
        )
        exec(["ip", "link", "set", name, "up"])

        self.rxqueue = []

        self.selector = selectors.DefaultSelector()
        self.selector.register(self.interface.fileno(), selectors.EVENT_READ, None)

    def __del__(self):
        self.close()

    def close(self):
        if self.selector is not None:
            self.selector.close()
            self.selector = None

        if self.interface is not None:
            self.interface.close()
            self.interface = None

    def send(self, packet):
        self.interface.send(packet)

    def recv(self, filter=filter_accept_all, blocking=True):
        for index, packet in enumerate(self.rxqueue):
            if filter(packet):
                self.rxqueue.pop(index)
                return packet

        while True:
            if not blocking:
                if self.selector is None:
                    return None
                if self.interface is None:
                    return None
                if len(self.selector.select(0)) == 0:
                    return None

            packet = self.interface.recv()
            # The Linux kernel sends those and we're generally not interested
            if ICMPv6ND_RS in packet:
                continue
            if filter(packet):
                return packet

            self.rxqueue.append(packet)

    def recv_tcp(self):
        return self.recv(lambda packet: TCP in packet)

    def recv_udp(self):
        return self.recv(lambda packet: UDP in packet)


@pytest.fixture
def ppp():
    try:
        ppp = Ppp("ppp0")
        # time.sleep(5)

        yield ppp
    finally:
        # time.sleep(5)
        ppp.close()
        del ppp


class TcpSocket:
    def __init__(self, interface, addr, port):
        self.interface = interface
        self.addr_local = addr
        self.port_local = port
        self.seq_local = random.randrange(0, 2**32)
        self.seq_remote = None
        self.addr_remote = None
        self.port_remote = None
        self._cached_packet = None

    def has_connection(self):
        return self.addr_remote is not None

    def recv(self):
        if self._cached_packet is not None:
            # return cached packet we got during last call to write_data()
            packet = self._cached_packet
            self._cached_packet = None
        else:
            packet = self.interface.recv_tcp()
        logging.debug(f"received: {packet.show(dump=True)}")

        return packet

    def wait_for_client(self):
        packet = self.recv()
        assert packet[IPv6].dst == self.addr_local
        assert packet[TCP].dport == self.port_local
        assert packet[TCP].flags == "S"

        self.seq_remote = packet[TCP].seq + 1
        self.addr_remote = packet[IPv6].src
        self.port_remote = packet[TCP].sport

        answer = IPv6(src=packet[IPv6].dst, dst=packet[IPv6].src) / TCP(
            sport=packet[TCP].dport,
            dport=packet[TCP].sport,
            seq=self.seq_local,
            ack=self.seq_remote,
            flags="SA",
            options=packet[TCP].options,
        )
        logging.debug(f"send synack: {answer.show(dump=True)}")
        self.interface.send(answer)
        self.seq_local += 1

        packet = self.recv()
        assert packet[TCP].sport == self.port_remote
        assert packet[TCP].dport == self.port_local
        assert packet[TCP].flags == "A"
        assert packet[TCP].seq == self.seq_remote
        assert packet[TCP].ack == self.seq_local

    def wait_for_disconnect(self):
        packet = self.recv()
        assert packet[TCP].sport == self.port_remote
        assert packet[TCP].dport == self.port_local
        assert packet[TCP].flags == "FA"
        assert packet[TCP].seq == self.seq_remote

        answer = IPv6(src=packet[IPv6].dst, dst=packet[IPv6].src) / TCP(
            sport=packet[TCP].dport,
            dport=packet[TCP].sport,
            seq=self.seq_local,
            ack=self.seq_remote + 1,
            flags="A",
            options=packet[TCP].options,
        )
        logging.debug(f"ack fin: {answer.show(dump=True)}")
        self.interface.send(answer)

        self.seq_local = random.randrange(0, 2**32)
        self.seq_remote = None
        self.addr_remote = None
        self.port_remote = None

    def read_data(self, ack=True):
        packet = self.recv()
        assert packet[TCP].sport == self.port_remote
        assert packet[TCP].dport == self.port_local
        assert packet[TCP].flags == "PA"
        assert packet[TCP].seq == self.seq_remote
        assert packet[TCP].ack == self.seq_local

        data = packet[Raw].load
        self.seq_remote += len(data)

        if ack:
            answer = IPv6(src=packet[IPv6].dst, dst=packet[IPv6].src) / TCP(
                sport=packet[TCP].dport,
                dport=packet[TCP].sport,
                seq=self.seq_local,
                ack=self.seq_remote,
                flags="A",
                options=packet[TCP].options,
            )
            logging.debug(f"ack data: {answer.show(dump=True)}")
            self.interface.send(answer)

        return data

    def write_data(self, data):
        answer = (
            IPv6(src=self.addr_local, dst=self.addr_remote)
            / TCP(
                sport=self.port_local,
                dport=self.port_remote,
                seq=self.seq_local,
                ack=self.seq_remote,
                flags="PA",
                options=[("NOP", 0), ("NOP", 0)],
            )
            / Raw(load=data)
        )
        logging.debug(f"send data: {answer.show(dump=True)}")
        self.interface.send(answer)
        self.seq_local += len(data)

        packet = self.recv()
        assert packet[TCP].sport == self.port_remote
        assert packet[TCP].dport == self.port_local
        assert (
            # if packet contains more data, push flag is set
            packet[TCP].flags == "A"
            or packet[TCP].flags == "PA"
        )
        assert packet[TCP].seq == self.seq_remote
        assert packet[TCP].ack == self.seq_local
        if packet.getlayer(Raw) is not None:
            # Instead of getting a separate ACK, we got an ACK combined with more data (this is likely timing-dependent;
            # if lemonbeatd already has more data to send, it will be combined with the ACK). We need to save it for the
            # next call to recv().
            self._cached_packet = packet

    def handle_command(
        self,
        expected_id,
        expected_length=0,
        expected_payload=None,
        answer=b"\x01\x00\x00",
    ):
        # lemonbeatd always sends the first byte in a separate packet; thus we need two calls to read_data()
        data = self.read_data()
        assert len(data) == 1
        data += self.read_data()
        assert len(data) >= 2
        header = data[0:3]
        expected_header = (
            b"\x01"
            + expected_id.to_bytes(1, "little")
            + expected_length.to_bytes(1, "little")
        )
        assert header == expected_header

        if len(data) > 3:
            payload = data[3:]
        else:
            payload = None

        if expected_payload is not None:
            assert payload == expected_payload

        if answer is not None:
            self.write_data(answer)

        return payload


@pytest.fixture
def tcpserver(ppp):
    tcp = TcpSocket(ppp, lbtest.PPP_ADDR_LL_ZEPHYR, 8888)
    yield tcp


@pytest.fixture(autouse=True)
def ensure_gc():
    yield
    # make sure everything gets cleanup up. As our python code is only test code
    # we don't hide issues to be found in production later. With same luck reduces
    # test flakiness.
    gc.collect()


## make sure a clean instance of lemonbeatd is running
@pytest.fixture
def lemonbeatd(ppp, tcpserver, notify_socket):
    svc = lbtest.Lemonbeatd(tcpserver)

    try:
        # cleanup from other tests
        tz.set_timezone("Universal")
        svc.remove_workdir()

        svc.start()
        yield svc
    finally:
        svc.try_stop()


## The receiver end of a simulated systemd service notify socket
@pytest.fixture
def notify_socket():
    s = lbtest.NotifySocket("/tmp/lemonbeat_test.notify")
    yield s
    logging.debug("close notify socket")
    s.close()


## just a list that allows registering objects for cleanup
#
#  We only use this for sockets because that's something where we care about
#  when they get closed and can't rely on the garbage collector.
#  The object doesn't need to be a socket, it just needs a `close` function.
@pytest.fixture
def socket_cleanup():
    sockets = []
    yield sockets

    # some of the `close` functions check if there is unexpected pending data.
    # increase the chance of catching those by waiting a bit before closing the
    # sockets.
    time.sleep(0.1)

    for s in sockets:
        logging.debug(f"close socket {s}")
        s.close()


class Timedate(dbus.service.Object):
    @dbus.service.signal("org.freedesktop.DBus.Properties")
    def PropertiesChanged(self):
        logging.debug("raised PropertiesChanged signal")
        pass

    @dbus.service.method(
        "org.freedesktop.timedate1", in_signature="sb", out_signature=""
    )
    def SetTimezone(self, timezone, interactive):
        logging.debug(f"SetTimezone({timezone}, {interactive})")
        self.PropertiesChanged()


def run_glib_mainloop():
    mainloop = GLib.MainLoop()
    mainloop.run()


class DBusServices:
    def __init__(self, bus):
        self.timedate_busname = dbus.service.BusName("org.freedesktop.timedate1", bus)
        self.timedate = Timedate(bus, "/org/freedesktop/timedate1")


@pytest.fixture(scope="session")
def dbussvc():
    dbus.mainloop.glib.threads_init()
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)

    system_bus = dbus.SystemBus(private=True)
    svcs = DBusServices(system_bus)

    threading.Thread(target=run_glib_mainloop, daemon=True).start()

    yield svcs
