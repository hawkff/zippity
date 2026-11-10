#!/usr/bin/env python3
"""Packages, installers, and disk images that 7zz reads, built with their own tools."""

import gzip
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parents[1]
BINARY = REPO / 'target/debug/zippity'
# CI installs every fixture tool for its platform and sets this, so nothing is skipped there.
STRICT = os.environ.get('ZIPPITY_STRICT_FIXTURES') == '1'
PLATFORM = 'darwin' if sys.platform == 'darwin' else 'linux'
ENV = {**os.environ, 'PATH': os.environ['PATH'] + os.pathsep + '/usr/sbin' + os.pathsep + '/sbin'}
ROOT_TEXT, INNER_TEXT = 'hello from the root\n', 'nested file\n'


def helper(value, success=True):
    output = subprocess.run([BINARY], input=json.dumps(value).encode(), capture_output=True, timeout=120)
    assert (output.returncode == 0) == success, (value, output.stderr.decode())
    return json.loads((output.stdout if success else output.stderr).splitlines()[-1])


def run(*command, cwd=None):
    subprocess.run(command, cwd=cwd, check=True, capture_output=True, env=ENV)


def stuffit(items):
    """A classic StuffIt archive with stored data forks; a folder is (name, [items])."""
    def crc16(data):
        crc = 0
        for byte in data:
            crc ^= byte
            for _ in range(8):
                crc = (crc >> 1) ^ 0xA001 if crc & 1 else crc >> 1
        return crc
    def entry(name, data=b'', method=0):
        header = bytearray(112)
        header[0] = header[1] = method
        header[2] = len(name)
        header[3:3 + len(name)] = name
        header[66:74] = b'TEXTttxt'
        struct.pack_into('>IIII', header, 84, 0, len(data), 0, len(data))
        struct.pack_into('>HH', header, 100, 0, crc16(data))
        struct.pack_into('>H', header, 110, crc16(bytes(header[:110])))
        return bytes(header) + data
    def body(items):
        return b''.join(entry(name.encode(), method=32) + body(value) + entry(name.encode(), method=33)
            if isinstance(value, list) else entry(name.encode(), value.encode()) for name, value in items)
    content = body(items)
    return b'SIT!' + struct.pack('>HI', len(items), 22 + len(content)) + b'rLau\x01' + bytes(7) + content


def available(tools, platforms):
    missing = [tool for tool in tools if not shutil.which(tool, path=ENV['PATH'])]
    if not missing:
        return True
    assert not (STRICT and PLATFORM in platforms), f'missing fixture tools: {missing}'
    return False


def check(archive, files, hidden=0, format='other', at_least_hidden=False):
    """Lists, verifies, and extracts `files` ({member: text}) from `archive`."""
    index = helper({'op': 'list', 'archive': str(archive)})
    assert index['source']['format'] == format, (archive.name, index['source']['format'])
    paths = {entry['path'] for entry in index['entries'] if not entry['directory']}
    assert set(files) <= paths, (archive.name, sorted(paths))
    # The tree's link to root.txt is listed as a link where the listing shows its target,
    # and hidden where it does not.
    links = [entry for entry in index['entries'] if 'link' in entry]
    assert all(entry['link'] == 'root.txt' for entry in links), (archive.name, links)
    shown = index['hidden'] + len(links)
    assert shown >= hidden if at_least_hidden else shown == hidden, (archive.name, index['hidden'], links)
    revision = index['source']['revision']
    assert helper({'op': 'check', 'archive': str(archive), 'revision': revision})['verified']
    out = archive.parent / (archive.name + '.out')
    out.mkdir()
    targets = [{'member': member, 'destination': str(out / f'{number}.txt')} for number, member in enumerate(files)]
    report = helper({'op': 'copy_many', 'archive': str(archive), 'revision': revision, 'files': targets})
    assert report['damaged'] == [] and report['lost'] == [], (archive.name, report)
    for number, text in enumerate(files.values()):
        assert (out / f'{number}.txt').read_text() == text, archive.name
    return index


with tempfile.TemporaryDirectory() as temporary:
    work = Path(temporary)
    tree = work / 'tree'
    (tree / 'sub').mkdir(parents=True)
    (tree / 'root.txt').write_text(ROOT_TEXT)
    (tree / 'sub' / 'inner.txt').write_text(INNER_TEXT)
    os.symlink('root.txt', tree / 'link.txt')
    both, flat = {'root.txt': ROOT_TEXT, 'sub/inner.txt': INNER_TEXT}, {'root.txt': ROOT_TEXT}
    checked = []

    def fixture(name, tools, platforms=('linux', 'darwin')):
        def wrap(build):
            if PLATFORM in platforms and available(tools, platforms):
                build(work / name)
                checked.append(name)
        return wrap

    @fixture('sample.a', ['ar'])
    def _(archive):
        run('ar', 'rc', str(archive), 'root.txt', 'sub/inner.txt', cwd=tree)
        check(archive, {'root.txt': ROOT_TEXT, 'inner.txt': INNER_TEXT}, format='ar')

    @fixture('sample.cpio', ['cpio'])
    def _(archive):
        listing = subprocess.run(['find', '.'], cwd=tree, check=True, capture_output=True).stdout
        with archive.open('wb') as output:
            subprocess.run(['cpio', '-o', '-H', 'newc'], cwd=tree, input=listing, stdout=output, stderr=subprocess.DEVNULL, check=True)
        check(archive, both, hidden=1)

    @fixture('sample.deb', ['dpkg-deb'], ('linux',))
    def _(archive):
        package = work / 'deb'
        (package / 'DEBIAN').mkdir(parents=True)
        (package / 'DEBIAN' / 'control').write_text('Package: sample\nVersion: 1.0\nArchitecture: all\nMaintainer: x <x@example.com>\nDescription: sample\n')
        shutil.copytree(tree, package / 'usr/share/sample', symlinks=True)
        run('dpkg-deb', '--build', '--root-owner-group', str(package), str(archive))
        # A package opens at its own level. 7zz recognizes a deb by its debian-binary member
        # and lists only the control and data streams.
        index = helper({'op': 'list', 'archive': str(archive)})
        assert index['source']['format'] == 'ar', index['source']
        assert sorted(e['path'].split('.')[0] for e in index['entries']) == ['control', 'data'], index['entries']
        assert helper({'op': 'check', 'archive': str(archive), 'revision': index['source']['revision']})['verified']
        data = next(e['path'] for e in index['entries'] if e['path'].startswith('data.tar'))
        inner = work / data
        helper({'op': 'copy', 'archive': str(archive), 'revision': index['source']['revision'], 'member': data, 'destination': str(inner)})
        check(inner, {'usr/share/sample/root.txt': ROOT_TEXT}, hidden=1, format=helper({'op': 'list', 'archive': str(inner)})['source']['format'])

    @fixture('sample.arj', ['arj'], ('linux',))
    def _(archive):
        run('arj', 'a', '-r', str(archive), 'root.txt', 'sub', cwd=tree)
        check(archive, both)

    @fixture('sample.cab', ['gcab'], ('linux',))
    def _(archive):
        run('gcab', '-c', str(archive), 'root.txt', 'sub/inner.txt', cwd=tree)
        check(archive, both)

    @fixture('sample-setup.exe', ['makensis'], ('linux',))
    def _(archive):
        script = work / 'sample.nsi'
        script.write_text(f'OutFile "{archive}"\nInstallDir "$TEMP\\sample"\nSection\n  SetOutPath "$INSTDIR"\n  File "{tree / "root.txt"}"\nSectionEnd\n')
        run('makensis', '-V1', str(script))
        check(archive, flat)

    @fixture('sample.rpm', ['rpmbuild'], ('linux',))
    def _(archive):
        top = work / 'rpm'
        (top / 'SPECS').mkdir(parents=True)
        spec = top / 'SPECS' / 'sample.spec'
        spec.write_text('Name: sample\nVersion: 1.0\nRelease: 1\nSummary: sample\nLicense: MIT\nBuildArch: noarch\n'
            '%description\nsample\n%install\nmkdir -p %{buildroot}/usr/share/sample\n'
            f'cp {tree / "root.txt"} %{{buildroot}}/usr/share/sample/root.txt\n%files\n/usr/share/sample\n')
        run('rpmbuild', '--quiet', '--define', f'_topdir {top}', '-bb', str(spec))
        shutil.copy(next((top / 'RPMS' / 'noarch').glob('*.rpm')), archive)
        # 7zz follows the package into its compressed payload, a cpio archive.
        index = helper({'op': 'list', 'archive': str(archive)})
        payload = next(e['path'] for e in index['entries'] if e['path'].endswith('.cpio'))
        inner = work / 'payload.cpio'
        helper({'op': 'copy', 'archive': str(archive), 'revision': index['source']['revision'], 'member': payload, 'destination': str(inner)})
        check(inner, {'usr/share/sample/root.txt': ROOT_TEXT})

    @fixture('sample.wim', ['wimlib-imagex'], ('linux',))
    def _(archive):
        run('wimlib-imagex', 'capture', str(tree), str(archive))
        check(archive, both, hidden=1)

    @fixture('sample.squashfs', ['mksquashfs'], ('linux',))
    def _(archive):
        run('mksquashfs', str(tree), str(archive), '-quiet', '-noappend')
        check(archive, both, hidden=1)

    @fixture('sample.cramfs', ['mkfs.cramfs'], ('linux',))
    def _(archive):
        run('mkfs.cramfs', str(tree), str(archive))
        check(archive, both, hidden=1)

    @fixture('sample-ext.img', ['mke2fs'], ('linux',))
    def _(archive):
        with archive.open('wb') as output:
            output.truncate(8 * 1024 * 1024)
        run('mke2fs', '-q', '-F', '-t', 'ext4', '-d', str(tree), str(archive))
        check(archive, both, hidden=1)
        if available(['img2simg'], ('linux',)):
            sparse = work / 'sample.simg'
            run('img2simg', str(archive), str(sparse))
            check(sparse, both, hidden=1)

    @fixture('sample-fat.img', ['mkfs.vfat', 'mcopy'], ('linux',))
    def _(archive):
        with archive.open('wb') as output:
            output.truncate(8 * 1024 * 1024)
        run('mkfs.vfat', '-n', 'SAMPLE', str(archive))
        run('mcopy', '-s', '-i', str(archive), str(tree / 'root.txt'), str(tree / 'sub'), '::/')
        check(archive, both)
        # A GPT disk around the file system opens straight to its files, also in VM images.
        if available(['sfdisk'], ('linux',)):
            disk = work / 'sample-disk.img'
            with disk.open('wb') as output:
                output.truncate(10 * 1024 * 1024)
            subprocess.run(['sfdisk', '-q', str(disk)], input=b'label: gpt\nstart=2048, size=16384, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n',
                check=True, capture_output=True, env=ENV)
            with disk.open('r+b') as output:
                output.seek(2048 * 512)
                output.write(archive.read_bytes())
            check(disk, both)
            if available(['qemu-img'], ('linux',)):
                for kind, suffix in [('qcow2', 'qcow2'), ('vdi', 'vdi'), ('vpc', 'vhd'), ('vhdx', 'vhdx'), ('vmdk', 'vmdk')]:
                    image = work / f'sample-disk.{suffix}'
                    run('qemu-img', 'convert', '-q', '-f', 'raw', '-O', kind, str(disk), str(image))
                    check(image, both)

    @fixture('sample-ntfs.img', ['mkntfs', 'ntfscp'], ('linux',))
    def _(archive):
        with archive.open('wb') as output:
            output.truncate(16 * 1024 * 1024)
        run('mkntfs', '-q', '-F', '-f', str(archive))
        run('ntfscp', '-q', str(archive), str(tree / 'root.txt'), 'root.txt')
        check(archive, flat, at_least_hidden=True)

    @fixture('sample-hfs.dmg', ['hdiutil'], ('darwin',))
    def _(archive):
        run('hdiutil', 'create', '-quiet', '-fs', 'HFS+', '-volname', 'Sample', '-srcfolder', str(tree), str(archive))
        # The link and the file system's private directory, whose name ends in a carriage return.
        check(archive, {'Sample/root.txt': ROOT_TEXT, 'Sample/sub/inner.txt': INNER_TEXT}, hidden=2, at_least_hidden=True)
        compressed = work / 'sample-udzo.dmg'
        run('hdiutil', 'create', '-quiet', '-fs', 'HFS+', '-volname', 'Sample', '-format', 'UDZO', '-srcfolder', str(tree), str(compressed))
        check(compressed, {'Sample/root.txt': ROOT_TEXT}, hidden=2, at_least_hidden=True)

    @fixture('sample-apfs.dmg', ['hdiutil'], ('darwin',))
    def _(archive):
        run('hdiutil', 'create', '-quiet', '-fs', 'APFS', '-volname', 'Sample', '-srcfolder', str(tree), str(archive))
        check(archive, both, hidden=1)

    @fixture('sample.xar', ['xar'], ('darwin',))
    def _(archive):
        run('xar', '-cf', str(archive), 'root.txt', 'sub', cwd=tree)
        check(archive, both)

    @fixture('sample.pkg', ['pkgbuild'], ('darwin',))
    def _(archive):
        payload = work / 'pkg-root'
        payload.mkdir()
        (payload / 'root.txt').write_text(ROOT_TEXT)
        run('pkgbuild', '--quiet', '--root', str(payload), '--identifier', 'com.example.sample', '--version', '1.0', str(archive))
        index = helper({'op': 'list', 'archive': str(archive)})
        assert index['source']['format'] == 'other' and index['entries'], index

    @fixture('spanned.zip', ['zip'])
    def _(archive):
        # Info-ZIP segments: .z01 first, .zip last; random data keeps them apart.
        source = work / 'spanned-input'
        source.mkdir()
        payload = os.urandom(200_000)
        (source / 'payload.bin').write_bytes(payload)
        (source / 'note.txt').write_text(ROOT_TEXT)
        run('zip', '-q', '-s', '64k', str(archive), 'payload.bin', 'note.txt', cwd=source)
        index = helper({'op': 'list', 'archive': str(archive)})
        assert index['source']['format'] == 'zip' and len(index['source']['volumes']) == 4, index['source']
        assert helper({'op': 'list', 'archive': str(work / 'spanned.z01')})['source']['revision'] == index['source']['revision']
        revision = index['source']['revision']
        assert helper({'op': 'check', 'archive': str(archive), 'revision': revision})['verified']
        targets = [{'member': name, 'destination': str(work / f'spanned-{name}')} for name in ('payload.bin', 'note.txt')]
        report = helper({'op': 'copy_many', 'archive': str(archive), 'revision': revision, 'files': targets})
        assert report['done'] == len(payload) + len(ROOT_TEXT), report
        assert (work / 'spanned-payload.bin').read_bytes() == payload
        assert (work / 'spanned-note.txt').read_text() == ROOT_TEXT
        # A set of segments is read-only, like other volume sets.
        refused = helper({'op': 'apply', 'archive': str(archive), 'revision': revision,
            'changes': [{'kind': 'delete', 'path': 'note.txt'}]}, success=False)
        assert refused['kind'] == 'unsupported', refused
        assert len(helper({'op': 'list', 'archive': str(archive)})['entries']) == 2

    @fixture('sample.sit', ['lsar', 'unar'])
    def _(archive):
        archive.write_bytes(stuffit([('root.txt', ROOT_TEXT), ('inner.txt', INNER_TEXT), ('sub', [('deep.txt', ROOT_TEXT)])]))
        index = check(archive, {'root.txt': ROOT_TEXT, 'inner.txt': INNER_TEXT, 'sub/deep.txt': ROOT_TEXT}, format='unar')
        revision = index['source']['revision']
        # A member that fails its checksum is reported; the others still copy.
        broken = work / 'broken.sit'
        data = bytearray(archive.read_bytes())
        data[data.index(INNER_TEXT.encode())] ^= 1
        broken.write_bytes(bytes(data))
        revision = helper({'op': 'list', 'archive': str(broken)})['source']['revision']
        targets = [{'member': name, 'destination': str(work / f'broken-{name}')} for name in ('root.txt', 'inner.txt')]
        report = helper({'op': 'copy_many', 'archive': str(broken), 'revision': revision, 'files': targets})
        assert report == {'done': len(ROOT_TEXT), 'damaged': ['inner.txt'], 'lost': [], 'partial': []}, report
        assert (work / 'broken-root.txt').read_text() == ROOT_TEXT and not (work / 'broken-inner.txt').exists()
        # A partial copy keeps what unar wrote for the damaged member, unverified.
        target = [{'member': 'inner.txt', 'destination': str(work / 'kept-inner.txt')}]
        report = helper({'op': 'copy_many', 'archive': str(broken), 'revision': revision, 'files': target, 'keep_damaged': True})
        sidecar = work / 'kept-inner.txt.damaged'
        assert report == {'done': 0, 'damaged': ['inner.txt'], 'lost': [], 'partial': [str(sidecar)]}, report
        damaged = bytearray(INNER_TEXT.encode())
        damaged[0] ^= 1
        assert sidecar.read_bytes() == bytes(damaged) and sidecar.stat().st_mode & 0o777 == 0o600

    # Content decides, not the name: a ZIP named .bin opens, and plain text does not.
    disguised = work / 'disguised.bin'
    run('7zz', 'a', '-tzip', str(disguised), 'root.txt', cwd=tree)
    check(disguised, flat, format='zip')
    plain = work / 'notes'
    plain.write_text('plain text, not an archive\n')
    assert helper({'op': 'probe', 'path': str(disguised)}) == {'archive': True}
    assert helper({'op': 'probe', 'path': str(plain)}) == {'archive': False}
    assert helper({'op': 'list', 'archive': str(plain)}, success=False)['kind'] == 'not_archive'
    print('Packages, installers, file system and disk images open by content:', ' '.join(checked))
