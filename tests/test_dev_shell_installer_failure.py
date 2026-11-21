"""A failed hook installer must fail the `nix develop` entrypoint.

Spec: codetracer-pm spec/Testing/EVM-Portable-Standard-Hooks.md, "A failed
installer must fail the declared entrypoint."

No mocks: this enters the real dev shell of a copy of this checkout (the
shellHook only installs hooks when flake.nix at the repo root is byte-identical
to the evaluated one, so the copy is made from the working tree). The
installer is made to fail genuinely, by planting a modified managed hook
template, which it refuses to overwrite.
"""

from __future__ import annotations
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class DevShellInstallerFailure(unittest.TestCase):
    def setUp(self) -> None:
        self.nix = shutil.which("nix")
        self.git = shutil.which("git")
        if self.nix is None or self.git is None:
            self.fail("missing declared nix or git")
        tmp = tempfile.TemporaryDirectory(prefix="evm-devshell-failure-")
        self.addCleanup(tmp.cleanup)
        self.work = Path(tmp.name) / "checkout"
        self.work.mkdir()
        listing = subprocess.check_output(
            [self.git, "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT
        ).split(b"\0")
        for raw in filter(None, listing):
            rel = Path(raw.decode())
            source = ROOT / rel
            if not source.exists():
                continue  # deleted in the working tree
            target = self.work / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            if source.is_symlink():
                os.symlink(os.readlink(source), target)
            else:
                shutil.copy2(source, target)
        self.run_git("init", "-q")
        self.run_git("add", "-A")
        hooks = self.work / ".git/hooks"
        hooks.mkdir(exist_ok=True)
        (hooks / "post-checkout.repro-managed").write_text("# modified managed template\n")

    def run_git(self, *args: str) -> None:
        subprocess.run(
            [self.git, "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
            cwd=self.work,
            check=True,
        )

    def test_failed_installer_fails_command_without_running_it(self) -> None:
        marker = self.work / "command-ran"
        result = subprocess.run(
            [self.nix, "develop", "--command", "sh", "-c", f"touch {marker}"],
            cwd=self.work,
            text=True,
            capture_output=True,
        )
        self.assertIn("unknown or modified managed hook template", result.stderr)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists(), "command ran after the installer failed")


if __name__ == "__main__":
    unittest.main()
