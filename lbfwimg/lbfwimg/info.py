# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import pathlib
import sys

import lbfwimg


def main(args):
    input = pathlib.Path(args.INPUT)

    with open(input, "rb") as f:
        file_size = input.stat().st_size
        reader = lbfwimg.FirmwareContainerReader(f, file_size)

        print(f"file_size: {file_size}", file=sys.stderr)
        print(f"firmware_version: `{reader.firmware_version}`", file=sys.stderr)
        print("images:", file=sys.stderr)
        while image := reader.next_image():
            length = image.length
            imgdata = image.read()
            checksum_calculated = lbfwimg.CRC16CALC.calculate_checksum(imgdata)
            print(
                f"\t slot={image.slot} crc16={checksum_calculated:04X} length={length}",
                file=sys.stderr,
            )


def args(parent):
    parser = parent.add_parser(
        "info", help="Show information about a firmware container"
    )
    parser.add_argument("INPUT", help="input firmware file")
    parser.set_defaults(func=main)
