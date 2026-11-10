#!/usr/bin/env python3
"""Symbolic links are kept when their targets stay inside the archive and the destination."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/zippity'


def call(op, good=True, **values):
    result = subprocess.run([BINARY], input=json.dumps({'op': op, **values}).encode(), capture_output=True, timeout=90)
    assert (result.returncode == 0) == good, (op, result.stderr.decode())
    return json.loads(result.stdout.splitlines()[-1]) if good else json.loads(result.stderr)


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    tree = root / 'tree'
    (tree / 'dir' / 'sub').mkdir(parents=True)
    (tree / 'dir' / 'sub' / 'file.txt').write_text('data\n')
    for name, target in (('rel', 'sub/file.txt'), ('up', '../dir/sub'), ('out', '../../outside'),
            ('abs', '/etc/passwd'), ('via', 'rel/x'), ('zchain', 'rel')):
        os.symlink(target, tree / 'dir' / name)
    archives = {'tar': root / 'links.tar', '7z': root / 'links.7z', 'zip': root / 'links.zip'}
    subprocess.run(['tar', 'cf', str(archives['tar']), 'dir'], cwd=tree, check=True)
    for format in ('7z', 'zip'):
        subprocess.run(['7zz', 'a', f'-t{format}', '-snl', '-bso0', '-bsp0', str(archives[format]), 'dir'], cwd=tree, check=True)
    for format, archive in archives.items():
        index = call('list', archive=str(archive))
        revision = index['source']['revision']
        links = {e['path']: e['link'] for e in index['entries'] if 'link' in e}
        assert links == {'dir/rel': 'sub/file.txt', 'dir/up': '../dir/sub', 'dir/zchain': 'rel'}, (format, links)
        # out leaves the archive, abs is absolute, and via passes through rel.
        assert index['hidden'] == 3, (format, index['hidden'])
        assert all(e['size'] == 0 for e in index['entries'] if 'link' in e)
        # The whole tree keeps both links, which stay inside the destination.
        whole = root / f'whole-{format}'
        for e in index['entries']:
            if e['directory']:
                (whole / e['path']).mkdir(parents=True, exist_ok=True)
        files = [{'member': e['path'], 'destination': str(whole / e['path'])} for e in index['entries'] if not e['directory']]
        report = call('copy_many', archive=str(archive), revision=revision, files=files, root=str(whole))
        assert report.get('skipped', []) == [] and report['done'] == len('data\n'), report
        assert os.readlink(whole / 'dir/rel') == 'sub/file.txt' and (whole / 'dir/rel').read_text() == 'data\n'
        assert os.readlink(whole / 'dir/up') == '../dir/sub'
        # zchain comes after rel, so its last step is a link this extraction made.
        assert os.readlink(whole / 'dir/zchain') == 'rel' and (whole / 'dir/zchain').read_text() == 'data\n'
        # Extracting again with overwrite replaces the links the first extraction made.
        report = call('copy_many', archive=str(archive), revision=revision, files=files, root=str(whole), overwrite=True)
        assert report.get('skipped', []) == [] and report['done'] == len('data\n'), report
        assert os.readlink(whole / 'dir/zchain') == 'rel' and (whole / 'dir/zchain').read_text() == 'data\n'
        # The folder's members alone: up would climb out of the destination, rel stays inside.
        alone = root / f'alone-{format}'
        alone.mkdir()
        files = [{'member': f'dir/{name}', 'destination': str(alone / name)} for name in ('rel', 'up')]
        report = call('copy_many', archive=str(archive), revision=revision, files=files, root=str(alone))
        assert report.get('skipped', []) == ['dir/up'] and not os.path.lexists(alone / 'up'), report
        assert os.readlink(alone / 'rel') == 'sub/file.txt'
        # Without a root no link is made, and reading a link is refused.
        nowhere = root / f'nowhere-{format}'
        report = call('copy_many', archive=str(archive), revision=revision, files=[{'member': 'dir/rel', 'destination': str(nowhere)}])
        assert report.get('skipped', []) == ['dir/rel'] and not os.path.lexists(nowhere), report
        error = call('read', good=False, archive=str(archive), member='dir/rel', revision=revision, offset=0, length=10)
        assert error['kind'] == 'unsupported', error
        error = call('copy', good=False, archive=str(archive), member='dir/rel', revision=revision, destination=str(nowhere))
        assert error['kind'] == 'unsupported' and not os.path.lexists(nowhere), error
    # A link already in the destination that leads elsewhere is not passed through.
    trap = root / 'trap'
    (trap / 'dir').mkdir(parents=True)
    os.symlink(str(root), trap / 'dir' / 'sub')
    index = call('list', archive=str(archives['tar']))
    report = call('copy_many', archive=str(archives['tar']), revision=index['source']['revision'],
        files=[{'member': 'dir/rel', 'destination': str(trap / 'dir' / 'rel')}], root=str(trap))
    assert report.get('skipped', []) == ['dir/rel'] and not os.path.lexists(trap / 'dir' / 'rel'), report
    # Nor is one at the target's last step.
    trap = root / 'trap-end'
    (trap / 'dir' / 'sub').mkdir(parents=True)
    os.symlink(str(root), trap / 'dir' / 'sub' / 'file.txt')
    report = call('copy_many', archive=str(archives['tar']), revision=index['source']['revision'],
        files=[{'member': 'dir/rel', 'destination': str(trap / 'dir' / 'rel')}], root=str(trap))
    assert report.get('skipped', []) == ['dir/rel'] and not os.path.lexists(trap / 'dir' / 'rel'), report
    # A link whose data needs a password hides only itself.
    mixed = root / 'mixed.zip'
    subprocess.run(['7zz', 'a', '-tzip', '-snl', '-bso0', '-bsp0', str(mixed), 'dir/rel'], cwd=tree, check=True)
    subprocess.run(['7zz', 'a', '-tzip', '-snl', '-pfixture-password', '-bso0', '-bsp0', str(mixed), 'dir/up'], cwd=tree, check=True)
    index = call('list', archive=str(mixed))
    assert {e['path'] for e in index['entries'] if 'link' in e} == {'dir/rel'} and index['hidden'] == 1, index
    index = call('list', archive=str(mixed), password='fixture-password')
    assert {e['path'] for e in index['entries'] if 'link' in e} == {'dir/rel', 'dir/up'}, index
    # Creation stores links as links; those that leave the selection are stored but hidden.
    for format in ['zip', '7z'] + (['rar'] if shutil.which('rar') else []):
        for password in (None, 'fixture-password'):
            created = root / f'created-{bool(password)}.{format}'
            call('create', sources=[str(tree / 'dir')], destination=str(created), format=format, options={'level': 5}, password=password)
            index = call('list', archive=str(created), password=password)
            links = {e['path']: e['link'] for e in index['entries'] if 'link' in e}
            assert links == {'dir/rel': 'sub/file.txt', 'dir/up': '../dir/sub', 'dir/zchain': 'rel'}, (format, links)
            assert index['hidden'] == 3, (format, index['hidden'])
    # A split set counts a link as itself, so a dangling one is stored too.
    result = call('create', sources=[str(tree / 'dir' / 'via')], destination=str(root / 'split.7z.parts'), format='7z',
        options={'level': 5, 'volume_bytes': 1024 * 1024})
    assert [e.get('link') for e in call('list', archive=result['path'])['entries']] == ['rel/x']
    # Save As keeps links, and an edit adds one that stays inside the archive.
    clean = root / 'clean'
    (clean / 'pack').mkdir(parents=True)
    (clean / 'pack' / 'file.txt').write_text('data\n')
    os.symlink('file.txt', clean / 'pack' / 'link')
    archive = root / 'clean.zip'
    call('create', sources=[str(clean / 'pack')], destination=str(archive), format='zip', options={'level': 5})
    index = call('list', archive=str(archive))
    result = call('convert', archive=str(archive), revision=index['source']['revision'], destination=str(root / 'clean.7z'),
        format='7z', options={'level': 5})
    copied = {e['path']: e.get('link') for e in call('list', archive=result['path'])['entries']}
    assert copied == {'pack': None, 'pack/file.txt': None, 'pack/link': 'file.txt'}, copied
    assert not list(root.glob('.zippity-write-*'))
    extra = root / 'extra'
    extra.mkdir()
    os.symlink('pack/file.txt', extra / 'inside')
    os.symlink('/etc/passwd', extra / 'outside')
    saved = call('apply', archive=str(archive), revision=index['source']['revision'], changes=call('stage', sources=[str(extra / 'inside')]))
    assert any(e['path'] == 'inside' and e.get('link') == 'pack/file.txt' for e in saved['entries']), saved['entries']
    error = call('plan', good=False, archive=str(archive), revision=saved['source']['revision'], changes=call('stage', sources=[str(extra / 'outside')]))
    assert error['kind'] == 'unsupported', error
    print('Links inside the archive and the destination are kept; others are left out')
