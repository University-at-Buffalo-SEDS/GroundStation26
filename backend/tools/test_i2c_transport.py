#!/usr/bin/env python3
"""Run the production Linux I2C methods without linking the full GUI/backend stack."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

def item(source, signature):
    start = source.index(signature)
    brace = source.index('{', start)
    depth = 1
    end = brace + 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]

def main():
    if sys.platform != 'linux':
        raise SystemExit('Run this transport harness on Linux; it exercises the Linux implementation.')
    root = Path(__file__).resolve().parents[1]
    source = (root / 'src/comms.rs').read_text()
    constants = '\n'.join(re.findall(r'^const (?:I2C_\w+|RAW_UART_\w+|STREAM_PACKET_MAX_SIZE)\b.*?;', source, re.M | re.S))
    methods = item(source, 'impl I2cComms {')
    methods = methods.replace(item(methods, 'pub fn open('), '')
    parts = [r'''
#![allow(dead_code, non_camel_case_types)]
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
type RouterSideId = u32;
mod libc {
    pub type c_ulong = std::os::raw::c_ulong;
    pub const ETIMEDOUT: i32 = 110;
    pub const EREMOTEIO: i32 = 121;
    pub const ENXIO: i32 = 6;
    unsafe extern "C" { pub fn ioctl(fd: i32, request: c_ulong, ...) -> i32; }
}
''', '#[path = '+json.dumps(str(root/'src/i2c_packet.rs'))+']\nmod i2c_packet;', constants]
    for signature in ['struct I2cMailboxSlot', 'struct I2cRxAssembly', 'impl I2cRxAssembly',
                      'fn encode_i2c_slot(', 'fn decode_i2c_slot(', 'pub struct I2cComms',
                      'fn build_raw_uart_frame(', 'fn build_link_frame(', 'fn parse_link_frame(',
                      'struct I2cMsg', 'struct I2cRdwrIoctlData', 'fn is_i2c_idle_read_error(']:
        prefix = '#[repr(C)]\n' if signature in ('struct I2cMsg', 'struct I2cRdwrIoctlData') else ''
        parts.append(prefix + item(source, signature))
    parts += [methods, item(source, 'mod tests {'), item(source, 'mod i2c_v2_transaction_tests {')]
    rustc = shutil.which('rustc') or str(Path.home()/'.cargo/bin/rustc')
    with tempfile.TemporaryDirectory(prefix='i2c-transport-test-') as directory:
        src = Path(directory)/'transport.rs'; exe = Path(directory)/'transport-test'
        src.write_text('\n'.join(parts))
        subprocess.run([rustc, '--edition=2024', '--test', '-C', 'debuginfo=0', str(src), '-o', str(exe)], check=True)
        subprocess.run([str(exe)], check=True)

if __name__ == '__main__': main()
