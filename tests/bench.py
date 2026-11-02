#!/usr/bin/env python3
"""Times the helper against raw 7zz doing the same work on large archives. A budget is a
multiple of the 7zz time, plus a 7zz listing where the helper lists the archive and raw
7zz does not, plus a fixed allowance for the extra process. Timings vary with machine
load, so only ZIPPITY_BENCH_BUDGET=1 turns a row over budget into a failure."""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile

REPO = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('ZIPPITY_BENCH_BINARY', REPO / 'target/release/zippity'))
FOLDERS, PER_FOLDER = 200, 100


def helper(request):
    result = subprocess.run([BINARY], input=json.dumps(request).encode(), capture_output=True)
    assert result.returncode == 0, (request['op'], result.stderr.decode()[:500])
    return json.loads(result.stdout.splitlines()[-1])


def zz(*args, output=subprocess.DEVNULL):
    subprocess.run(['7zz', *args], check=True, stdout=output, stderr=subprocess.DEVNULL)


def best(action, reset=None, runs=3):
    """The fastest of `runs` timings; `reset` runs untimed before each."""
    times = []
    for _ in range(runs):
        if reset:
            reset()
        start = time.perf_counter()
        action()
        times.append(time.perf_counter() - start)
    return min(times)


with tempfile.TemporaryDirectory(prefix='zippity-bench-') as temporary:
    root = Path(temporary)
    tree = root / 'tree'
    for folder in range(FOLDERS):
        (tree / f'folder{folder:03}').mkdir(parents=True)
        for number in range(PER_FOLDER):
            (tree / f'folder{folder:03}' / f'file{number:03}.txt').write_text(f'{folder} {number} ' * 30)
    (root / 'big.bin').write_bytes(os.urandom(128 * 1024 * 1024))
    many = {'zip': root / 'many.zip', '7z': root / 'many.7z'}
    zz('a', '-tzip', '-mx=1', str(many['zip']), str(tree))
    zz('a', '-t7z', '-mx=1', str(many['7z']), str(tree))
    big, outer = root / 'big.zip', root / 'outer.zip'
    zz('a', '-tzip', '-mx=0', str(big), str(root / 'big.bin'))
    zz('a', '-tzip', '-mx=0', str(outer), str(many['zip']))
    out = root / 'out'

    def clean():
        shutil.rmtree(out, ignore_errors=True)
        out.mkdir()

    def folders(index):
        """Creates the archive's folders under `out`, as the plugin does before copying."""
        clean()
        for entry in index['entries']:
            if entry['directory']:
                (out / entry['path']).mkdir(parents=True, exist_ok=True)

    def copy_many(archive, index, members):
        files = [{'member': member, 'destination': str(out / member)} for member in members]
        report = helper({'op': 'copy_many', 'archive': str(archive), 'revision': index['source']['revision'], 'files': files})
        # A copy that left members out would look faster than it is.
        sizes = {entry['path']: entry['size'] for entry in index['entries']}
        assert report['damaged'] == report['lost'] == [] and not report.get('skipped'), report
        assert report['done'] == sum(sizes[member] for member in members), report

    rows = []

    def row(name, mine, raw, listing=0.0, factor=2.0, allowance=0.05):
        rows.append((name, mine, raw, factor * (raw + listing) + allowance))

    for kind, archive in many.items():
        index = helper({'op': 'list', 'archive': str(archive)})
        files = [entry['path'] for entry in index['entries'] if not entry['directory']]
        assert len(files) == FOLDERS * PER_FOLDER, len(files)
        last = files[-1]
        selected = [path for path in files if path.startswith('tree/folder100/')]
        listing = best(lambda: zz('l', '-slt', '-ba', str(archive)))
        row(f'list {kind}, {len(files)} files', best(lambda: helper({'op': 'list', 'archive': str(archive)})),
            listing, factor=2.5)
        row(f'preview {kind}, last member',
            best(lambda: helper({'op': 'copy', 'archive': str(archive), 'member': last,
                'revision': index['source']['revision'], 'destination': str(out / 'preview')}), clean),
            best(lambda: zz('e', '-so', str(archive), f'-i!{last}', output=(out / 'preview').open('wb')), clean),
            listing)
        row(f'extract {kind}, one folder of {len(selected)}',
            best(lambda: copy_many(archive, index, selected), lambda: folders(index)),
            best(lambda: zz('x', f'-o{out}', str(archive), '-i!tree/folder100'), clean), listing)
        row(f'extract {kind}, all {len(files)}',
            best(lambda: copy_many(archive, index, files), lambda: folders(index)),
            best(lambda: zz('x', f'-o{out}', str(archive)), clean), factor=4, allowance=0.2)
        row(f'check {kind}',
            best(lambda: helper({'op': 'check', 'archive': str(archive), 'revision': index['source']['revision']})),
            best(lambda: zz('t', str(archive))), factor=2.5)
    index = helper({'op': 'list', 'archive': str(big)})
    row('extract 128 MiB member', best(lambda: copy_many(big, index, ['big.bin']), clean),
        best(lambda: zz('x', f'-o{out}', str(big)), clean), factor=4, allowance=0.1)
    index = helper({'op': 'list', 'archive': str(outer)})
    inner = out / 'many.zip'

    def nested():
        helper({'op': 'copy', 'archive': str(outer), 'member': 'many.zip',
            'revision': index['source']['revision'], 'destination': str(inner)})
        helper({'op': 'list', 'archive': str(inner)})

    def nested_raw():
        zz('e', '-so', str(outer), '-i!many.zip', output=inner.open('wb'))
        zz('l', '-slt', '-ba', str(inner))

    row('open nested zip of 20000 files', best(nested, clean), best(nested_raw, clean), factor=3, allowance=0.1)
    # The largest listing zippity takes, from tiny stored members.
    huge = root / 'huge.zip'
    with zipfile.ZipFile(huge, 'w') as archive:
        for number in range(199000):
            archive.writestr(f'folder{number // 1000:03}/file{number % 1000:03}.txt', 'x')
    index = helper({'op': 'list', 'archive': str(huge)})
    listing = best(lambda: zz('l', '-slt', '-ba', str(huge)))
    row('list zip, 199000 files', best(lambda: helper({'op': 'list', 'archive': str(huge)})), listing,
        factor=2.5, allowance=0.1)
    raw = best(lambda: zz('e', '-so', str(huge), '-i!folder198/file999.txt', output=(out / 'preview').open('wb')), clean)
    row('preview zip of 199000, one member',
        best(lambda: helper({'op': 'copy', 'archive': str(huge), 'member': 'folder198/file999.txt',
            'revision': index['source']['revision'], 'destination': str(out / 'preview')}), clean),
        raw, listing, allowance=0.1)
    # Previews keep the listing in their member cache, so later ones skip listing.
    cache = helper({'op': 'cache'})['path']
    row('preview again, listing kept',
        best(lambda: helper({'op': 'copy', 'archive': str(huge), 'member': 'folder198/file999.txt',
            'revision': index['source']['revision'], 'destination': str(out / 'preview'), 'cache': cache}), clean),
        raw, allowance=0.3)
    helper({'op': 'clean_cache', 'directory': cache})

    def in_yazi(archive, key, finished):
        """Seconds from pressing `key` on `archive` in Yazi until `finished(screen)` holds."""
        config = root / 'yazi'
        if not config.exists():
            shutil.copytree(REPO / 'zippity.yazi', config / 'plugins' / 'zippity.yazi')
            for name in ('vfs.toml', 'keymap.toml', 'yazi.toml'):
                shutil.copyfile(REPO / 'examples' / name, config / name)
        env = {**os.environ, 'YAZI_CONFIG_HOME': str(config), 'XDG_CACHE_HOME': str(root / 'yazi-cache'),
            'XDG_STATE_HOME': str(root / 'yazi-state'), 'PATH': f'{BINARY.parent}{os.pathsep}{os.environ["PATH"]}'}

        def tmux(*args):
            return subprocess.run(['tmux', '-L', 'zippity-bench', *args], env=env, capture_output=True, text=True)

        def screen():
            return tmux('capture-pane', '-pt', 'bench').stdout

        def until(predicate, what):
            deadline = time.monotonic() + 120
            while not predicate():
                assert time.monotonic() < deadline, what + '\n' + screen()
                time.sleep(0.005)

        try:
            tmux('new-session', '-d', '-s', 'bench', '-x', '160', '-y', '40', shlex.join(['yazi', str(archive)]))
            until(lambda: archive.name in screen(), 'Yazi did not start')
            time.sleep(0.5)
            start = time.perf_counter()
            tmux('send-keys', '-t', 'bench', key)
            until(lambda: finished(screen()), f'{key} on {archive.name} did not finish')
            return time.perf_counter() - start
        finally:
            tmux('kill-server')

    # These cover the plugin too: decoding the index, building folders, and planning copies.
    opened = min(in_yazi(huge, 'l', lambda screen: 'zippity://' in screen and 'folder000' in screen) for _ in range(3))
    row('open zip of 199000 in Yazi', opened, listing, allowance=0.3)
    beside = root / 'beside'
    beside.mkdir()
    shutil.copyfile(many['zip'], beside / 'many.zip')
    extracted = []
    for _ in range(3):
        shutil.rmtree(beside / 'tree', ignore_errors=True)
        extracted.append(in_yazi(beside / 'many.zip', 'M-x', lambda screen: 'Selected members extracted' in screen))
        assert sum(1 for path in (beside / 'tree').rglob('*') if path.is_file()) == FOLDERS * PER_FOLDER
    raw = best(lambda: zz('x', f'-o{out}', str(many['zip'])), clean)
    row('Alt-x on zip of 20000 files in Yazi', min(extracted), raw, factor=4, allowance=0.5)
    print(f'{sys.platform}: helper and 7zz, best of 3, with the helper budget')
    for name, mine, raw, budget in rows:
        print(f'{name:40} {mine:7.3f} s {raw:7.3f} s {mine / raw:6.2f}x  budget {budget:7.3f} s')
    over = [name for name, mine, _, budget in rows if mine > budget]
    if os.environ.get('ZIPPITY_BENCH_BUDGET') == '1':
        assert not over, f'over budget: {over}'
        print('The helper stays within its budget against 7zz')
    elif over:
        print(f'Over budget, not enforced: {over}')
