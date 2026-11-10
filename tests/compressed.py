#!/usr/bin/env python3
"""Single compressed files and compressed tars, including sizes the format never stores."""

import bz2
import gzip
import io
import json
import lzma
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

REPO = Path(__file__).resolve().parents[1]
BINARY = REPO / 'target/debug/zippity'
# CI installs every tool and sets this, so nothing is skipped there.
STRICT = os.environ.get('ZIPPITY_STRICT_FIXTURES') == '1'
TEXT = b'compressed notes, long enough to compress a little: ' + b'abc' * 40 + b'\n'


def helper(value, success=True):
    output = subprocess.run([BINARY], input=json.dumps(value).encode(), capture_output=True, timeout=60)
    assert (output.returncode == 0) == success, (value, output.stderr.decode())
    return json.loads((output.stdout if success else output.stderr).splitlines()[-1])


def tool(name):
    found = shutil.which(name)
    assert found or not STRICT, f'missing fixture tool: {name}'
    return found


def through(command, data):
    return subprocess.run(command, input=data, check=True, capture_output=True).stdout


def single(archive, format, unknown):
    """The archive lists one member, named after it, which copies and verifies."""
    index = helper({'op': 'list', 'archive': str(archive)})
    assert index['source']['format'] == format, (archive.name, index['source'])
    [entry] = index['entries']
    assert entry['path'] == 'notes.txt', (archive.name, entry)
    assert entry.get('unknown_size', False) == unknown, (archive.name, entry)
    assert entry['size'] == (0 if unknown else len(TEXT)), (archive.name, entry)
    revision = index['source']['revision']
    assert helper({'op': 'check', 'archive': str(archive), 'revision': revision})['verified']
    destination = archive.parent / (archive.name + '.out')
    report = helper({'op': 'copy_many', 'archive': str(archive), 'revision': revision,
        'files': [{'member': 'notes.txt', 'destination': str(destination)}]})
    assert report == {'done': len(TEXT), 'damaged': [], 'lost': [], 'partial': []}, (archive.name, report)
    assert destination.read_bytes() == TEXT, archive.name


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    checked = []

    # Python's codecs write gzip with the original name, and bzip2 and lzma without sizes.
    with gzip.GzipFile(root / 'notes.txt.gz', 'wb', mtime=1_600_000_000) as output:
        output.write(TEXT)
    (root / 'notes.txt.bz2').write_bytes(bz2.compress(TEXT))
    (root / 'notes.txt.xz').write_bytes(lzma.compress(TEXT, format=lzma.FORMAT_XZ))
    (root / 'notes.txt.lzma').write_bytes(lzma.compress(TEXT, format=lzma.FORMAT_ALONE))
    for name, format, unknown in [('notes.txt.gz', 'gz', False), ('notes.txt.bz2', 'bz2', True),
            ('notes.txt.xz', 'xz', False), ('notes.txt.lzma', 'lzma', True)]:
        single(root / name, format, unknown)
        checked.append(name)
    if tool('zstd'):
        # Compressed from a pipe, the frame does not record the size.
        (root / 'notes.txt.zst').write_bytes(through(['zstd', '-q', '-c'], TEXT))
        single(root / 'notes.txt.zst', 'zst', True)
        checked.append('notes.txt.zst')
    if tool('compress'):
        (root / 'notes.txt.Z').write_bytes(through(['compress', '-c'], TEXT))
        single(root / 'notes.txt.Z', 'Z', True)
        checked.append('notes.txt.Z')
    if tool('lz4'):
        (root / 'notes.txt.lz4').write_bytes(through(['lz4', '-q', '-c'], TEXT))
        single(root / 'notes.txt.lz4', 'lz4', True)
        checked.append('notes.txt.lz4')

    # Compressed tars that 7zz decodes as an outer stream, and lz4 through its own tool.
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode='w') as archive:
        info = tarfile.TarInfo('folder/inside.txt')
        info.size = len(TEXT)
        archive.addfile(info, io.BytesIO(TEXT))
    tar = buffer.getvalue()
    tars = [('sample.tar.lzma', 'tar.lzma', lzma.compress(tar, format=lzma.FORMAT_ALONE))]
    if tool('compress'):
        tars.append(('sample.tar.Z', 'tar.Z', through(['compress', '-c'], tar)))
    if tool('lz4'):
        tars.append(('sample.tar.lz4', 'tar.lz4', through(['lz4', '-q', '-c'], tar)))
    # The name says nothing here, so the first decompressed block decides.
    tars += [('data.gz', 'tar.gz', gzip.compress(tar)), ('pack', 'tar.gz', gzip.compress(tar))]
    for name, format, data in tars:
        archive = root / name
        archive.write_bytes(data)
        index = helper({'op': 'list', 'archive': str(archive)})
        assert index['source']['format'] == format, (name, index['source'])
        assert 'folder/inside.txt' in {e['path'] for e in index['entries']}, (name, index['entries'])
        destination = root / (name + '.inside')
        helper({'op': 'copy', 'archive': str(archive), 'revision': index['source']['revision'],
            'member': 'folder/inside.txt', 'destination': str(destination)})
        assert destination.read_bytes() == TEXT, name
        checked.append(name)

    # A tar older than POSIX has no ustar mark, only a header checksum.
    header = bytearray(tar[:512])
    header[257:512] = bytes(255)
    header[148:156] = b' ' * 8
    header[148:156] = b'%06o\0 ' % sum(header)
    old = root / 'oldtar.gz'
    old.write_bytes(gzip.compress(bytes(header) + tar[512:]))
    index = helper({'op': 'list', 'archive': str(old)})
    assert index['source']['format'] == 'tar.gz' and 'folder/inside.txt' in {e['path'] for e in index['entries']}, index

    # A compressed tar split into numbered volumes keeps its members.
    data = gzip.compress(tar)
    (root / 'split.tar.gz.001').write_bytes(data[:len(data) // 2])
    (root / 'split.tar.gz.002').write_bytes(data[len(data) // 2:])
    index = helper({'op': 'list', 'archive': str(root / 'split.tar.gz.001')})
    assert index['source']['format'] == 'tar.gz' and len(index['source']['volumes']) == 2, index['source']
    destination = root / 'split-inside.txt'
    helper({'op': 'copy', 'archive': str(root / 'split.tar.gz.001'), 'revision': index['source']['revision'],
        'member': 'folder/inside.txt', 'destination': str(destination)})
    assert destination.read_bytes() == TEXT

    # One output stream cannot be shared by size, so an unsized member goes alone.
    unsized = root / 'notes.txt.bz2'
    revision = helper({'op': 'list', 'archive': str(unsized)})['source']['revision']
    twice = [{'member': 'notes.txt', 'destination': str(root / name)} for name in ('first.txt', 'second.txt')]
    assert helper({'op': 'copy_many', 'archive': str(unsized), 'revision': revision, 'files': twice}, success=False)['kind'] == 'request'
    assert not (root / 'first.txt').exists() and not (root / 'second.txt').exists()

    blob = root / 'blob.gz'
    blob.write_bytes(gzip.compress(TEXT))
    assert helper({'op': 'list', 'archive': str(blob)})['source']['format'] == 'gz'
    print('Single compressed files and compressed tars open and copy:', ' '.join(checked))
