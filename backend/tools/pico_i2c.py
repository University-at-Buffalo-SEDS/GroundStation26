#!/usr/bin/env python3
"""Configure or flash the GroundStation-side RP2040 Pico. No automatic reboot."""
import argparse
import ctypes
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import time

MAGIC = 0xD2
SELECT = bytes([MAGIC, 0, 0, 0])
MAX_PAYLOAD = 4092

class Msg(ctypes.Structure):
    _fields_ = [('addr', ctypes.c_uint16), ('flags', ctypes.c_uint16),
                ('len', ctypes.c_uint16), ('buf', ctypes.POINTER(ctypes.c_uint8))]
class Transfer(ctypes.Structure):
    _fields_ = [('msgs', ctypes.POINTER(Msg)), ('nmsgs', ctypes.c_uint32)]

class Bus:
    def __init__(self, bus, addr):
        self.fd = os.open(f'/dev/i2c-{bus}', os.O_RDWR)
        self.addr = addr
    def close(self):
        os.close(self.fd)
    def transfer(self, data, read=False):
        buf = (ctypes.c_uint8 * len(data))(*data)
        msg = Msg(self.addr, 1 if read else 0, len(data), buf)
        transfer = Transfer(ctypes.pointer(msg), 1)
        fcntl.ioctl(self.fd, 0x0707, transfer)
        return bytes(buf)
    def write(self, data):
        return self.transfer(data)
    def read(self, size):
        return self.transfer(bytes(size), True)

def decode_header(raw):
    if len(raw) != 4 or raw[0] != MAGIC or raw[1] not in (0, 1, 2, 0x7E, 0x7F):
        raise ValueError('Pico does not speak I2C v2; flash the matching firmware first')
    length = int.from_bytes(raw[2:4], 'little')
    if length > MAX_PAYLOAD or (raw[1] == 0 and length):
        raise ValueError('invalid I2C v2 packet length')
    return raw[1], length

def select(bus):
    bus.write(SELECT)
    return decode_header(bus.read(4))

def validate_uf2(path):
    data = path.read_bytes()
    if not data or len(data) % 512:
        raise ValueError('not a complete UF2 file')
    blocks = len(data) // 512
    seen = set()
    for offset in range(0, len(data), 512):
        a, b, flags, addr, size, index, count, family = struct.unpack_from('<8I', data, offset)
        end, = struct.unpack_from('<I', data, offset + 508)
        if (a, b, end) != (0x0A324655, 0x9E5D5157, 0x0AB16F30):
            raise ValueError('invalid UF2 magic')
        if not flags & 0x2000 or family != 0xE48BFF56:
            raise ValueError('firmware must target RP2040')
        if flags & 1 or size != 256 or count != blocks or index in seen or index >= blocks:
            raise ValueError('invalid UF2 block layout')
        if not 0x10000000 <= addr < 0x10200000 or addr + size > 0x10200000:
            raise ValueError('UF2 block outside Pico flash')
        seen.add(index)
    return data

def clock_config(text, hz):
    # Remove previous settings from compound dtparam lines, then append a global override.
    lines = []
    for line in text.splitlines():
        if line.lstrip().startswith('dtparam='):
            key, value = line.split('=', 1)
            parts = [p for p in value.split(',') if not p.strip().startswith('i2c_arm_baudrate=')]
            if parts:
                lines.append(key + '=' + ','.join(parts))
        else:
            lines.append(line)
    return '\n'.join(lines).rstrip() + f'\n\n[all]\ndtparam=i2c_arm=on,i2c_arm_baudrate={hz}\n'

def write_backup(path, text):
    backup = path.with_name(path.name + f'.before-i2c-v2-{time.time_ns()}')
    shutil.copy2(path, backup)
    path.write_text(text)
    print(f'Updated {path}; backup: {backup}')

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bus', type=int, default=1)
    parser.add_argument('--addr', type=lambda v: int(v, 0), default=0x55)
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('probe', help='check v2 firmware; stop GroundStation before accessing its bus')
    sub.add_parser('bootloader', help='enter USB BOOTSEL using already-installed v2 firmware')
    flash = sub.add_parser('flash', help='flash a USB-connected Pico already in BOOTSEL')
    flash.add_argument('uf2', type=Path)
    flash.add_argument('--mount', type=Path, help='mounted RPI-RP2 drive; otherwise use picotool')
    setup = sub.add_parser('configure', help='select v2 and prepare 1 MHz; takes effect after restart/reboot')
    setup.add_argument('--boot-config', type=Path, default=Path('/boot/firmware/config.txt'))
    setup.add_argument('--comms-config', type=Path, required=True)
    setup.add_argument('--clock-hz', type=int, choices=[400000, 1000000], default=1000000)
    args = parser.parse_args()
    if args.command == 'flash':
        validate_uf2(args.uf2)
        if args.mount:
            info = args.mount / 'INFO_UF2.TXT'
            if not info.is_file() or 'RPI-RP2' not in info.read_text():
                raise ValueError('--mount must be an RP2040 RPI-RP2 bootloader drive')
            with (args.mount / 'firmware.uf2').open('wb') as out:
                out.write(args.uf2.read_bytes())
                out.flush()
                os.fsync(out.fileno())
            print('UF2 copied; verify the Pico reconnects and run probe before configuring v2.')
        else:
            if not shutil.which('picotool'):
                raise ValueError('install picotool or specify --mount /media/USER/RPI-RP2')
            subprocess.run(['picotool', 'load', '-v', '-x', str(args.uf2)], check=True)
        return
    if args.command == 'configure':
        cfg = json.loads(args.comms_config.read_text())
        if cfg['fill_box']['interface'] != 'i2c':
            raise ValueError('fill_box must already use I2C')
        # Verify the firmware before making persistent configuration changes.
        bus = Bus(cfg['fill_box'].get('bus', 1), cfg['fill_box'].get('addr', 0x55))
        try:
            select(bus)
        finally:
            bus.close()
        boot = clock_config(args.boot_config.read_text(), args.clock_hz)
        cfg['fill_box'].update(protocol_version=2, chunk_delay_ms=0, initial_wait_ms=0)
        cfg['fill_box'].pop('baud_rate', None)  # serial baud has no effect on Linux I2C
        write_backup(args.boot_config, boot)
        write_backup(args.comms_config, json.dumps(cfg, indent=2) + '\n')
        print('Configuration prepared. Reboot the Pi to apply the I2C clock, then restart GroundStation.')
        return
    bus = Bus(args.bus, args.addr)
    try:
        kind, length = select(bus)
        print(f'I2C v2 detected at 0x{args.addr:02x}; pending kind={kind}, bytes={length}')
        if args.command == 'bootloader':
            bus.write(bytes([MAGIC, 0x7E, 7, 0]) + b'BOOTSEL')
            print('BOOTSEL requested. Connect the Pico USB data cable to this Pi, then run flash.')
    finally:
        bus.close()

if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as err:
        raise SystemExit(str(err))
