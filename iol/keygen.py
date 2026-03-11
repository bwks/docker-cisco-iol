#!/usr/bin/env python3
"""Generate IOURC license file for Cisco IOL based on current hostname/hostid."""

import hashlib
import os
import struct
import socket


def generate_license():
    hostname = socket.gethostname()
    hostid = int(os.popen("hostid").read().strip(), 16)

    # IOL license algorithm
    ioukey = hostid
    for char in hostname:
        ioukey += ord(char)

    pad1 = b'\x4B\x58\x21\x81\x56\x7B\x0D\xF3\x21\x43\x9B\x7E\xAC\x1D\xE6\x8A'
    pad2 = b'\x80\x00\x00\x00\x04\x00\x00\x00\x0C\x04\x00\x00\x0C\x04\x00\x00'

    iession = struct.pack('i', ioukey) + pad1 + pad2
    md5 = hashlib.md5()
    md5.update(iession)
    iession = md5.hexdigest()[:16]

    key = '{}-{}-{}-{}'.format(iession[:4], iession[4:8], iession[8:12], iession[12:16])

    with open('/iol/iourc', 'w') as f:
        f.write('[license]\n')
        f.write('{} = {};\n'.format(hostname, key))

    print('Generated IOURC license for hostname: {}'.format(hostname))


if __name__ == '__main__':
    generate_license()
