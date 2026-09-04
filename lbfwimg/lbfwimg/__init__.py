# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

__version__ = "0.1.0"

import argparse
import hashlib

import crc

from . import create, info, migrate

CRC16_XMODEM = crc.Configuration(
    width=16,
    polynomial=0x1021,
    init_value=0x0000,
    final_xor_value=0x0000,
    reverse_input=False,
    reverse_output=False,
)
CRC16CALC = crc.CrcCalculator(CRC16_XMODEM, True)
MAGIC = b"\x4c\x42\x46\x57"


class FirmwareContainer:
    def write(self, data):
        self.hasher.update(data)
        self.writer.write(data)

    def __init__(self, writer, firmware_version):
        self.writer = writer
        self.hasher = hashlib.sha512()

        # magic
        self.write(MAGIC)

        # format_version
        self.write(b"\x00")

        version_raw = firmware_version.encode("utf-8")
        self.write(len(version_raw).to_bytes(1, "little"))
        self.write(version_raw)

    def write_image(self, slot, data):
        self.write(slot.to_bytes(4, "little"))
        self.write(len(data).to_bytes(4, "little"))
        self.write(data)

        return self

    def finish(self):
        self.writer.write(self.hasher.digest())

        writer = self.writer

        self.writer = None
        self.hasher = None

        return writer


class FirmwareImageReader:
    def __init__(self, reader, slot, length):
        self.reader = reader
        self.slot = slot
        self.length = length

    def read(self, length=None):
        if length is None:
            length = self.length
        if length > self.length:
            raise Exception(
                "tried to read {length} bytes, but only {self.length} are available"
            )

        self.length -= length
        data = self.reader.read(length)

        return data


class FirmwareContainerReader:
    def delete_state(self):
        self.reader = None
        self.hasher = None
        self.bytes_left = 0
        self.image_bytes_left = 0

    def read(self, num):
        try:
            if self.image_bytes_left > 0 and num > self.image_bytes_left:
                raise Exception("read exceeds current image data")

            data = self.reader.read(num)

            if self.image_bytes_left > 0:
                self.image_bytes_left -= num
        except Exception as e:
            # make sure the user can't ignore exceptions
            self.delete_state()

            raise e

        self.hasher.update(data)
        self.bytes_left -= num

        return data

    def __init__(self, reader, filesize):
        self.bytes_left = filesize
        self.reader = reader
        self.hasher = hashlib.sha512()
        self.image_bytes_left = 0

        magic = self.read(4)
        if magic != MAGIC:
            raise Exception(f"invalid magic: `{magic}`")

        version = self.read(1)
        if version != b"\x00":
            raise Exception(f"unsupported version: `{version}`")

        fwver_length = int.from_bytes(self.read(1), byteorder="little")
        self.firmware_version = self.read(fwver_length).decode("utf-8")

    def next_image(self):
        if self.image_bytes_left > 0:
            raise Exception(f"`{self.image_bytes_left}` image bytes left")

        if self.bytes_left < 64:
            self.delete_state()
            raise Exception("not enough bytes left for checksum footer")
        elif self.bytes_left == 64:
            checksum_footer = self.reader.read(64)
            checksum_calculated = self.hasher.digest()

            if checksum_footer != checksum_calculated:
                self.delete_state()
                raise Exception(
                    f"invalid checksum. footer={checksum_footer} calculated={checksum_calculated}"
                )

            self.delete_state()
            return None
        else:
            slot = int.from_bytes(self.read(4), byteorder="little")
            length = int.from_bytes(self.read(4), byteorder="little")

            if length + 64 > self.bytes_left:
                e = Exception(
                    f"image size({length}) + 64 exceeds what's left from image({self.bytes_left})"
                )
                self.delete_state()
                raise e

            self.image_bytes_left = length
            return FirmwareImageReader(self, slot, length)


def load_checksum(src):
    with open(src, "r") as f:
        return int.from_bytes(bytes.fromhex(f.read()), byteorder="big")


def checksum_path(src):
    return src.with_suffix(src.suffix + ".checksum")


def parse_args():
    parser = argparse.ArgumentParser(
        description="Tool for all things related to lemonbeatd firmware image containers"
    )
    subparsers = parser.add_subparsers()

    create.args(subparsers)
    info.args(subparsers)
    migrate.args(subparsers)

    return parser.parse_args()


def main():
    args = parse_args()
    args.func(args)
