# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

import io
import json

import crc
import lbfwimg
import pytest

import lbtest

CRC32_ISO_HDLC = crc.Configuration(
    width=32,
    polynomial=0x04C11DB7,
    init_value=0xFFFFFFFF,
    final_xor_value=0xFFFFFFFF,
    reverse_input=True,
    reverse_output=True,
)
CONTENTTAG_CRC = crc.CrcCalculator(CRC32_ISO_HDLC, True)

image = [
    0xEF,
    0x62,
    0x9E,
    0xE4,  # fota header magic
    0x0E,
    0x00,  # image header size
    0x0F,
    0x00,  # hardware id
    0xFF,
    0x7D,  # image crc
    0x00,
    0x00,
    0x00,
    0x00,  # image size
    0xBA,
    0x1D,
    0x9A,
    0x7A,  # image header magic
    0x15,
    0x00,  # image header size
    0x00,
    0x00,
    0x00,
    0x00,  # image size
    0x00,
    0xC0,
    0x02,
    0x00,  # target address
    0x00,
    0x00,
    0x00,
    0x00,  # image flags
    0x02,
    0x04,
    0x04,  # image version
    0x98,
    0x26,  # image trailer magic
    0x04,
    0x00,  # trailer size
]


def fota_upload(
    ppp,
    tcpserver,
    lemonbeatd,
    notify_socket,
    socket_cleanup,
    use_cbtl=False,
    lost_status_report=False,
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    if use_cbtl is True:
        dev0 = lbtest.make_cbtl(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    else:
        dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)

    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    # using small chunks to easily test chunked upload
    firmware = lbtest.FirmwareUpdate(bytes(image), 0xB910, 10)
    container_raw = (
        lbfwimg.FirmwareContainer(io.BytesIO(), "2.4.4")
        .write_image(1, firmware.image)
        .finish()
        .getvalue()
    )

    # upload
    lbtest.ipc_fota_init(dev0.identifier(), ipc_cmd_sock, container_raw)

    # upload
    dev0.assert_firmware_init(firmware)
    dev0.assert_firmware_get_info(firmware, 0)

    # assert state 'downloading' and result 'success'
    dev0.assert_fota_event(ipc_event_sock, 1, 1, "2.4.4")

    # assert response to the IPC request
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is True

    dev0.assert_firmware_data(firmware, 0)
    dev0.assert_firmware_data(firmware, 1, lost_status_report)

    # send a request in the middle test ensure that this does not interfere
    # with the upload
    dev0.ipc_read_resource(ipc_cmd_sock, "lemonbeat/0/data_download_int/0")

    dev0.assert_firmware_data(firmware, 2)

    # Since we don't know if the request was received before or after the
    # request that starts uploading chunk 2, we only read the answer now.
    # If lemonbeatd would misbehave, the upload of chunk 3 would not happen.
    response = json.loads(ipc_cmd_sock.recv())
    assert len(response) == 1
    response = response[0]
    assert response["success"]

    dev0.assert_firmware_data(firmware, 3)

    # assert state 'download complete' and result 'idle'
    dev0.assert_fota_event(ipc_event_sock, 2, 0)
    return dev0


# FOT001
@pytest.mark.parametrize("use_cbtl", [True, False])
def test_fota_upload_base(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, use_cbtl
):
    fota_upload(
        ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, use_cbtl, False
    )


@pytest.mark.timeout(30)
def test_fota_upload_lost_status_report(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    fota_upload(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, False, True)


@pytest.mark.timeout(40)
def test_fota_upload_jump_to_end(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)

    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    # using small chunks to easily test chunked upload
    firmware = lbtest.FirmwareUpdate(bytes(image), 0xB910, 10)
    container_raw = (
        lbfwimg.FirmwareContainer(io.BytesIO(), "2.4.4")
        .write_image(1, firmware.image)
        .finish()
        .getvalue()
    )

    # upload
    lbtest.ipc_fota_init(dev0.identifier(), ipc_cmd_sock, container_raw)

    # upload
    dev0.assert_firmware_init(firmware)
    dev0.assert_firmware_get_info(firmware, 0)

    # assert state 'downloading' and result 'success'
    dev0.assert_fota_event(ipc_event_sock, 1, 1, "2.4.4")

    # assert response to the IPC request
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is True

    dev0.assert_firmware_data(firmware, 0)
    dev0.assert_firmware_data(firmware, 1, lost_status_report=True)
    dev0.assert_firmware_data(firmware, 2)
    dev0.assert_firmware_data(firmware, 3, lost_status_report=True)

    # assert state 'download complete' and result 'idle'
    dev0.assert_fota_event(ipc_event_sock, 2, 0)
    return dev0


# NOTE: lemonbeatd will flash the images in descending order, not the order
#       they appear in the container.
# LSU001
def test_fota_upload_multiple_images(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_cbtl(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    firmware = lbtest.FirmwareUpdate(bytes(image), 0xB910, 256)
    container_raw = (
        lbfwimg.FirmwareContainer(io.BytesIO(), "2.4.4")
        .write_image(1, firmware.image)
        .write_image(2, firmware.image)
        .finish()
        .getvalue()
    )

    # upload
    lbtest.ipc_fota_init(dev0.identifier(), ipc_cmd_sock, container_raw)

    # upload 1/2
    content_tag = CONTENTTAG_CRC.calculate_checksum(firmware.image)
    data_download_int = bytes([0x00, 0x00, 0x00, 0x02]) + content_tag.to_bytes(4, "big")

    dev0.assert_val_set("data_download_int", data_download_int)
    dev0.assert_firmware_init(firmware, id=2)
    dev0.assert_firmware_get_info(firmware, 0)

    # assert state 'downloading' and result 'success'
    dev0.assert_fota_event(ipc_event_sock, 1, 1, "2.4.4")

    # assert response to the IPC request
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is True

    dev0.assert_firmware_data(firmware)

    # upload 2/2
    dev0.assert_firmware_init(firmware, id=1)
    dev0.assert_firmware_get_info(firmware, 0)
    dev0.assert_firmware_data(firmware)

    # assert state 'download complete' and result 'idle'
    dev0.assert_fota_event(ipc_event_sock, 2, 0)


# TODO test where fota upload got interrupted in the middle. Real HW behavior?
# LSU004
def test_fota_upload_multi_secondary_already_uploaded(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)

    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    firmware = lbtest.FirmwareUpdate(bytes(image), 0xB910, 256)
    container_raw = (
        lbfwimg.FirmwareContainer(io.BytesIO(), "2.4.4")
        .write_image(1, firmware.image)
        .write_image(2, firmware.image)
        .finish()
        .getvalue()
    )

    # upload
    lbtest.ipc_fota_init(dev0.identifier(), ipc_cmd_sock, container_raw)

    # upload 1/2 : no upload will happen as firmware init signals a successful upload
    data_download_int = [0x00, 0x00, 0x00, 0x02, 0xFC, 0xB3, 0x52, 0x95]
    dev0.assert_val_set("data_download_int", data_download_int)
    dev0.assert_firmware_init(firmware, id=2, offset=str(firmware.size))

    # upload 2/2
    dev0.assert_firmware_init(firmware, id=1)
    dev0.assert_firmware_get_info(firmware, 0)

    # assert state 'downloading' and result 'success'
    dev0.assert_fota_event(ipc_event_sock, 1, 1, "2.4.4")

    # assert response to the IPC request
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is True

    dev0.assert_firmware_data(firmware)

    # assert state 'download complete' and result 'idle'
    dev0.assert_fota_event(ipc_event_sock, 2, 0)


# FOT002
@pytest.mark.parametrize("update_start_confirmation", [True])
@pytest.mark.parametrize("use_cbtl", [True, False])
def test_fota_flash_and_reinclude(
    ppp,
    tcpserver,
    lemonbeatd,
    notify_socket,
    socket_cleanup,
    update_start_confirmation,
    use_cbtl,
):
    dev0 = fota_upload(
        ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup, use_cbtl
    )
    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    firmware = lbtest.FirmwareUpdate(bytes(image), 0xB910, 256)

    # flash
    dev0.ipc_execute_resource(ipc_cmd_sock, "firmware_update/0/update")

    dev0.assert_firmware_get_info(firmware, firmware.size)

    # assert state 'updating' and result 'initial'
    dev0.assert_fota_event(ipc_event_sock, 3, 0, "")

    if use_cbtl:
        # reboot
        dev0.assert_val_set("command", 31.0)
        dev0.announce_devdesc()

    dev0.assert_firmware_update_start_with_status(
        firmware, 1, update_start_confirmation=update_start_confirmation
    )

    if update_start_confirmation:
        # assert state 'updating' and result 'success'
        dev0.assert_fota_event(ipc_event_sock, 3, 1)

    dev0.announce_devdesc()

    # assert that request was successful
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is True

    # re-inclusion after fota
    dev0.assert_device_nonce_reset(tcpserver)
    dev0.assert_inclusion(True)
    dev0.assert_meminfo_request()
    dev0.assert_valdesc_requests()
    dev0.assert_val_requests()

    # assert state 'idle' and result 'success'
    dev0.assert_fota_event(ipc_event_sock, 0, 1)

    lbtest.assert_ipc_endpoint(dev0, ipc_event_sock, update_result=1)

    dev0.assert_utc_update()
    lbtest.assert_utc_offset_change(ipc_event_sock, "UTC+00:00")


# FOT003
def test_fota_bad_otau_header(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    dev0 = fota_upload(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup)
    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()
    firmware = lbtest.FirmwareUpdate(image, 0xB910, 256)

    # flash
    dev0.ipc_execute_resource(ipc_cmd_sock, "firmware_update/0/update")

    dev0.assert_firmware_get_info(firmware, firmware.size)

    # assert state 'updating' and result 'initial'
    dev0.assert_fota_event(ipc_event_sock, 3, 0, "")

    # send back status 'data missing'
    dev0.assert_firmware_update_start_with_status(firmware, 8)

    # assert state 'idle' and result 'success'
    dev0.assert_fota_event(ipc_event_sock, 0, 8)

    # assert that request was unsuccessful
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is False


# FOT004
def test_fota_wrong_crc(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    dev0 = fota_upload(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup)
    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()
    firmware = lbtest.FirmwareUpdate(image, 0xB910, 256)

    # flash
    dev0.ipc_execute_resource(ipc_cmd_sock, "firmware_update/0/update")

    dev0.assert_firmware_get_info(firmware, firmware.size)

    # assert state 'updating' and result 'initial'
    dev0.assert_fota_event(ipc_event_sock, 3, 0, "")

    # send back status 'checksum error'
    dev0.assert_firmware_update_start_with_status(firmware, 4)

    # assert state 'idle' and result 'success'
    dev0.assert_fota_event(ipc_event_sock, 0, 5)

    # assert that request was unsuccessful
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is False


# FOT005
def test_fota_cancel(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup):
    dev0 = fota_upload(ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup)
    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    firmware = lbtest.FirmwareUpdate(bytes([0]), 0x0000, 0)

    # upload
    lbtest.ipc_fota_init(dev0.identifier(), ipc_cmd_sock, firmware.image)

    # assert state 'idle' result 'initial'
    dev0.assert_fota_event(ipc_event_sock, 0, 0, "")

    # assert that request was successful
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is True


def test_fota_flash_no_upload(
    ppp, tcpserver, lemonbeatd, notify_socket, socket_cleanup
):
    lbtest.include_radiomodule(tcpserver, notify_socket)
    ipc_cmd_sock = lemonbeatd.cmd_sock()
    ipc_event_sock = lemonbeatd.event_sock()

    dev0 = lbtest.make_simple_device(ppp, socket_cleanup, lbtest.IFADDR_DEV0)
    lbtest.include_device(
        tcpserver, ipc_cmd_sock, ipc_event_sock, notify_socket, socket_cleanup, dev0
    )

    # flash
    dev0.ipc_execute_resource(ipc_cmd_sock, "firmware_update/0/update")

    # assert that request was unsuccessful, as no upload happened before
    response = lbtest.wait_for_ipc_sock(ipc_cmd_sock)
    assert response[0]["success"] is False
