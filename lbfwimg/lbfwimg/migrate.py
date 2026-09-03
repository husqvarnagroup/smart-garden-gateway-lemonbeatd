# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import pathlib
import shutil

import lbfwimg


def migrate_firmware(dest, src, version):
    with open(src, "rb") as f:
        srcdata = f.read()

    dest.parent.mkdir(parents=True, exist_ok=True)
    with open(dest, "wb") as f:
        lbfwimg.FirmwareContainer(f, version).write_image(1, srcdata).finish()

    checksum_file = lbfwimg.load_checksum(src.with_suffix(src.suffix + ".checksum"))

    with open(dest, "rb") as f:
        reader = lbfwimg.FirmwareContainerReader(f, dest.stat().st_size)
        assert (
            reader.firmware_version == version
        ), f"unexpected version: {reader.firmware_version}"

        img = reader.next_image()
        assert img is not None, "container has no images"
        assert img.slot == 1, f"unexpected slot: {img.slot}"

        imgdata = img.read()
        checksum_calculated = lbfwimg.CRC16CALC.calculate_checksum(imgdata)
        assert (
            checksum_calculated == checksum_file
        ), f"crc16 checksum error. calculated:{checksum_calculated:X} file:{checksum_file:X}"

        assert reader.next_image() is None


def main(args):
    input = pathlib.Path(args.INPUT)
    output = pathlib.Path(args.OUTPUT)

    if output.exists():
        if args.force:
            shutil.rmtree(args.OUTPUT)
        else:
            raise Exception("output directory does already exist")

    output.mkdir(exist_ok=False)

    for device_dir in input.iterdir():
        if not device_dir.is_dir():
            continue

        for version_dir in device_dir.iterdir():
            if not version_dir.is_dir():
                continue

            for firmware_path in version_dir.iterdir():
                if not firmware_path.is_file():
                    continue
                if firmware_path.name.endswith(".checksum"):
                    continue

                migrate_firmware(
                    output.joinpath(device_dir.name)
                    .joinpath(version_dir.name)
                    .joinpath(firmware_path.name),
                    firmware_path,
                    version_dir.name,
                )


def args(parent):
    parser = parent.add_parser(
        "migrate", help="Migrate OTAU firmware directory to lemonbeatd firmware images"
    )
    parser.add_argument(
        "--force", action="store_true", help="delete output directory if it exists"
    )
    parser.add_argument("INPUT", help="input OTAU firmware directory")
    parser.add_argument("OUTPUT", help="output lemonbeatd firmware directory")
    parser.set_defaults(func=main)
