#!/usr/bin/env python3
import collections
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/zippity'

def call(op, good=True, env=None, **values):
    result = subprocess.run([BINARY], input=json.dumps({'op':op, **values}).encode(), capture_output=True, timeout=90, env=env)
    assert (result.returncode == 0) == good, (op, result.stderr.decode())
    if good and op == 'read':
        return result.stdout
    # Copies print progress lines before their result.
    return json.loads(result.stdout.splitlines()[-1]) if good else json.loads(result.stderr)

with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    source = root / 'files'
    source.mkdir()
    (source / 'folder').mkdir()
    (source / 'folder' / 'one.txt').write_text('one\n')
    (source / 'remove.txt').write_text('remove\n')
    (source / 'replace.txt').write_text('before\n')
    (source / 'empty.txt').touch()
    (source / 'empty-folder').mkdir()
    replacement = root / 'replace.txt'
    replacement.write_text('after\n')
    formats = ['zip', '7z'] + (['rar'] if shutil.which('rar') else [])
    for format in formats:
        for password in (None, 'fixture-password'):
            archive = root / (format + ('-encrypted' if password else '') + '.' + format)
            options = {'level':5, 'encrypt_headers':bool(password and format != 'zip')}
            created = call('create', sources=[str(source)], destination=str(archive), format=format, options=options, password=password)
            assert Path(created['path']) == archive
            index = call('list', archive=str(archive), password=password)
            assert index['source']['format'] == format
            assert {e['path'] for e in index['entries'] if not e['directory']} == {'files/folder/one.txt','files/remove.txt','files/replace.txt','files/empty.txt'}
            empty = next(e for e in index['entries'] if e['path'] == 'files/empty.txt')
            assert empty['encrypted'] == bool(password and format != '7z')
            assert any(e['path'] == 'files/empty-folder' and e['directory'] for e in index['entries'])
            if password:
                call('check', good=False, archive=str(archive), revision=index['source']['revision'], password='wrong')
            original = archive.read_bytes()
            additions = call('stage', sources=[str(replacement)], prefix='files')
            changes = [{'kind':'rename','from':'files/folder','to':'renamed'}, {'kind':'delete','path':'files/remove.txt'}, *additions]
            overlay = call('plan', archive=str(archive), revision=index['source']['revision'], password=password, changes=changes)
            assert 'renamed/one.txt' in {e['path'] for e in overlay}
            assert archive.read_bytes() == original
            updated = call('apply', archive=str(archive), revision=index['source']['revision'], password=password, changes=changes)
            assert {e['path'] for e in updated['entries'] if not e['directory']} == {'renamed/one.txt','files/replace.txt','files/empty.txt'}
            assert {e['path'] for e in updated['entries']} == {e['path'] for e in overlay}
            if options['encrypt_headers']:
                call('list', good=False, archive=str(archive))
            before_error = archive.read_bytes()
            call('apply', good=False, archive=str(archive), revision=updated['source']['revision'], password=password,
                changes=[{'kind':'rename','from':'renamed/one.txt','to':'../unsafe'}])
            assert archive.read_bytes() == before_error
            call('apply', good=False, archive=str(archive), revision=index['source']['revision'], password=password, changes=changes)
            assert archive.read_bytes() == before_error
    # Creation takes a method, dictionary, word size, and solid block size.
    def methods(archive):
        listing = subprocess.run(['7zz', 'l', '-slt', str(archive)], capture_output=True, text=True, check=True).stdout
        return {line.split(' = ', 1)[1] for line in listing.splitlines() if line.startswith(('Method = ', 'Solid = '))}
    # ZIP stores a file that compression would not shrink, so give it text that compresses.
    text = root / 'tuned.txt'
    text.write_text('zippity ' * 20000)
    for number, (format, settings, expected) in enumerate((
        ('7z', {'method':'ppmd', 'dictionary':'16m', 'word_size':16, 'solid':'off'}, ('PPMD:o16', '-')),
        ('7z', {'dictionary':'1m', 'word_size':64, 'solid':'4m'}, ('LZMA2', '+')),
        ('zip', {'method':'bzip2', 'dictionary':'900k'}, ('BZip2',)),
    )):
        target = root / f'tuned-{number}.{format}'
        call('create', sources=[str(source), str(text)], destination=str(target), format=format, options={'level':9, **settings})
        found = methods(target)
        assert all(any(value.startswith(want) for value in found) for want in expected), (settings, found)
    for format, settings in (('zip', {'method':'lzma2'}), ('zip', {'solid':'on'}), ('7z', {'method':'ppmd', 'word_size':40}), ('zip', {'method':'ppmd', 'word_size':17}),
            ('7z', {'dictionary':'64m -p'}), ('7z', {'dictionary':'64é'}), ('7z', {'method':'copy', 'dictionary':'1m'}), ('rar', {'method':'ppmd'})):
        error = call('create', good=False, sources=[str(source)], destination=str(root / f'refused.{format}'), format=format, options={'level':5, **settings})
        assert error['kind'] == 'request' and not (root / f'refused.{format}').exists(), (settings, error)
    (source / 'random.bin').write_bytes(os.urandom(3 * 1024 * 1024))
    for format in formats:
        directory = root / f'backup.{format}.parts'
        result = call('create', sources=[str(source / 'random.bin')], destination=str(directory), format=format,
            options={'level':0, 'volume_bytes':1024 * 1024}, password=None)
        assert len(list(directory.iterdir())) >= 3
        assert all(path.name.startswith('backup.') for path in directory.iterdir()), list(directory.iterdir())
        index = call('list', archive=result['path'])
        assert index['entries'][0]['size'] == 3 * 1024 * 1024
        call('create', good=False, sources=[str(source / 'random.bin')], destination=str(directory), format=format,
            options={'level':0, 'volume_bytes':1024 * 1024}, password=None)
    readonly = root / 'readonly'
    readonly.mkdir()
    (readonly / 'data.txt').write_text('Private staged input.\n')
    readonly.chmod(0o555)
    try:
        call('create', sources=[str(readonly)], destination=str(root / 'readonly.zip'), format='zip', options={'level':5})
        assert not list(root.glob('.zippity-write-*')), 'Successful creation left private staged inputs'
        call('create', good=False, sources=[str(readonly)], destination=str(root / 'invalid.zip'), format='zip', options={'level':5, 'encrypt_headers':True})
        assert not list(root.glob('.zippity-write-*')), 'Failed creation left private staged inputs'
    finally:
        readonly.chmod(0o755)
    batch = root / 'batch'
    batch.mkdir()
    for number in range(50):
        (batch / f'{number}.txt').write_text(str(number))
    (batch / '.hidden').write_text('hidden\n')
    (batch / 'empty.txt').touch()
    (batch / '資料').mkdir()
    (batch / '資料' / 'é.txt').write_text('unicode\n')
    (batch / 'empty').mkdir()
    os.utime(batch, (1_600_000_000, 1_600_000_000))
    batch.chmod(0o555)
    observer = root / 'backend-observer'
    observer.mkdir()
    log = root / 'backend-calls.jsonl'
    env = {**os.environ, 'PATH': str(observer) + os.pathsep + os.environ['PATH']}
    for backend in ('7zz', 'rar'):
        executable = shutil.which(backend)
        if executable:
            wrapper = observer / backend
            wrapper.write_text(
                '#!/usr/bin/env python3\nimport json, os, sys\n'
                f'with open({str(log)!r}, "a") as out: out.write(json.dumps([{backend!r}, sys.argv[1]]) + "\\n")\n'
                f'os.execv({executable!r}, [{executable!r}, *sys.argv[1:]])\n'
            )
            wrapper.chmod(0o700)
    try:
        for format in formats:
            archive = root / ('batch.' + format)
            call('create', sources=[str(replacement)], destination=str(archive), format=format, options={'level':5}, password='fixture-password')
            index = call('list', archive=str(archive), password='fixture-password')
            changes = call('stage', sources=[str(batch)])
            planned = call('plan', archive=str(archive), revision=index['source']['revision'], password='fixture-password', changes=changes)
            log.write_text('')
            updated = call('apply', archive=str(archive), revision=index['source']['revision'], password='fixture-password', changes=changes, env=env)
            counts = collections.Counter(tuple(json.loads(line)) for line in log.read_text().splitlines())
            backend = 'rar' if format == 'rar' else '7zz'
            assert counts[backend, 'a'] == 1, counts
            assert counts['7zz', 'l'] <= 4, counts
            assert {e['path'] for e in updated['entries']} == {e['path'] for e in planned}
            directory = next(e for e in updated['entries'] if e['path'] == 'batch')
            assert directory['mode'] == 0o555 and abs(directory['modified'] - 1_600_000_000) < 1, directory
            assert not list(root.glob('.zippity-write-*'))
            additions = call('stage', sources=[str(replacement)], prefix='batch')
            updated = call('apply', archive=str(archive), revision=updated['source']['revision'], password='fixture-password', changes=additions)
            retained = next(e for e in updated['entries'] if e['path'] == 'batch')
            assert (retained['mode'], retained['modified']) == (directory['mode'], directory['modified'])
            deleted = [f'batch/{number}.txt' for number in range(50)]
            log.write_text('')
            updated = call('apply', archive=str(archive), revision=updated['source']['revision'], password='fixture-password',
                changes=[{'kind':'delete','path':path} for path in deleted], env=env)
            counts = collections.Counter(tuple(json.loads(line)) for line in log.read_text().splitlines())
            assert counts[backend, 'd'] == 1, counts
            assert set(deleted).isdisjoint(e['path'] for e in updated['entries'])

            early, late = root / 'early.txt', root / 'late.txt'
            early.write_text('early\n')
            late.write_text('late\n')
            first = call('stage', sources=[str(early)])[0]
            last = call('stage', sources=[str(late)])[0]
            first['path'] = last['path'] = 'added/inner/file.txt'
            changes = [first, last, {'kind':'rename','from':'added','to':'renamed'},
                first, {'kind':'delete','path':'batch/.hidden'}]
            planned = call('plan', archive=str(archive), revision=updated['source']['revision'], password='fixture-password', changes=changes)
            saved = call('apply', archive=str(archive), revision=updated['source']['revision'], password='fixture-password', changes=changes)
            assert {e['path'] for e in saved['entries']} == {e['path'] for e in planned}
            assert call('read', archive=str(archive), revision=saved['source']['revision'], password='fixture-password',
                member='added/inner/file.txt', offset=0, length=100) == b'early\n'
            assert call('read', archive=str(archive), revision=saved['source']['revision'], password='fixture-password',
                member='renamed/inner/file.txt', offset=0, length=100) == b'late\n'
            for change in (
                {'kind':'rename','from':'renamed','to':'replace.txt/blocked'},
                dict(last, path='replace.txt/blocked'),
                dict(last, path='added'),
            ):
                error = call('plan', good=False, archive=str(archive), revision=saved['source']['revision'], password='fixture-password', changes=[change])
                assert error['kind'] == 'conflict', error
            before = archive.read_bytes()
            stale = call('stage', sources=[str(late)])
            late.write_text('changed\n')
            error = call('apply', good=False, archive=str(archive), revision=saved['source']['revision'], password='fixture-password', changes=stale)
            assert error['kind'] == 'changed', error
            assert archive.read_bytes() == before
    finally:
        batch.chmod(0o755)

    # Save As copies every member, with its metadata, into a new archive.
    encrypted = root / '7z-encrypted.7z'
    index = call('list', archive=str(encrypted), password='fixture-password')
    for format, password in (('zip', 'new-password'), ('7z', None)):
        target = root / f'converted.{format}'
        result = call('convert', archive=str(encrypted), revision=index['source']['revision'], password='fixture-password',
            destination=str(target), format=format, options={'level':5}, new_password=password)
        converted = call('list', archive=result['path'], password=password)
        assert {e['path'] for e in converted['entries']} == {e['path'] for e in index['entries']}
        assert all(e['encrypted'] == bool(password) for e in converted['entries'] if not e['directory'])
        assert call('read', archive=str(target), revision=converted['source']['revision'], password=password,
            member='files/replace.txt', offset=0, length=100) == b'after\n'
    for revision, target, kind in (('stale', 'stale.zip', 'changed'), (index['source']['revision'], 'converted.zip', 'exists')):
        before = (root / target).read_bytes() if (root / target).exists() else None
        error = call('convert', good=False, archive=str(encrypted), revision=revision, password='fixture-password',
            destination=str(root / target), format='zip', options={'level':5})
        assert error['kind'] == kind, error
        assert ((root / target).read_bytes() if (root / target).exists() else None) == before
    index = call('list', archive=str(root / 'batch.zip'), password='fixture-password')
    result = call('convert', archive=str(root / 'batch.zip'), revision=index['source']['revision'], password='fixture-password',
        destination=str(root / 'batch-copy.7z'), format='7z', options={'level':5}, new_password='fixture-password')
    copied = call('list', archive=result['path'], password='fixture-password')
    assert next(e for e in copied['entries'] if e['path'] == 'batch')['mode'] == 0o555
    assert not list(root.glob('.zippity-write-*')), 'Save As left private staging behind'
    # Finder metadata members are archive content here, and bad settings fail before any copy.
    mac = root / 'mac.zip'
    with zipfile.ZipFile(mac, 'w') as out:
        for name in ('mac/file.txt', 'mac/.DS_Store', 'mac/._file.txt'):
            out.writestr(name, name)
    index = call('list', archive=str(mac))
    result = call('convert', archive=str(mac), revision=index['source']['revision'], destination=str(root / 'mac.7z'), format='7z', options={'level':5})
    assert {e['path'] for e in call('list', archive=result['path'])['entries']} == {e['path'] for e in index['entries']}
    error = call('convert', good=False, archive=str(mac), revision=index['source']['revision'], destination=str(root / 'bad.7z'),
        format='7z', options={'level':5, 'zip_crypto':True}, new_password='fixture-password')
    assert error['kind'] == 'request' and not (root / 'bad.7z').exists() and not list(root.glob('.zippity-write-*'))
    # A member stored without read access gains owner read; an empty archive has nothing to save.
    locked, empty = root / 'locked.zip', root / 'empty.zip'
    with zipfile.ZipFile(locked, 'w') as out:
        info = zipfile.ZipInfo('locked.txt')
        info.external_attr = 0o100000 << 16
        out.writestr(info, 'locked\n')
    zipfile.ZipFile(empty, 'w').close()
    index = call('list', archive=str(locked))
    result = call('convert', archive=str(locked), revision=index['source']['revision'], destination=str(root / 'unlocked.zip'), format='zip', options={'level':5})
    saved = call('list', archive=result['path'])
    assert saved['entries'][0]['mode'] == 0o400, saved['entries']
    assert call('read', archive=result['path'], revision=saved['source']['revision'], member='locked.txt', offset=0, length=100) == b'locked\n'
    index = call('list', archive=str(empty))
    error = call('convert', good=False, archive=str(empty), revision=index['source']['revision'], destination=str(root / 'empty.7z'), format='7z', options={'level':5})
    assert error['kind'] == 'empty' and not (root / 'empty.7z').exists()

    implicit_source = root / 'implicit-input'
    (implicit_source / 'outer' / 'inner').mkdir(parents=True)
    (implicit_source / 'outer' / 'inner' / 'one.txt').write_text('one\n')
    for format in formats:
        archive = root / ('implicit.' + format)
        if format == 'zip':
            with zipfile.ZipFile(archive, 'w') as out:
                parent = zipfile.ZipInfo('outer/')
                parent.external_attr = 0o40555 << 16
                out.writestr(parent, '')
                out.writestr('outer/inner/one.txt', 'one\n')
        else:
            # Both writers store only the named file, so its parent folders stay implicit.
            command = ['rar','a','-idq','--'] if format == 'rar' else ['7zz','a','-bso0','-bsp0','--']
            subprocess.run([*command, str(archive), 'outer/inner/one.txt'], cwd=implicit_source, check=True, capture_output=True)
        index = call('list', archive=str(archive))
        changes = [{'kind':'delete','path':'outer/inner/one.txt'},
            {'kind':'rename','from':'outer/inner','to':'renamed/branch'},
            {'kind':'delete','path':'renamed/branch'}]
        planned = call('plan', archive=str(archive), revision=index['source']['revision'], changes=changes)
        saved = call('apply', archive=str(archive), revision=index['source']['revision'], changes=changes)
        assert {e['path'] for e in saved['entries']} == {e['path'] for e in planned} == {'outer','renamed'}
        if format == 'zip':
            parent = next(e for e in saved['entries'] if e['path'] == 'outer')
            original = next(e for e in index['entries'] if e['path'] == 'outer')
            assert parent['mode'] == original['mode'] and parent['modified'] == original['modified']
        assert not list(root.glob('.zippity-write-*'))

    archive = root / 'normalized-directories.zip'
    with zipfile.ZipFile(archive, 'w') as out:
        parent = zipfile.ZipInfo('./outer/')
        parent.external_attr = 0o40550 << 16
        out.writestr(parent, '')
        out.writestr('./outer/old.txt', 'old\n')
        out.writestr('implicit/child.txt', 'child\n')
    index = call('list', archive=str(archive))
    changes = call('stage', sources=[str(replacement)], prefix='outer')
    changes.append({'kind':'delete','path':'implicit/child.txt'})
    planned = call('plan', archive=str(archive), revision=index['source']['revision'], changes=changes)
    saved = call('apply', archive=str(archive), revision=index['source']['revision'], changes=changes)
    assert {e['path'] for e in saved['entries']} == {e['path'] for e in planned}
    parent = next(e for e in saved['entries'] if e['path'] == 'outer')
    original = next(e for e in index['entries'] if e['path'] == 'outer')
    assert (parent['mode'], parent['modified']) == (original['mode'], original['modified'])

    if 'rar' in formats:
        names = root / 'literal-names'
        names.mkdir()
        (names / '-idq').write_text('literal switch\n')
        (names / 'keep.txt').write_text('keep\n')
        archive = root / 'literal.rar'
        call('create', sources=[str(names / '-idq'), str(names / 'keep.txt')], destination=str(archive), format='rar', options={'level':5})
        index = call('list', archive=str(archive))
        saved = call('apply', archive=str(archive), revision=index['source']['revision'], changes=[{'kind':'delete','path':'-idq'}])
        assert {e['path'] for e in saved['entries']} == {'keep.txt'}

    failure = root / 'failing-backend'
    failure.mkdir()
    executable = shutil.which('7zz')
    wrapper = failure / '7zz'
    wrapper.write_text('#!/usr/bin/env python3\nimport os, sys\n'
        'if sys.argv[1] == "a": sys.exit(2)\n'
        f'os.execv({executable!r}, [{executable!r}, *sys.argv[1:]])\n')
    wrapper.chmod(0o700)
    archive = root / 'failed-batch.zip'
    call('create', sources=[str(replacement)], destination=str(archive), format='zip', options={'level':5})
    index = call('list', archive=str(archive))
    before = archive.read_bytes()
    batch.chmod(0o555)
    try:
        changes = call('stage', sources=[str(batch)])
        call('apply', good=False, archive=str(archive), revision=index['source']['revision'], changes=changes,
            env={**os.environ, 'PATH':str(failure) + os.pathsep + os.environ['PATH']})
        assert archive.read_bytes() == before
        assert not list(root.glob('.zippity-write-*'))
    finally:
        batch.chmod(0o755)
    changing = root / 'changing.txt'
    changing.write_text('original input\n')
    changes = call('stage', sources=[str(changing)])
    wrapper.write_text('#!/usr/bin/env python3\nimport os, subprocess, sys\n'
        f'if sys.argv[1] != "a": os.execv({executable!r}, [{executable!r}, *sys.argv[1:]])\n'
        f'result = subprocess.run([{executable!r}, *sys.argv[1:]])\n'
        f'with open({str(changing)!r}, "w") as out: out.write("changed input\\n")\n'
        'sys.exit(result.returncode)\n')
    error = call('apply', good=False, archive=str(archive), revision=index['source']['revision'], changes=changes,
        env={**os.environ, 'PATH':str(failure) + os.pathsep + os.environ['PATH']})
    assert error['kind'] == 'changed', error
    assert archive.read_bytes() == before
    assert not list(root.glob('.zippity-write-*'))

    replacement.chmod(0o640)
    os.utime(replacement, (1_600_000_000, 1_600_000_000))
    change = call('stage', sources=[str(replacement)])[0]
    archive = root / 'staged-copy.zip'
    call('create', sources=[str(replacement)], destination=str(archive), format='zip', options={'level':5})
    copied = root / 'staged-copy.txt'
    request = {'archive':str(archive), 'revision':call('list', archive=str(archive))['source']['revision'],
        'change':change, 'destination':str(copied)}
    assert call('copy_input', **request) == {'done':6}
    assert copied.read_text() == 'after\n' and copied.stat().st_mode & 0o7777 == 0o600
    call('copy_input', good=False, **request)
    copied.chmod(0o4751)
    call('copy_input', **request, overwrite=True)
    assert copied.stat().st_mode & 0o7777 == 0o751
    call('copy_input', **request, overwrite=True, preserve_metadata=True)
    assert copied.stat().st_mode & 0o7777 == 0o640 and abs(copied.stat().st_mtime - 1_600_000_000) < 1
    print('Creation, encryption, split sets, batched saves, retained folders, and conflict protection passed')
