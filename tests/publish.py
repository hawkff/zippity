#!/usr/bin/env python3
"""A copy is published only from the temporary file it was written to. exFAT and FAT
volumes on macOS give a file another number once it holds data, and can neither rename
without replacing nor link; extraction, creation, and saves work there all the same, and
still take only free names."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/zippity'


# Lists one member. Before it writes that member, it moves the destination folder away and
# leaves a link to another folder in its place, where a file has the temporary file's name.
SWAPPING = r'''#!/usr/bin/env python3
import os, sys
sys.stdin.buffer.read()
size = int(os.environ['SWAP_SIZE'])
if sys.argv[1] == 'l':
    print(f'Path = payload.txt\nSize = {size}\nFolder = -\n')
    sys.exit(0)
folder, planted = os.environ['SWAP_FOLDER'], os.environ['SWAP_PLANTED']
name = next(name for name in os.listdir(folder) if name.startswith('.zippity-'))
os.rename(folder, folder + '.moved')
os.mkdir(planted)
with open(os.path.join(planted, name), 'w') as other:
    other.write('injected')
os.symlink(planted, folder)
sys.stdout.buffer.write(b'hello'[:size])
'''


def call(op, good=True, env=None, **values):
    result = subprocess.run([BINARY], input=json.dumps({'op': op, **values}).encode(), capture_output=True, timeout=120, env=env)
    assert (result.returncode == 0) == good, (op, result.stderr.decode())
    return json.loads(result.stdout.splitlines()[-1]) if good else json.loads(result.stderr)


def names(folder):
    """What a folder holds, without the AppleDouble files macOS may add on FAT volumes."""
    return sorted(path.name for path in folder.iterdir() if not path.name.startswith('._'))


def stored(archive):
    """Member names as the archive stores them."""
    listing = subprocess.run(['7zz', 'l', '-slt', '-ba', str(archive)], capture_output=True, text=True, check=True).stdout
    return sorted(line[len('Path = '):] for line in listing.splitlines() if line.startswith('Path = '))


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    decoy, swapping = root / 'decoy.zip', root / 'swapping'
    decoy.write_bytes(b'PK\x03\x04fixture')
    swapping.mkdir()
    (swapping / '7zz').write_text(SWAPPING)
    (swapping / '7zz').chmod(0o700)

    def swapped(folder, planted, size):
        """Copies a member of `size` bytes while its folder is swapped for a link to `planted`."""
        folder.mkdir()
        env = {**os.environ, 'PATH': str(swapping) + os.pathsep + os.environ['PATH'],
            'SWAP_FOLDER': str(folder), 'SWAP_PLANTED': str(planted), 'SWAP_SIZE': str(size)}
        revision = call('list', env=env, archive=str(decoy))['source']['revision']
        error = call('copy_many', good=False, env=env, archive=str(decoy), revision=revision,
            files=[{'member': 'payload.txt', 'destination': str(folder / 'payload.txt')}])
        assert error['message'] == 'A temporary file was replaced', (size, error)
        assert folder.is_symlink() and names(planted) == [], (size, names(planted))

    # The other file is refused when the member is written into it, and for an empty
    # member, which nothing writes, before it would be published.
    for size in (5, 0):
        swapped(root / f'swapped-{size}', root / f'planted-{size}', size)
    if sys.platform != 'darwin':
        print('A replaced temporary file is refused; FAT volumes are checked on macOS, where hdiutil mounts them')
        sys.exit(0)

    members = {'a.txt': 'first\n', 'b.txt': 'damaged member\n', 'empty.txt': ''}
    good, damaged = root / 'good.zip', root / 'damaged.zip'
    with zipfile.ZipFile(good, 'w') as out:
        for name, text in members.items():
            out.writestr(name, text)
    broken = bytearray(good.read_bytes())
    broken[broken.index(b'damaged member')] ^= 1
    damaged.write_bytes(bytes(broken))
    (root / 'random.bin').write_bytes(os.urandom(3 * 1024 * 1024))
    beside = root / 'beside.zip'
    with zipfile.ZipFile(beside, 'w') as out:
        out.writestr('dir/b.txt', 'b\n')
        out.writestr('dir/._b.txt', 'metadata of b')
    for kind in ('ExFAT', 'MS-DOS FAT32'):
        image, volume = root / (kind + '.dmg'), root / kind
        volume.mkdir()
        subprocess.run(['hdiutil', 'create', '-quiet', '-size', '40m', '-fs', kind, '-volname', 'ZIPPITY', str(image)], check=True)
        subprocess.run(['hdiutil', 'attach', '-quiet', '-nobrowse', '-mountpoint', str(volume), str(image)], check=True)
        try:
            # New files, then the same files replaced.
            revision = call('list', archive=str(good))['source']['revision']
            files = [{'member': name, 'destination': str(volume / name)} for name in members]
            for overwrite in (False, True):
                report = call('copy_many', archive=str(good), revision=revision, files=files, overwrite=overwrite)
                assert report['done'] == sum(map(len, members.values())) and report['damaged'] == [], (kind, report)
                assert all((volume / name).read_text() == text for name, text in members.items()), kind
            assert call('copy_many', good=False, archive=str(good), revision=revision, files=files)['kind'] == 'exists'
            # A failed pass reads back what it wrote, and a partial copy takes a free name.
            out = volume / 'salvaged'
            out.mkdir()
            revision = call('list', archive=str(damaged))['source']['revision']
            files = [{'member': name, 'destination': str(out / name)} for name in members]
            for partial in ([str(out / 'b.txt.damaged')], []):
                report = call('copy_many', archive=str(damaged), revision=revision, files=files, keep_damaged=True, overwrite=True)
                assert report['damaged'] == ['b.txt'] and report['partial'] == partial, (kind, report)
            assert names(out) == ['a.txt', 'b.txt.damaged', 'empty.txt'], (kind, names(out))
            assert (out / 'a.txt').read_text() == 'first\n' and (out / 'b.txt.damaged').read_text() == 'eamaged member\n', kind
            # A new archive and a new volume set take only free names.
            made = volume / 'made.zip'
            request = {'sources': [str(volume / 'a.txt')], 'destination': str(made), 'format': 'zip', 'options': {'level': 5}}
            assert call('create', **request)['path'] == str(made)
            assert call('create', good=False, **request)['kind'] == 'exists'
            # macOS may keep a `._name` file beside each file here. A folder goes in without
            # them, and with the time it had.
            (volume / 'tree' / 'inner').mkdir(parents=True)
            (volume / 'tree' / 'inner' / 'x.txt').write_text('x\n')
            os.utime(volume / 'tree' / 'inner', (1_600_000_000, 1_600_000_000))
            packed = call('create', sources=[str(volume / 'tree')], destination=str(volume / 'tree.zip'), format='zip', options={'level': 5})
            assert stored(packed['path']) == ['tree', 'tree/inner', 'tree/inner/x.txt'], (kind, stored(packed['path']))
            inner = next(e for e in call('list', archive=packed['path'])['entries'] if e['path'] == 'tree/inner')
            assert abs(inner['modified'] - 1_600_000_000) < 3, (kind, inner)
            parts = volume / 'set.7z.parts'
            request = {'sources': [str(root / 'random.bin')], 'destination': str(parts), 'format': '7z',
                'options': {'level': 0, 'volume_bytes': 1024 * 1024}}
            first = call('create', **request)['path']
            assert len(names(parts)) >= 3 and call('list', archive=first)['entries'][0]['size'] == 3 * 1024 * 1024, (kind, names(parts))
            assert call('create', good=False, **request)['kind'] == 'exists'
            # A save replaces the archive, and Save As writes a new one beside it. Both stage
            # away from this volume, so no `._name` file of macOS gets in, and a `._name`
            # member of the archive keeps its own bytes.
            revision = call('list', archive=str(made))['source']['revision']
            changes = call('stage', sources=[str(volume / 'b.txt')]) + [{'kind': 'rename', 'from': 'a.txt', 'to': 'renamed.txt'}]
            saved = call('apply', archive=str(made), revision=revision, changes=changes)
            assert stored(made) == ['b.txt', 'renamed.txt'], (kind, stored(made))
            result = call('convert', archive=str(made), revision=saved['source']['revision'],
                destination=str(volume / 'converted.7z'), format='7z', options={'level': 5})
            assert stored(result['path']) == ['b.txt', 'renamed.txt'], (kind, stored(result['path']))
            revision = call('list', archive=str(beside))['source']['revision']
            result = call('convert', archive=str(beside), revision=revision, destination=str(volume / 'beside.7z'), format='7z', options={'level': 5})
            kept = subprocess.run(['7zz', 'e', '-so', result['path'], 'dir/._b.txt'], capture_output=True, check=True).stdout
            assert kept == b'metadata of b' and stored(result['path']) == ['dir', 'dir/._b.txt', 'dir/b.txt'], (kind, kept)
            leftovers = [path.name for path in volume.iterdir() if path.name.startswith(('.zippity-', '._.zippity-'))]
            assert not leftovers, (kind, leftovers)
            # A temporary file on an ordinary volume is not traded for a file on this one,
            # which has no owners to keep anyone out.
            swapped(root / ('swapped ' + kind), volume / 'planted', 5)
        finally:
            subprocess.run(['hdiutil', 'detach', '-quiet', '-force', str(volume)], check=True)
    print('A replaced temporary file is refused, and extraction, creation, and saves work on exFAT and FAT volumes')
