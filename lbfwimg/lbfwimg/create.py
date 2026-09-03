# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

__version__ = "0.1.0"

import argparse
import pathlib
import sys

import lbfwimg


class ImageArg(argparse._AppendAction):
    def __call__(self, parser, namespace, values, option_string):
        d = {"slot": int(values[0]), "path": values[1]}
        return super().__call__(parser, namespace, d, option_string)


def main(args):
    dest = pathlib.Path(args.FILE)

    checksums_needle = []
    with open(dest, "wb") as f:
        builder = lbfwimg.FirmwareContainer(f, args.firmware_version)

        for image in args.image:
            path = pathlib.Path(image["path"])

            path_checksum = lbfwimg.checksum_path(path)
            path_checksum_exists = path_checksum.exists()
            if path_checksum_exists:
                checksum_file = lbfwimg.load_checksum(path_checksum)
            else:
                print(f"WARNING: no checksum file found for `{path}`", file=sys.stderr)

            with open(path, "rb") as f_image:
                imagedata = f_image.read()

            checksum_calculated = lbfwimg.CRC16CALC.calculate_checksum(imagedata)
            if path_checksum_exists:
                if checksum_calculated != checksum_file:
                    raise Exception(
                        f"crc16 checksum error. calculated:{checksum_calculated:X} file:{checksum_file:X}"
                    )

                checksums_needle.append(checksum_file)
            else:
                checksums_needle.append(checksum_calculated)

            builder.write_image(image["slot"], imagedata)

        builder.finish()

    # just a pedantic check that re-reads and verifies the whole file
    with open(dest, "rb") as f:
        reader = lbfwimg.FirmwareContainerReader(f, dest.stat().st_size)
        assert (
            reader.firmware_version == args.firmware_version
        ), f"unexpected version: {reader.firmware_version}"

        index = 0
        while image_container := reader.next_image():
            image_args = args.image[index]

            assert (
                image_container.slot == image_args["slot"]
            ), f"unexpected slot: {image['slot']}"

            checksum_needle = checksums_needle[index]
            checksum_calculated = lbfwimg.CRC16CALC.calculate_checksum(
                image_container.read()
            )

            assert (
                checksum_calculated == checksum_needle
            ), f"crc16 checksum error. calculated:{checksum_calculated:X} needle:{checksum_needle:X}"

            index = index + 1

        assert index == len(args.image)


def args(parent):
    parser = parent.add_parser("create", help="Generate lemonbeatd firmware image")
    parser.add_argument(
        "--firmware-version",
        required=True,
        help="firmware version to store in the container",
    )
    parser.add_argument(
        "--image",
        default=[],
        nargs=2,
        action=ImageArg,
        metavar=("SLOT", "PATH"),
        help="firmware image to add",
    )
    parser.add_argument("FILE", help="output file path")
    parser.set_defaults(func=main)
