#!/usr/bin/env python3
"""Create the locked decoder peer with its own declared Git templates."""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import tomllib

nix, consumer = sys.argv[1:]
consumer = Path(consumer).resolve(strict=True)
lock = tomllib.loads((consumer / "repro.lock").read_text())
peers = [d for d in lock["lock"]["deps"] if d["name"] == "codetracer-trace-format-nim"]
assert len(peers) == 1, "Missing unique locked decoder peer"
peer = peers[0]
assert peer["url"] == "https://github.com/metacraft-labs/codetracer-trace-format-nim"
assert peer["path"] == "../codetracer-trace-format-nim"
revision = peer["revision"]
assert len(revision) == 40 and all(c in "0123456789abcdef" for c in revision)
destination = consumer.parent / "codetracer-trace-format-nim"
assert not os.path.lexists(destination), "Occupied decoder peer; existing hooks must remain untouched"

def run(argv, **kwargs):
    return subprocess.check_output(argv, **kwargs).decode().strip()

system = run([nix, "eval", "--impure", "--raw", "--expr", "builtins.currentSystem"])
assert system in ("x86_64-linux", "aarch64-linux", "x86_64-darwin", "aarch64-darwin")
selector = f"github:metacraft-labs/codetracer-trace-format-nim/{revision}#devShells.{system}.default.nativeBuildInputs"
projection = 'inputs: builtins.map (p: { name=p.name; path=p.outPath; drv=p.drvPath; }) (builtins.filter (p: (p.pname or "") == "git") inputs)'
roles = json.loads(run([nix, "eval", "--option", "allow-import-from-derivation", "false", "--json", selector, "--apply", projection]))
assert len(roles) == 1, "Decoder peer must declare exactly one Git role"
role = roles[0]
output = Path(role["path"])
drv = role["drv"]
assert str(output).startswith("/nix/store/") and drv.startswith("/nix/store/") and drv.endswith(".drv")
actual = run([nix, "build", "--no-link", "--print-out-paths", drv + "^out"])
assert actual == str(output), "Declared peer Git output mismatch"
git = output / "bin/git"
templates = output / "share/git-core/templates"
assert git.is_file() and templates.is_dir()

def sample_manifest():
    result = {}
    for path in sorted((templates / "hooks").glob("*.sample")):
        st = path.lstat()
        assert stat.S_ISREG(st.st_mode), "Unknown selected template entry"
        result[path.name] = (stat.S_IMODE(st.st_mode), hashlib.sha256(path.read_bytes()).hexdigest())
    assert "applypatch-msg.sample" in result
    return result

store_before = sample_manifest()
env = dict(os.environ)
env.pop("GIT_TEMPLATE_DIR", None)
env["GIT_CONFIG_NOSYSTEM"] = "1"
env["GIT_CONFIG_GLOBAL"] = os.devnull
# The store normalizes executable modes. The clone constructor's real init is
# the authority for installed sample modes, not the immutable template modes.
with tempfile.TemporaryDirectory(prefix=".decoder-git-template-", dir=consumer.parent) as witness:
    subprocess.run([str(git), "init", "--template=" + str(templates), witness], env=env, check=True)
    expected = {}
    for path in sorted((Path(witness) / ".git/hooks").glob("*.sample")):
        st = path.lstat()
        assert stat.S_ISREG(st.st_mode), "Unknown initialized sample entry"
        expected[path.name] = (stat.S_IMODE(st.st_mode), hashlib.sha256(path.read_bytes()).hexdigest())
    assert set(expected) == set(store_before)
    assert all(expected[name][1] == store_before[name][1] for name in expected)
subprocess.run([str(git), "init", "--template=" + str(templates), str(destination)], env=env, check=True)
subprocess.run([str(git), "-C", str(destination), "remote", "add", "origin", peer["url"]], env=env, check=True)
subprocess.run([str(git), "-C", str(destination), "fetch", "--no-auto-maintenance", "--depth=1", "origin", revision], env=env, check=True)
subprocess.run([str(git), "-C", str(destination), "checkout", "--detach", revision], env=env, check=True)
assert run([str(git), "-C", str(destination), "rev-parse", "HEAD"], env=env) == revision
assert not run([str(git), "-C", str(destination), "status", "--porcelain=v1"], env=env)
for name, identity in expected.items():
    path = destination / ".git/hooks" / name
    st = path.lstat()
    assert stat.S_ISREG(st.st_mode)
    assert (stat.S_IMODE(st.st_mode), hashlib.sha256(path.read_bytes()).hexdigest()) == identity
assert sample_manifest() == store_before, "Selected templates changed"
print(json.dumps({"peerRevision": revision, "gitOutput": str(output), "gitVersion": run([str(git), "--version"]), "templateSamples": expected}, sort_keys=True))
