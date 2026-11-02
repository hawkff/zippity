#!/usr/bin/env python3
"""macOS Finder metadata stays out of listings and moves with its members. On macOS,
archives keep Finder tags without other attributes, and extraction applies them."""
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import zipfile

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/zippity'
TAGS = 'com.apple.metadata:_kMDItemUserTags'


def call(op, good=True, **values):
    result = subprocess.run([BINARY], input=json.dumps({'op': op, **values}).encode(), capture_output=True, timeout=90)
    assert (result.returncode == 0) == good, (op, result.stderr.decode())
    return json.loads(result.stdout.splitlines()[-1]) if good else json.loads(result.stderr)


def stored(archive):
    """Member names as the archive stores them, folders included."""
    listing = subprocess.run(['7zz', 'l', '-slt', '-ba', str(archive)], capture_output=True, text=True, check=True).stdout
    return sorted(line[len('Path = '):] for line in listing.splitlines() if line.startswith('Path = '))


def shown(archive):
    return {e['path']: e.get('finder', False) for e in call('list', archive=str(archive))['entries']}


def attributes(path):
    return set(subprocess.run(['xattr', str(path)], capture_output=True, text=True, check=True).stdout.split())


def tag(path, name):
    value = plistlib.dumps([name], fmt=plistlib.FMT_BINARY).hex()
    subprocess.run(['xattr', '-wx', TAGS, value, str(path)], check=True)


def tags(path):
    value = subprocess.run(['xattr', '-px', TAGS, str(path)], capture_output=True, text=True, check=True).stdout
    return plistlib.loads(bytes.fromhex(''.join(value.split())))


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    finder = root / 'finder.zip'
    with zipfile.ZipFile(finder, 'w') as out:
        for name in ('__MACOSX/', '__MACOSX/dir/'):
            out.writestr(name, '')
        out.writestr('__MACOSX/dir/._a.txt', 'metadata of a')
        out.writestr('dir/a.txt', 'a\n')
        out.writestr('dir/b.txt', 'b\n')
        out.writestr('dir/._b.txt', 'metadata of b')
        out.writestr('._lonely', 'nothing beside it')
    index = call('list', archive=str(finder))
    expected = {'._lonely': False, 'dir': False, 'dir/a.txt': True, 'dir/b.txt': True}
    assert shown(finder) == expected and index['hidden'] == 0, index

    # Extraction writes the members, never their metadata as files.
    out = root / 'out'
    (out / 'dir').mkdir(parents=True)
    files = [{'member': f'dir/{name}.txt', 'destination': str(out / 'dir' / f'{name}.txt')} for name in 'ab']
    report = call('copy_many', archive=str(finder), revision=index['source']['revision'], files=files, root=str(out))
    assert report['done'] == 4 and sorted(path.name for path in (out / 'dir').iterdir()) == ['a.txt', 'b.txt'], report
    assert sorted(path.name for path in out.iterdir()) == ['dir']

    # Edits keep the metadata with its members.
    edited = root / 'edited.zip'
    shutil.copyfile(finder, edited)

    def apply(changes):
        revision = call('list', archive=str(edited))['source']['revision']
        return call('apply', archive=str(edited), revision=revision, changes=changes)

    apply([{'kind': 'rename', 'from': 'dir/a.txt', 'to': 'dir/c.txt'}])
    names = stored(edited)
    assert '__MACOSX/dir/._c.txt' in names and '__MACOSX/dir/._a.txt' not in names, names
    apply([{'kind': 'rename', 'from': 'dir', 'to': 'moved'}])
    names = stored(edited)
    assert {'moved/._b.txt', '__MACOSX/moved', '__MACOSX/moved/._c.txt'} <= set(names), names
    assert not [name for name in names if name.startswith(('dir', '__MACOSX/dir'))], names
    replacement = root / 'b.txt'
    replacement.write_text('new b\n')
    apply(call('stage', sources=[str(replacement)], prefix='moved'))
    assert 'moved/._b.txt' not in stored(edited)
    apply([{'kind': 'delete', 'path': 'moved/c.txt'}])
    assert '__MACOSX/moved/._c.txt' not in stored(edited)
    apply([{'kind': 'delete', 'path': 'moved'}])
    assert not [name for name in stored(edited) if 'moved' in name], stored(edited)
    assert shown(edited) == {'._lonely': False}

    # A member renamed or added beside a `._name` file makes that file its metadata.
    orphans = root / 'orphans.zip'
    with zipfile.ZipFile(orphans, 'w') as out:
        out.writestr('a', 'a\n')
        out.writestr('._b', 'metadata of b')
        out.writestr('._c', 'metadata of c')
    assert shown(orphans) == {'a': False, '._b': False, '._c': False}
    revision = call('list', archive=str(orphans))['source']['revision']
    added = root / 'c'
    added.write_text('c\n')
    changes = [{'kind': 'rename', 'from': 'a', 'to': 'b'}, *call('stage', sources=[str(added)])]
    planned = call('plan', archive=str(orphans), revision=revision, changes=changes)
    assert {entry['path']: entry.get('finder', False) for entry in planned} == {'b': True, 'c': True}, planned
    call('apply', archive=str(orphans), revision=revision, changes=changes)
    assert shown(orphans) == {'b': True, 'c': True} and {'._b', '._c'} <= set(stored(orphans))

    # Save As copies the metadata as it is stored.
    result = call('convert', archive=str(finder), revision=index['source']['revision'],
        destination=str(root / 'finder.7z'), format='7z', options={'level': 5})
    assert {'__MACOSX/dir/._a.txt', 'dir/._b.txt'} <= set(stored(result['path']))
    assert shown(result['path']) == expected

    # Creation leaves out Finder's folder icon files.
    icon = root / 'icon'
    icon.mkdir()
    (icon / 'Icon\r').write_bytes(b'')
    (icon / 'x.txt').write_text('x\n')
    call('create', sources=[str(icon)], destination=str(root / 'icon.zip'), format='zip', options={'level': 5})
    assert stored(root / 'icon.zip') == ['icon', 'icon/x.txt'], stored(root / 'icon.zip')

    if sys.platform == 'darwin':
        tree = root / 'tagged'
        tree.mkdir()
        (tree / 't.txt').write_text('tagged\n')
        tag(tree / 't.txt', 'Red\n6')
        tag(tree, 'Blue\n4')
        for name, value in (('com.apple.quarantine', '0081;00000000;Safari;'),
                ('com.apple.metadata:kMDItemWhereFroms', 'https://example.com/t.txt')):
            subprocess.run(['xattr', '-w', name, value, str(tree / 't.txt')], check=True)
        made = root / 'tagged.zip'
        call('create', sources=[str(tree)], destination=str(made), format='zip', options={'level': 5})
        assert {'__MACOSX/._tagged', '__MACOSX/tagged/._t.txt'} <= set(stored(made)), stored(made)
        packed = subprocess.run(['7zz', 'e', '-so', str(made), '__MACOSX/tagged/._t.txt'], capture_output=True, check=True).stdout
        assert b'_kMDItemUserTags' in packed and b'quarantine' not in packed and b'WhereFroms' not in packed
        assert shown(made) == {'tagged': True, 'tagged/t.txt': True}
        index = call('list', archive=str(made))
        out = root / 'tagged-out'
        (out / 'tagged').mkdir(parents=True)
        call('copy_many', archive=str(made), revision=index['source']['revision'], root=str(out),
            files=[{'member': 'tagged/t.txt', 'destination': str(out / 'tagged/t.txt')}],
            directories=[{'member': 'tagged', 'destination': str(out / 'tagged')}])
        assert tags(out / 'tagged/t.txt') == ['Red\n6'] and tags(out / 'tagged') == ['Blue\n4']
        assert not attributes(out / 'tagged/t.txt') & {'com.apple.quarantine', 'com.apple.metadata:kMDItemWhereFroms'}
        # Finder's own archives keep every attribute; extraction applies only the kept ones.
        ditto = root / 'ditto.zip'
        subprocess.run(['ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(tree), str(ditto)], check=True)
        index = call('list', archive=str(ditto))
        target = root / 'ditto-t.txt'
        call('copy_many', archive=str(ditto), revision=index['source']['revision'],
            files=[{'member': 'tagged/t.txt', 'destination': str(target)}])
        assert tags(target) == ['Red\n6'] and 'com.apple.quarantine' not in attributes(target)
        # Additions in an edit session bring their tags.
        extra = root / 'extra.txt'
        extra.write_text('extra\n')
        tag(extra, 'Green\n2')
        revision = call('list', archive=str(made))['source']['revision']
        call('apply', archive=str(made), revision=revision, changes=call('stage', sources=[str(extra)]))
        assert '__MACOSX/._extra.txt' in stored(made) and shown(made)['extra.txt']
    print('Finder metadata stays out of listings, moves with its members, and is kept on macOS')
