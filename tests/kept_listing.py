#!/usr/bin/env python3
"""A listing kept in a private member cache stands in for listing the archive again until
the archive changes. Listings that need a password, and folders zippity did not make, are
left alone."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/zippity'


def call(op, env=None, **values):
    result = subprocess.run([BINARY], input=json.dumps({'op': op, **values}).encode(), capture_output=True, timeout=90, env=env)
    assert result.returncode == 0, (op, result.stderr.decode())
    return json.loads(result.stdout.splitlines()[-1])


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    observer, log, out = root / 'observer', root / 'calls', root / 'out'
    observer.mkdir()
    out.mkdir()
    real = shutil.which('7zz')
    wrapper = observer / '7zz'
    wrapper.write_text('#!/usr/bin/env python3\nimport os, sys\n'
        f'open({str(log)!r}, "a").write(sys.argv[1] + "\\n")\n'
        f'os.execv({real!r}, [{real!r}, *sys.argv[1:]])\n')
    wrapper.chmod(0o700)
    env = {**os.environ, 'PATH': f'{observer}{os.pathsep}{os.environ["PATH"]}'}
    archive = root / 'kept.zip'
    with zipfile.ZipFile(archive, 'w') as fixture:
        fixture.writestr('a.txt', 'first\n')
        fixture.writestr('b.txt', 'second\n')
    cache = call('cache')['path']
    copies = iter(range(1000))

    def copy(member, cache, password=None):
        """The 7zz commands one copy ran, and what it copied."""
        log.write_text('')
        revision = call('list', archive=str(archive))['source']['revision']
        target = out / f'{next(copies)}.txt'
        call('copy', env=env, archive=str(archive), member=member, revision=revision,
            destination=str(target), cache=cache, password=password)
        return log.read_text().split(), target.read_text()

    assert copy('a.txt', cache) == (['l', 'x'], 'first\n')
    kept = Path(cache) / '.listing.json'
    assert kept.stat().st_mode & 0o777 == 0o600
    assert copy('b.txt', cache) == (['x'], 'second\n')
    # Without a cache, with a folder zippity did not make, or with a password, it lists.
    assert copy('a.txt', None)[0] == ['l', 'x']
    assert copy('a.txt', str(root))[0] == ['l', 'x'] and not (root / '.listing.json').exists()
    assert copy('a.txt', cache, password='fixture-password')[0] == ['l', 'x']
    # A changed archive is listed again.
    with zipfile.ZipFile(archive, 'a') as fixture:
        fixture.writestr('c.txt', 'third\n')
    assert copy('c.txt', cache) == (['l', 'x'], 'third\n')
    assert copy('a.txt', cache) == (['x'], 'first\n')
    call('clean_cache', directory=cache)
    assert not Path(cache).exists()
    print('Kept listings stand in for listing an unchanged archive again')
