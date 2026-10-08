"""Genuine complete hook factory from the declared immutable native package.

Source-only proposal. Private qualification must bind the complete constructor,
all runtime roles, executable chain, legacy/refusal controls and natural drains.
"""
from __future__ import annotations
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import errno
import sys
import tempfile
import time

PIN = '76659f5730ecf698b1963c656494d2cb66eb256d'
LOCKED_SOURCE = {'type': 'github', 'owner': 'metacraft-labs', 'repo': 'reprobuild', 'rev': PIN, 'narHash': 'sha256-5/zXbXM6CnrnxTZpXUcvB4bfk9UBehuSFZ3M0O/B1zQ=', 'lastModified': 1790923307}
SOURCE_SHA = 'b2f6c882f13c3d598fe47fcac1e60b7ef8dc9c23ae72ac2876faa43568532c86'
HOOKS = ('pre-commit', 'pre-push', 'post-commit', 'post-checkout', 'post-merge')


def principal(path: Path) -> dict:
    path = path.absolute()
    resolved = path.resolve(strict=True)
    meta = resolved.stat()
    if not stat.S_ISREG(meta.st_mode) or meta.st_uid != 0 or meta.st_mode & 0o222:
        raise RuntimeError('factory principal is not an immutable root-owned file: ' + str(path))
    return {'path': str(path), 'resolved': str(resolved),
            'mode': stat.S_IMODE(meta.st_mode), 'sha256': hashlib.sha256(resolved.read_bytes()).hexdigest(),
            'link': os.readlink(path) if path.is_symlink() else None}


def os_observer_principal(path: Path) -> dict:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or not info.st_mode & 0o111 or info.st_mode & 0o022:
        raise RuntimeError('OS observer is not a root-owned executable with protected group/other modes')
    return {'path': str(path), 'dev': info.st_dev, 'ino': info.st_ino,
            'mode': stat.S_IMODE(info.st_mode), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}


def session_census(sid: int) -> dict:
    # Frontend scope only: same UID, live non-zombie session members.
    remaining, unknown = [], []
    try:
        if sys.platform.startswith('linux'):
            for entry in Path('/proc').iterdir():
                if not entry.name.isdigit():
                    continue
                try:
                    if entry.stat().st_uid != os.getuid():
                        continue
                    raw = (entry / 'stat').read_text()
                    fields = raw[raw.rindex(')') + 2:].split()
                    if int(fields[3]) == sid and fields[0] != 'Z':
                        remaining.append(int(entry.name))
                except OSError as error:
                    if error.errno not in (errno.ESRCH, errno.ENOENT):
                        unknown.append({'pid': entry.name, 'error': repr(error)})
                except (ValueError, IndexError) as error:
                    unknown.append({'pid': entry.name, 'error': repr(error)})
        elif sys.platform == 'darwin':
            result = subprocess.run(['/bin/ps', '-axo', 'pid=,uid=,stat='], capture_output=True, text=True)
            if result.returncode:
                unknown.append({'observer': 'ps', 'exit': result.returncode})
            else:
                for line in result.stdout.splitlines():
                    try:
                        pid, uid, state = line.split()
                        if int(uid) == os.getuid() and not state.startswith('Z') and os.getsid(int(pid)) == sid:
                            remaining.append(int(pid))
                    except OSError as error:
                        if error.errno not in (errno.ESRCH, errno.ENOENT):
                            unknown.append({'observer': 'ps', 'line': line, 'error': repr(error)})
                    except ValueError as error:
                        unknown.append({'observer': 'ps', 'line': line, 'error': repr(error)})
        else:
            unknown.append({'unsupportedPlatform': sys.platform})
    except OSError as error:
        unknown.append({'observer': 'session census', 'error': repr(error)})
    return {'remaining': sorted(remaining), 'unknown': unknown}


def terminal(argv: list[str], cwd: Path, env: dict[str, str]) -> str:
    # Capture to ordinary exclusive files, so the child cannot block on pipes.
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdout=out, stderr=err, start_new_session=True)
        identity_error = None
        try:
            if os.getsid(child.pid) != child.pid:
                raise RuntimeError('new direct child does not own the expected session')
        except (OSError, RuntimeError) as error:
            identity_error = error
        interrupted = None
        try:
            while True:
                try:
                    code = child.wait()
                    break
                except (KeyboardInterrupt, InterruptedError) as error:
                    interrupted = interrupted or error
        finally:
            while child.poll() is None:
                try:
                    child.wait()
                except (KeyboardInterrupt, InterruptedError) as error:
                    interrupted = interrupted or error
        out.seek(0)
        err.seek(0)
        stdout, stderr = out.read(), err.read()
        census = session_census(child.pid)
        while census['remaining'] and not census['unknown']:
            try:
                time.sleep(0.1)
            except (KeyboardInterrupt, InterruptedError) as error:
                interrupted = interrupted or error
            census = session_census(child.pid)
        if census != {'remaining': [], 'unknown': []}:
            raise RuntimeError('real factory child session did not naturally drain: ' + repr(census))
        if identity_error is not None:
            raise RuntimeError('child session identity unavailable before kernel reap: ' + repr(identity_error))
        if interrupted is not None:
            raise interrupted
        if code != 0:
            raise RuntimeError('real factory command failed: ' + repr(argv) + '\n' + stderr.decode(errors='replace'))
        return stdout.decode().strip()


class SelectedFactory:
    def __init__(self, repro: Path, git: Path, owner: Path):
        self.owner = owner
        self.env = os.environ.copy()
        package = Path(self.env['REPROBUILD_NATIVE_PACKAGE']).absolute()
        source = Path(self.env['REPROBUILD_NATIVE_SOURCE']).absolute()
        if any(not str(p).startswith('/nix/store/') for p in (package, source)):
            raise RuntimeError('declared factory package/source/derivation must be immutable Nix store roles')
        if repro.absolute() != package / 'bin/repro':
            raise RuntimeError('selected executable differs from the declared complete package')
        lock = json.loads((owner / 'flake.lock').read_text())
        node = lock['nodes']['root']['inputs']['reprobuild']
        if not isinstance(node, str) or lock['nodes'][node]['locked'] != LOCKED_SOURCE:
            raise RuntimeError('owning lock does not select the reviewed exact Repro source')
        module = source / 'libs/repro_cli_support/src/repro_cli_support.nim'
        if hashlib.sha256(module.read_bytes()).hexdigest() != SOURCE_SHA:
            raise RuntimeError('declared actual factory source bytes differ')
        nix = Path(shutil.which('nix') or '')
        nix_store = Path(shutil.which('nix-store') or '')
        self.principals = {str(p): principal(p) for p in (repro, git, nix, nix_store, module)}
        if sys.platform == 'darwin':
            self.os_observer = Path('/bin/ps')
            self.os_observer_identity = os_observer_principal(self.os_observer)
        wrapper = repro.read_text()
        first = wrapper.splitlines()[0]
        shebang = re.fullmatch(r'#! ?(/nix/store/[^ /]+/bin/bash)( -e)?', first)
        if shebang is None:
            raise RuntimeError('unsupported native package interpreter binding')
        interpreter = Path(shebang.group(1))
        self.interpreter_argument = shebang.group(2)
        targets = re.findall(r'^exec -a "\$0" "([^\"]+)"', wrapper, re.MULTILINE)
        if targets != [str(package / 'bin/.repro-wrapped')]:
            raise RuntimeError('unsupported or nonpackage wrapped executable binding')
        for p in (interpreter, Path(targets[0])):
            self.principals[str(p)] = principal(p)
        # The runtime controller additionally binds ELF/Mach-O loader/dependency
        # closure before qualification; wrapper resolution alone is insufficient.
        if terminal([str(nix), 'hash', 'path', '--sri', str(source)], owner, self.env) != LOCKED_SOURCE['narHash']:
            raise RuntimeError('complete selected source NAR differs from the reviewed immutable input')
        # Query the actual selected output's supported metadata instead of
        # injecting .drvPath string context into the consuming dev shell.
        drv = Path(terminal([str(nix_store), '--query', '--deriver', str(package)], owner, self.env))
        if not drv.is_absolute() or drv.parent != Path('/nix/store') or drv.suffix != '.drv':
            raise RuntimeError('selected complete package has no actual immutable constructor authority')
        self.principals[str(drv)] = principal(drv)
        decoded = json.loads(terminal([str(nix), 'derivation', 'show', str(drv)], owner, self.env))
        if len(decoded) != 1:
            raise RuntimeError('ambiguous package constructor')
        body = next(iter(decoded.values()))
        constructed_source = Path(body['env']['src'])
        if not str(constructed_source).startswith('/nix/store/'):
            raise RuntimeError('constructor source is outside immutable store authority')
        # Nix copies the input path under a new store name; full NAR identity,
        # not lexical path equality, binds every byte, mode and link target.
        if terminal([str(nix), 'hash', 'path', '--sri', str(constructed_source)], owner, self.env) != LOCKED_SOURCE['narHash']:
            raise RuntimeError('complete constructor source differs from locked input')
        constructed_module = constructed_source / 'libs/repro_cli_support/src/repro_cli_support.nim'
        self.principals[str(constructed_module)] = principal(constructed_module)
        if self.principals[str(constructed_module)]['sha256'] != SOURCE_SHA:
            raise RuntimeError('actual constructor factory module differs')
        output = body['outputs']['out']['path']
        # This selected Nix schema reports a store basename; accept precisely
        # that basename or its canonical absolute spelling, no arbitrary paths.
        if output not in (package.name, str(package)):
            raise RuntimeError('constructor output differs from selected package')
        self.constructed_source = constructed_source
        self.constructor_sha = hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest()
        self.bindings = {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                            for p in (owner / 'flake.nix', owner / 'flake.lock', Path(__file__))}
        self.guard()
        root = Path(tempfile.mkdtemp(prefix='evm-selected-real-hook-factory-'))
        root_identity = (root.stat().st_dev, root.stat().st_ino)
        cleanup_safe = False
        try:
            # Only this new, exclusive Git metadata context ignores inherited
            # command configuration; owning repository configuration is untouched.
            env = self.env.copy()
            for key in list(env):
                if key.startswith('GIT_'):
                    del env[key]
            env['GIT_CONFIG_NOSYSTEM'] = '1'
            env['GIT_CONFIG_GLOBAL'] = os.devnull
            terminal([str(git), 'init', '--quiet', str(root)], root, env)
            samples = {p.name: (p.read_bytes(), stat.S_IMODE(p.stat().st_mode))
                        for p in (root / '.git/hooks').iterdir() if p.is_file() and not p.is_symlink()}
            if any(not n.endswith('.sample') for n in samples):
                raise RuntimeError('exclusive factory starts with unexpected active hooks')
            env['REPROBUILD_REPRO'] = str(repro)
            terminal([str(repro), 'hooks', 'ensure', '--vcs', str(root)], root, env)
            directory = root / '.git/hooks'
            expected = {n for hook in HOOKS for n in (hook, hook + '.repro-managed')}
            if {p.name for p in directory.iterdir()} != set(samples) | expected:
                raise RuntimeError('actual selected factory does not produce the complete ten roles')
            self.bodies = {}
            for name in expected:
                p = directory / name
                if p.is_symlink() or not p.is_file() or stat.S_IMODE(p.stat().st_mode) != 0o755:
                    raise RuntimeError('invalid actual factory role: ' + name)
                self.bodies[name] = p.read_bytes()
            for name, (data, mode) in samples.items():
                p = directory / name
                if p.is_symlink() or p.read_bytes() != data or stat.S_IMODE(p.stat().st_mode) != mode:
                    raise RuntimeError('factory changed genuine Git sample: ' + name)
            self.hashes = {n: hashlib.sha256(b).hexdigest() for n, b in self.bodies.items()}
            contracts = re.findall(rb'reprobuild\.managed-hook\.v1\.pre-commit\.[0-9a-f]{16}', self.bodies['pre-commit.repro-managed'])
            if len(set(contracts)) != 1:
                raise RuntimeError('ambiguous actual selected factory contract')
            self.contract = contracts[0].decode()
            if terminal([str(repro), 'hooks', 'protocol', '--require=2', '--hook-contract=' + self.contract], root, env) != '2':
                raise RuntimeError('selected factory protocol refused its actual complete body')
            self.guard()
            cleanup_safe = True
        finally:
            if cleanup_safe:
                info = root.lstat()
                if not stat.S_ISDIR(info.st_mode) or (info.st_dev, info.st_ino) != root_identity:
                    self.retained_factory = str(root)
                    raise RuntimeError('exclusive factory directory identity changed before cleanup')
                shutil.rmtree(root)
            else:
                self.retained_factory = str(root)
        self.guard()

    def guard(self):
        if hasattr(self, 'os_observer') and os_observer_principal(self.os_observer) != self.os_observer_identity:
            raise RuntimeError('Darwin OS observer identity changed')
        if any(principal(Path(p)) != value for p, value in self.principals.items()):
            raise RuntimeError('factory executable/source principal changed')
        if any(hashlib.sha256(Path(p).read_bytes()).hexdigest() != value for p, value in self.bindings.items()):
            raise RuntimeError('owning factory declarations changed')
