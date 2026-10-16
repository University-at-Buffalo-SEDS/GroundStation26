#!/usr/bin/env python3
"""Print DHCP + IPv4LL setup for Ubuntu; never change the live interface.

NetworkManager needs ipv4.link-local explicitly; Netplan's link-local setting
is only effective alongside DHCP with the networkd renderer.
"""
import argparse
import re
import shlex


def configuration(renderer: str, interface: str, connection: str | None = None, netplan_id: str | None = None) -> str:
    for value in (interface, netplan_id or interface):
        if not re.fullmatch(r'[a-zA-Z0-9_.:-]{1,64}', value):
            raise ValueError('Use a literal Ethernet interface / Netplan ID')
    if renderer == 'networkmanager':
        if not connection:
            raise ValueError('Pass the existing wired NetworkManager connection name')
        profile = shlex.quote(connection)
        return (f'sudo nmcli connection modify {profile} ipv4.method auto ipv4.link-local enabled\n'
                f'# Activate locally when ready; this can interrupt the network:\n'
                f'sudo nmcli connection up {profile} ifname {shlex.quote(interface)}\n')
    if renderer != 'networkd':
        raise ValueError('Choose networkd or networkmanager')
    return ('network:\n  version: 2\n  ethernets:\n'
            f'    {netplan_id or interface}:\n      dhcp4: true\n      link-local: [ipv4, ipv6]\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--renderer', required=True, choices=['networkd', 'networkmanager'], help='Existing Ubuntu network manager')
    parser.add_argument('--interface', default='eth0', help='Main wired Ethernet interface')
    parser.add_argument('--connection', help='Existing NetworkManager wired connection name')
    parser.add_argument('--netplan-id', help='Existing Netplan Ethernet definition ID, if different from interface')
    args = parser.parse_args()
    try:
        print(configuration(args.renderer, args.interface, args.connection, args.netplan_id), end='')
    except ValueError as error:
        parser.error(str(error))

if __name__ == '__main__':
    main()
