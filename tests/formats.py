#!/usr/bin/env python3
import collections
import gzip
import io
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/zippity'

def request(value, success=True, env=None):
    output = subprocess.run([BINARY], input=json.dumps(value).encode(), capture_output=True, timeout=60, env=env)
    assert output.returncode == 0 if success else output.returncode != 0, output.stderr.decode()
    return output

with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    base = root / 'input'
    base.mkdir()
    (base / 'script.sh').write_text('echo archive\n')
    (base / 'script.sh').chmod(0o751)
    os.utime(base / 'script.sh', (1_600_000_000, 1_600_000_000))
    subprocess.run(['7zz', 'a', '-t7z', str(root / 'sample.7z'), 'script.sh'], cwd=base, check=True, capture_output=True)
    for suffix, mode in [('tar','w'), ('tar.gz','w:gz'), ('tar.bz2','w:bz2'), ('tar.xz','w:xz')]:
        with tarfile.open(root / ('sample.' + suffix), mode) as archive:
            archive.add(base, arcname='.')
    with (root / 'sample.tar.zst').open('wb') as output:
        subprocess.run(['zstd', '-q', '-c', str(root / 'sample.tar')], stdout=output, check=True)
    for suffix in ('7z','tar','tar.gz','tar.bz2','tar.xz','tar.zst'):
        archive = root / ('sample.' + suffix)
        listing = json.loads(request({'op':'list','archive':str(archive)}).stdout)
        entry = next(e for e in listing['entries'] if e['path'] == 'script.sh')
        assert entry['mode'] == 0o751, (suffix, entry)
        revision = listing['source']['revision']
        verified = request({'op':'check','archive':str(archive),'revision':revision})
        assert json.loads(verified.stdout)['verified']
        destination = root / ('out-' + suffix)
        request({'op':'copy','archive':str(archive),'revision':revision,'member':'script.sh','destination':str(destination),'preserve_metadata':True})
        assert destination.read_text() == 'echo archive\n'
        assert destination.stat().st_mode & 0o777 == 0o751
        assert abs(destination.stat().st_mtime - 1_600_000_000) < 1
    for kind in ('iso9660', 'udf', 'udf-late'):
        archive = root / (kind + '.ISO')
        fixture = 'udf' if kind.startswith('udf') else kind
        compressed = Path(__file__).resolve().parent / 'fixtures' / (fixture + '.iso.gz')
        original = gzip.decompress(compressed.read_bytes())
        if kind == 'udf-late':
            image = bytearray(original)
            for sector in range(16, 19):
                start = sector * 2048
                image[start:start + 2048] = bytes(2048)
                image[start:start + 8] = b'\0CDW02\x01\0'
            assert image[19 * 2048:19 * 2048 + 8] == b'\0BEA01\x01\0'
            original = bytes(image)
        archive.write_bytes(original)
        listing = json.loads(request({'op':'list','archive':str(archive)}).stdout)
        assert listing['source']['format'] == 'iso'
        assert {entry['path'] for entry in listing['entries']} == {'empty','folder','folder/file name.txt','notes.txt'}
        assert all(entry['size'] == 0 for entry in listing['entries'] if entry['directory'])
        revision = listing['source']['revision']
        request({'op':'check','archive':str(archive),'revision':revision})
        for member, content in [('notes.txt', b'ISO archive preview.\n'), ('folder/file name.txt', b'Nested ISO content.\n')]:
            response = request({'op':'read','archive':str(archive),'revision':revision,'member':member,'offset':2,'length':8})
            assert response.stdout == content[2:10]
            destination = root / (kind + '-' + Path(member).name)
            request({'op':'copy','archive':str(archive),'revision':revision,'member':member,'destination':str(destination)})
            assert destination.read_bytes() == content
        request({'op':'apply','archive':str(archive),'revision':revision,'changes':[{'kind':'delete','path':'notes.txt'}]}, success=False)
        request({'op':'copy','archive':str(archive),'revision':revision,'member':'notes.txt','destination':str(archive),'overwrite':True}, success=False)
        assert archive.read_bytes() == original
        cached = root / (kind + '-without-extension')
        archive.rename(cached)
        assert json.loads(request({'op':'list','archive':str(cached)}).stdout)['source']['format'] == 'iso'

    # One request copies many members with one listing and one decoding pass.
    selection = root / 'selection'
    for number in range(12):
        member = selection / f'folder{number % 3}' / f'{number}.txt'
        member.parent.mkdir(parents=True, exist_ok=True)
        member.write_text(f'member {number}\n' * (number + 1))
    (selection / 'empty.txt').touch()
    # Equal sizes leave only the content to tell these two apart.
    (selection / 'folder1' / 'equal-a.txt').write_text('alpha\n')
    (selection / 'folder2' / 'equal-b.txt').write_text('bravo\n')
    (selection / 'folder0').chmod(0o750)
    os.utime(selection / 'folder0', (1_500_000_000, 1_500_000_000))
    members = sorted(str(path.relative_to(selection)) for path in selection.rglob('*') if path.is_file())
    observer = root / 'observer'
    observer.mkdir()
    log, named = root / 'calls.log', root / 'named.log'
    wrapper = observer / '7zz'
    executable = shutil.which('7zz')
    # Each call logs its command, and how many members it names.
    wrapper.write_text('#!/usr/bin/env python3\nimport os, sys\n'
        f'open({str(log)!r}, "a").write(sys.argv[1] + "\\n")\n'
        f'open({str(named)!r}, "a").write(str(sum(a.startswith("-i!") for a in sys.argv)) + "\\n")\n'
        f'os.execv({executable!r}, [{executable!r}, *sys.argv[1:]])\n')
    wrapper.chmod(0o700)
    env = {**os.environ, 'PATH': str(observer) + os.pathsep + os.environ['PATH']}
    subprocess.run(['7zz', 'a', '-tzip', str(root / 'selection.zip'), '.'], cwd=selection, check=True, capture_output=True)
    subprocess.run(['7zz', 'a', '-t7z', str(root / 'selection.7z'), '.'], cwd=selection, check=True, capture_output=True)
    with tarfile.open(root / 'selection.tar.gz', 'w:gz') as archive:
        archive.add(selection, arcname='.')
    suffixes = ['zip', '7z', 'tar.gz']
    if shutil.which('rar'):
        subprocess.run(['rar', 'a', '-idq', '-s', '-r', str(root / 'selection.rar'), '*'], cwd=selection, check=True, capture_output=True)
        suffixes.append('rar')
    # Every other member, requested in reverse order, so the helper must restore archive order.
    chosen = sorted(set(members[::2]) | {'folder1/equal-a.txt', 'folder2/equal-b.txt'})
    for suffix in suffixes:
        archive = root / ('selection.' + suffix)
        revision = json.loads(request({'op':'list','archive':str(archive)}).stdout)['source']['revision']
        output = root / ('selection-' + suffix)
        for folder in ('folder0', 'folder1', 'folder2'):
            (output / folder).mkdir(parents=True)
        log.write_text('')
        result = request({'op':'copy_many','archive':str(archive),'revision':revision,'preserve_metadata':True,
            'files':[{'member':member,'destination':str(output / member)} for member in reversed(chosen)],
            'directories':[{'member':'folder0','destination':str(output / 'folder0')}]}, env=env)
        total = sum((selection / member).stat().st_size for member in chosen)
        assert json.loads(result.stdout.splitlines()[-1]) == {'done': total, 'damaged': [], 'lost': [], 'partial': []}
        for member in members:
            copied = output / member
            assert copied.read_bytes() == (selection / member).read_bytes() if member in chosen else not copied.exists(), (suffix, member)
        calls = collections.Counter(log.read_text().split())
        # Compressed tar adds one decompressor to the listing and to the extraction.
        assert calls == {'l': 1, 'x': 3 if suffix == 'tar.gz' else 1}, (suffix, calls)
        if suffix in ('7z', 'tar.gz'):
            folder = (output / 'folder0').stat()
            assert folder.st_mode & 0o777 == 0o750 and abs(folder.st_mtime - 1_500_000_000) < 1, (suffix, oct(folder.st_mode))
        # Every file of the archive at once is one pass that names none of them.
        output = root / ('everything-' + suffix)
        for folder in ('folder0', 'folder1', 'folder2'):
            (output / folder).mkdir(parents=True)
        log.write_text('')
        named.write_text('')
        result = request({'op':'copy_many','archive':str(archive),'revision':revision,
            'files':[{'member':member,'destination':str(output / member)} for member in reversed(members)]}, env=env)
        total = sum((selection / member).stat().st_size for member in members)
        assert json.loads(result.stdout.splitlines()[-1]) == {'done': total, 'damaged': [], 'lost': [], 'partial': []}
        assert all((output / member).read_bytes() == (selection / member).read_bytes() for member in members), suffix
        calls = collections.Counter(log.read_text().split())
        assert calls == {'l': 1, 'x': 3 if suffix == 'tar.gz' else 1}, (suffix, calls)
        assert set(named.read_text().split()) == {'0'}, (suffix, named.read_text())
    # A pass without names would also write the members a listing leaves out, so a selection
    # of every listed file beside Finder metadata or a hidden member still names its members.
    for extra in ('__MACOSX/._a.txt', '../outside.txt'):
        partly = root / 'partly-listed.zip'
        with zipfile.ZipFile(partly, 'w') as archive:
            for name, content in ((extra, 'left out'), ('a.txt', 'first\n'), ('b.txt', 'second\n')):
                archive.writestr(name, content)
        index = json.loads(request({'op':'list','archive':str(partly)}).stdout)
        assert [entry['path'] for entry in index['entries']] == ['a.txt', 'b.txt'], index['entries']
        named.write_text('')
        files = [{'member':name,'destination':str(root / ('partly-' + name))} for name in ('a.txt', 'b.txt')]
        result = request({'op':'copy_many','archive':str(partly),'revision':index['source']['revision'],'files':files,'overwrite':True}, env=env)
        assert json.loads(result.stdout.splitlines()[-1])['done'] == 13
        # The listing names nothing; the one pass for the copies names both.
        assert named.read_text().split()[:2] == ['0', '2'], (extra, named.read_text())
        assert (root / 'partly-a.txt').read_text() == 'first\n' and (root / 'partly-b.txt').read_text() == 'second\n'

    disc = root / 'batch.iso'
    disc.write_bytes(gzip.decompress((Path(__file__).resolve().parent / 'fixtures' / 'iso9660.iso.gz').read_bytes()))
    revision = json.loads(request({'op':'list','archive':str(disc)}).stdout)['source']['revision']
    expected = {'notes.txt': b'ISO archive preview.\n', 'folder/file name.txt': b'Nested ISO content.\n'}
    files = [{'member':member,'destination':str(root / ('disc-' + Path(member).name))} for member in reversed(sorted(expected))]
    result = request({'op':'copy_many','archive':str(disc),'revision':revision,'files':files})
    assert json.loads(result.stdout.splitlines()[-1]) == {'done': sum(map(len, expected.values())), 'damaged': [], 'lost': [], 'partial': []}
    assert all((root / ('disc-' + Path(member).name)).read_bytes() == data for member, data in expected.items())

    # A failed pass keeps every member that matches its listed size and CRC, so the damage is
    # found without decoding anything twice.
    damaged = root / 'damaged.zip'
    with zipfile.ZipFile(damaged, 'w') as archive:
        for name, content in (('a.txt', 'first\n'), ('b.txt', 'damaged member\n'), ('c.txt', 'last\n')):
            archive.writestr(name, content)
    broken = bytearray(damaged.read_bytes())
    broken[broken.index(b'damaged member')] ^= 1
    damaged.write_bytes(bytes(broken))
    revision = json.loads(request({'op':'list','archive':str(damaged)}).stdout)['source']['revision']
    files = [{'member':name,'destination':str(root / ('salvaged-' + name))} for name in ('c.txt', 'b.txt', 'a.txt')]
    log.write_text('')
    result = request({'op':'copy_many','archive':str(damaged),'revision':revision,'files':files}, env=env)
    assert json.loads(result.stdout.splitlines()[-1]) == {'done': 11, 'damaged': ['b.txt'], 'lost': [], 'partial': []}
    assert collections.Counter(log.read_text().split()) == {'l': 1, 'x': 1}
    assert (root / 'salvaged-a.txt').read_text() == 'first\n' and (root / 'salvaged-c.txt').read_text() == 'last\n'
    assert not (root / 'salvaged-b.txt').exists()
    # A partial copy keeps what 7zz produced for the damaged member: private and unverified.
    for name in ('a.txt', 'c.txt'):
        (root / ('salvaged-' + name)).unlink()
    log.write_text('')
    result = request({'op':'copy_many','archive':str(damaged),'revision':revision,'files':files,'keep_damaged':True}, env=env)
    sidecar = root / 'salvaged-b.txt.damaged'
    assert json.loads(result.stdout.splitlines()[-1]) == {'done': 11, 'damaged': ['b.txt'], 'lost': [], 'partial': [str(sidecar)]}
    assert collections.Counter(log.read_text().split()) == {'l': 1, 'x': 2}
    assert sidecar.read_bytes() == b'eamaged member\n' and sidecar.stat().st_mode & 0o777 == 0o600
    for name in ('a.txt', 'c.txt'):
        (root / ('salvaged-' + name)).unlink()
    sidecar.write_text('keep this')
    result = request({'op':'copy_many','archive':str(damaged),'revision':revision,'files':files,'keep_damaged':True})
    assert json.loads(result.stdout.splitlines()[-1])['partial'] == [] and sidecar.read_text() == 'keep this'
    alone = request({'op':'copy_many','archive':str(damaged),'revision':revision,
        'files':[{'member':'b.txt','destination':str(root / 'alone-b.txt')}]})
    assert json.loads(alone.stdout.splitlines()[-1]) == {'done': 0, 'damaged': ['b.txt'], 'lost': [], 'partial': []}
    request({'op':'copy','archive':str(damaged),'revision':revision,'member':'b.txt','destination':str(root / 'single-b.txt')}, success=False)
    assert not (root / 'alone-b.txt').exists() and not (root / 'single-b.txt').exists()

    # A bad CRC inside a solid 7z or RAR stream leaves the members after it intact.
    solid = root / 'solid-input'
    solid.mkdir()
    names = ['a.txt', 'b.txt', 'c.txt', 'd.txt']
    for name in names:
        (solid / name).write_text(f'{name} in a solid stream\n')
    makers = [('7z', ['7zz', 'a', '-t7z', '-m0=Copy', '-ms=2f'])]
    if shutil.which('rar'):
        makers.append(('rar', ['rar', 'a', '-idq', '-s', '-m0']))
    for suffix, command in makers:
        archive = root / ('solid-damaged.' + suffix)
        subprocess.run([*command, str(archive), *names], cwd=solid, check=True, capture_output=True)
        data = bytearray(archive.read_bytes())
        data[data.index(b'b.txt in a solid stream')] ^= 1
        archive.write_bytes(bytes(data))
        revision = json.loads(request({'op':'list','archive':str(archive)}).stdout)['source']['revision']
        log.write_text('')
        result = request({'op':'copy_many','archive':str(archive),'revision':revision,
            'files':[{'member':name,'destination':str(root / f'{suffix}-{name}')} for name in names]}, env=env)
        report = json.loads(result.stdout.splitlines()[-1])
        assert report == {'done': 3 * len('a.txt in a solid stream\n'), 'damaged': ['b.txt'], 'lost': [], 'partial': []}, (suffix, report)
        assert collections.Counter(log.read_text().split())['x'] == 1, suffix
        assert all((root / f'{suffix}-{name}').read_text() == f'{name} in a solid stream\n' for name in names if name != 'b.txt')

    # A copy keeps one temporary file open at a time, so selections can exceed the open-file limit.
    many = root / 'many'
    many.mkdir()
    for number in range(300):
        (many / f'{number}.txt').write_text(str(number))
    subprocess.run(['7zz', 'a', '-tzip', str(root / 'many.zip'), '.'], cwd=many, check=True, capture_output=True)
    revision = json.loads(request({'op':'list','archive':str(root / 'many.zip')}).stdout)['source']['revision']
    copies = root / 'many-copies'
    copies.mkdir()
    files = [{'member':f'{number}.txt','destination':str(copies / f'{number}.txt')} for number in range(300)]
    hard = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
    output = subprocess.run([BINARY], capture_output=True, timeout=60,
        input=json.dumps({'op':'copy_many','archive':str(root / 'many.zip'),'revision':revision,'files':files}).encode(),
        preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_NOFILE, (64, hard)))
    assert output.returncode == 0, output.stderr.decode()
    assert all((copies / f'{number}.txt').read_text() == str(number) for number in range(300))
    print('7z, tar families, ISO 9660/UDF images, integrity, metadata, and batched copies passed')
