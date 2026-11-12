"""Exercise genuine Git registration and filesystem ownership, without mocks.

Each case owns a temporary primary/linked worktree populated with the actual
owning declaration bytes. It never changes the production checkout or a user
hook. Native installer/template/default-hook controls remain separate tests.

The workspace cases start from the ten managed bodies a previously selected
package actually wrote, kept byte for byte under
tests/fixtures/legacy-managed-hooks-<revision>/: reprobuild 76659f and
reprobuild v0.2.20 (a650b20). They are captured output, not a mock: neither
package is part of the declared shell, and each fixture must hash to its
role's entry in that revision's set in the installer's
LEGACY_MANAGED_BODY_SETS before it is used, so a drifted fixture fails the
test instead of being admitted. The v0.2.20 fixtures are the factory bodies
(refusal-record directory `''`); the workspace case bakes into them the value
the real current engine wrote for the same checkout, as a v0.2.20 ensure run
there would have.
"""
from __future__ import annotations
import importlib.util
import hashlib
import os
import stat
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
LEGACY_FIXTURES = {
    "76659f5730ecf698b1963c656494d2cb66eb256d": ROOT / "tests/fixtures/legacy-managed-hooks-76659f",
    "a650b2034947eba87c809b1a7c9169f489aebd24": ROOT / "tests/fixtures/legacy-managed-hooks-a650b20",
}
# The installer imports its sibling helpers the way a script run finds them.
sys.path.insert(0, str(ROOT / "tools"))
SPEC = importlib.util.spec_from_file_location("owned_hook_installer", ROOT / "tools/install-canonical-hooks.py")
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("missing actual owning installer module")
INSTALLER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSTALLER)


class CommonHooksOwnership(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="evm-hook-ownership-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.primary = self.base / "primary"
        self.linked = self.base / "linked"
        self.primary.mkdir()
        self.git = shutil.which("git")
        if self.git is None:
            self.fail("missing declared native Git")
        self.command("init", "-q")
        self.command("config", "user.name", "Owned hook fixture")
        self.command("config", "user.email", "owned-hook-fixture@example.invalid")
        for name in INSTALLER.OWNING_DECLARATIONS:
            source = ROOT / name
            if source.is_symlink() or not source.is_file():
                self.fail(f"missing regular owning declaration: {name}")
            target = self.primary / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read_bytes())
        self.command("add", "--", *INSTALLER.OWNING_DECLARATIONS)
        self.command("commit", "-q", "-m", "Own exact declaration fixture")
        self.command("worktree", "add", "--detach", str(self.linked), "HEAD")

    def command(self, *args: str) -> str:
        return subprocess.check_output([self.git, *args], cwd=self.primary, text=True, stderr=subprocess.STDOUT).strip()

    def inventory(self) -> dict[str, tuple[str, int, str]]:
        result = {}
        for path in sorted(self.primary.rglob("*")):
            mode = stat.S_IMODE(path.lstat().st_mode)
            if path.is_symlink():
                result[str(path.relative_to(self.primary))] = ("link", mode, os.readlink(path))
            elif path.is_file():
                result[str(path.relative_to(self.primary))] = ("file", mode, hashlib.sha256(path.read_bytes()).hexdigest())
            elif path.is_dir():
                result[str(path.relative_to(self.primary))] = ("directory", mode, "")
            else:
                self.fail(f"unexpected special fixture path: {path}")
        return result

    def assert_installer_refuses(self, expected: str, extra_env: dict[str, str] | None = None, bootstrap: bool = False) -> None:
        repro = os.environ.get("REPROBUILD_REPRO") or shutil.which("repro")
        if not repro:
            self.fail("missing declared matching native Repro for real installer refusal control")
        env = os.environ.copy()
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        if extra_env:
            env.update(extra_env)
        before = self.inventory()
        result = subprocess.run(
            [os.sys.executable, str(self.primary / "tools/install-canonical-hooks.py"), "--repro", repro]
            + (["--bootstrap-managed"] if bootstrap else []),
            cwd=self.primary, env=env, capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(expected, result.stdout + result.stderr)
        self.assertEqual(self.inventory(), before, "refused installation changed owned filesystem bytes/types/modes")

    def test_missing_managed_bootstrap_refuses_before_writes(self) -> None:
        self.assert_installer_refuses("must be established by ownership-checked caller bootstrap")

    def test_real_fresh_matching_bootstrap(self) -> None:
        repro = os.environ.get("REPROBUILD_REPRO") or shutil.which("repro")
        if not repro:
            self.fail("missing declared matching engine")
        env = os.environ.copy()
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        source_before = {name: (self.primary / name).read_bytes() for name in INSTALLER.OWNING_DECLARATIONS}
        result = subprocess.run(
            [os.sys.executable, str(self.primary / "tools/install-canonical-hooks.py"),
                "--bootstrap-managed", "--repro", repro], cwd=self.primary,
            env=env, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Ownership-checked matching managed bootstrap complete", result.stdout)
        hooks = self.primary / ".git/hooks"
        self.assertEqual(set(INSTALLER.pre_push_inventory(hooks, INSTALLER.selected_tool("prek"))), {"pre-push", "pre-push.repro-managed", "pre-push.sample"})
        for name, expected in INSTALLER.KNOWN_MANAGED_BODIES.items():
            target = hooks / name
            self.assertFalse(target.is_symlink())
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o755)
            self.assertEqual(hashlib.sha256(target.read_bytes()).hexdigest(), expected)
        self.assertEqual({name: (self.primary / name).read_bytes() for name in INSTALLER.OWNING_DECLARATIONS}, source_before)

    def test_shell_entry_installation_leaves_working_tree_clean(self) -> None:
        # Run the installer exactly as the flake shell hook does: the real
        # interpreter, no -B, and PYTHONDONTWRITEBYTECODE removed, so nothing
        # but the installer itself can stop Python from caching the bytecode
        # of its sibling imports under tools/. The other real-installer cases
        # set PYTHONDONTWRITEBYTECODE and would hide such a write. The
        # pre-push gate counts untracked files, so entering the declared
        # environment must leave the working tree as clean as it found it.
        repro = os.environ.get("REPROBUILD_REPRO") or shutil.which("repro")
        if not repro:
            self.fail("missing declared matching engine")
        env = os.environ.copy()
        env.pop("PYTHONDONTWRITEBYTECODE", None)
        self.assertEqual(self.command("status", "--porcelain", "--untracked-files=all", "--ignored"), "")
        for extra in (["--bootstrap-managed"], []):
            result = subprocess.run(
                [os.sys.executable, "tools/install-canonical-hooks.py", "--repro", repro, *extra],
                cwd=self.primary, env=env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(
            (self.primary / "tools/__pycache__").exists(),
            "installer wrote a bytecode cache into the working tree",
        )
        self.assertEqual(self.command("status", "--porcelain", "--untracked-files=all", "--ignored"), "")

    def test_unknown_hook_refuses_before_bootstrap_writes(self) -> None:
        hook = self.primary / ".git/hooks/pre-commit"
        hook.write_bytes(b"#!/bin/sh\necho genuinely unknown owned fixture\n")
        hook.chmod(0o755)
        self.assert_installer_refuses("unknown or modified owning local hook body", bootstrap=True)

    def test_modified_native_sample_body_and_mode_refuse_before_bootstrap(self) -> None:
        sample = self.primary / ".git/hooks/pre-push.sample"
        original = sample.read_bytes()
        original_mode = stat.S_IMODE(sample.stat().st_mode)
        for mutation in ("body", "mode"):
            with self.subTest(mutation=mutation):
                try:
                    if mutation == "body":
                        sample.write_bytes(original + b"# genuinely modified sample\n")
                    else:
                        sample.chmod(0o644)
                    self.assert_installer_refuses("modified pre-push hook body or mode", bootstrap=True)
                finally:
                    sample.write_bytes(original)
                    sample.chmod(original_mode)
                self.assertEqual(sample.read_bytes(), original)
                self.assertEqual(stat.S_IMODE(sample.stat().st_mode), original_mode)

    def test_unknown_managed_body_refuses_before_writes(self) -> None:
        hook = self.primary / ".git/hooks/pre-commit"
        hook.write_bytes(b"#!/usr/bin/env sh\n# reprobuild hook dispatcher protocol=2\n# deliberate modified fixture\n")
        hook.chmod(0o755)
        self.assert_installer_refuses("unknown or modified managed hook template")

    def test_broken_existing_and_local_links_refuse_before_writes(self) -> None:
        for name in ("pre-commit", "pre-commit.repro-local"):
            with self.subTest(name=name):
                hook = self.primary / ".git/hooks" / name
                hook.symlink_to(self.base / "genuinely-absent-hook")
                try:
                    self.assert_installer_refuses("symlink")
                finally:
                    hook.unlink()

    def test_external_effective_global_path_refuses_before_writes(self) -> None:
        external = self.base / "external-hooks"
        external.mkdir()
        config = self.base / "private-global-gitconfig"
        subprocess.run([self.git, "config", "--file", str(config), "core.hooksPath", str(external)], check=True)
        original_config = config.read_bytes()
        self.assert_installer_refuses(
            "nonowning or unqualified hooks directory",
            {"GIT_CONFIG_GLOBAL": str(config), "GIT_CONFIG_NOSYSTEM": "1"},
        )
        self.assertEqual(config.read_bytes(), original_config)
        self.assertEqual(list(external.iterdir()), [])

    def test_symlink_common_hooks_refuses_before_writes(self) -> None:
        hooks = self.primary / ".git/hooks"
        backup = self.primary / ".git/hooks-fixture-backup"
        hooks.rename(backup)
        external = self.base / "external-hooks"
        external.mkdir()
        hooks.symlink_to(external, target_is_directory=True)
        self.assert_installer_refuses("own hooks directory is symlink")
        self.assertEqual(list(external.iterdir()), [])

    def test_primary_and_registered_linked_worktree(self) -> None:
        expected = self.primary / ".git"
        self.assertEqual(INSTALLER.owned_common_directory(self.git, self.primary), expected)
        self.assertEqual(INSTALLER.owned_common_directory(self.git, self.linked), expected)

    def test_different_config_refuses_without_changes(self) -> None:
        config = self.linked / ".pre-commit-config.yaml"
        original = config.read_bytes()
        config.write_bytes(original + b"\n# deliberately different owned fixture\n")
        before = self.command("config", "--local", "--list")
        with self.assertRaisesRegex(RuntimeError, "declaration differs"):
            INSTALLER.owned_common_directory(self.git, self.linked)
        self.assertEqual(self.command("config", "--local", "--list"), before)
        self.assertEqual(config.read_bytes(), original + b"\n# deliberately different owned fixture\n")

    def test_different_sdk_refuses(self) -> None:
        sdk = self.linked / "flake.nix"
        sdk.write_bytes(sdk.read_bytes() + b"\n# deliberately different SDK fixture\n")
        before = self.inventory()
        sdk_before = sdk.read_bytes()
        with self.assertRaisesRegex(RuntimeError, "declaration differs"):
            INSTALLER.owned_common_directory(self.git, self.linked)
        self.assertEqual(self.inventory(), before)
        self.assertEqual(sdk.read_bytes(), sdk_before)

    def test_other_managed_hooks_refuse_body_mode_and_extra_before_writes(self) -> None:
        self.test_real_fresh_matching_bootstrap()
        hooks = self.primary / ".git/hooks"
        for name in ("post-commit", "post-commit.repro-managed", "post-checkout", "post-checkout.repro-managed", "post-merge", "post-merge.repro-managed"):
            for mutation in ("body", "mode", "extra"):
                with self.subTest(name=name, mutation=mutation):
                    entry = hooks / (name + ".unqualified" if mutation == "extra" else name)
                    source = hooks / name
                    original = source.read_bytes()
                    # Use the genuinely bootstrapped managed body, then alter only this boundary.
                    if not source.is_file() or source.is_symlink():
                        self.fail(f"missing real owning managed hook source: {name}")
                    entry.write_bytes(source.read_bytes() + (b"\n# unknown modification\n" if mutation == "body" else b""))
                    entry.chmod(0o644 if mutation == "mode" else 0o755)
                    try:
                        self.assert_installer_refuses("unknown or modified", bootstrap=True)
                    finally:
                        if mutation == "extra":
                            entry.unlink()
                        else:
                            entry.write_bytes(original)
                            entry.chmod(0o755)

    def test_unregistered_link_refuses(self) -> None:
        unregistered = self.base / "unregistered"
        unregistered.mkdir()
        (unregistered / ".git").write_bytes((self.linked / ".git").read_bytes())
        with self.assertRaisesRegex(RuntimeError, "not registered"):
            INSTALLER.owned_common_directory(self.git, unregistered)

    def test_symlink_declaration_refuses(self) -> None:
        config = self.linked / ".pre-commit-config.yaml"
        config.unlink()
        config.symlink_to(self.primary / ".pre-commit-config.yaml")
        with self.assertRaisesRegex(RuntimeError, "not regular"):
            INSTALLER.owned_common_directory(self.git, self.linked)


    def test_configured_owner_native_generation_preserves_config(self) -> None:
        hooks = self.primary / ".git/hooks"
        self.command("config", "core.hooksPath", str(hooks))
        original = (self.primary / ".git/config").read_bytes()
        env = INSTALLER.native_install_environment(os.environ.copy(), self.git, self.primary, hooks)
        prek = shutil.which("prek")
        self.assertIsNotNone(prek, "missing declared native Prek")
        subprocess.run([prek, "install"], cwd=self.primary, env=env, check=True)
        generated = hooks / "pre-commit"
        self.assertEqual(generated.read_bytes(), INSTALLER.native_generated_body(prek))
        self.assertEqual(stat.S_IMODE(generated.stat().st_mode), 0o755)
        self.assertEqual((self.primary / ".git/config").read_bytes(), original)
        self.assertEqual(self.command("config", "--get", "core.hooksPath"), str(hooks))

    def test_ambiguous_command_configuration_refuses_without_writes(self) -> None:
        original = self.inventory()
        for key in ("GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"):
            env = os.environ.copy()
            env[key] = "1"
            with self.assertRaisesRegex(RuntimeError, "ambiguous inherited"):
                INSTALLER.native_install_environment(env, self.git, self.primary, self.primary / ".git/hooks")
            self.assertEqual(self.inventory(), original)

    # --- Reprobuild v0.2.20/v0.2.23 successors: legacy sets and the one
    # --- machine-local managed line. ---

    def make_workspace_checkout(self) -> None:
        # A real workspace checkout: the parent directory holds a workspace
        # .repro/ and the repository commits its own repro.lock, so
        # `repro hooks ensure` bakes a non-empty refusal-record directory.
        (self.base / ".repro").mkdir()
        shutil.copyfile(ROOT / "repro.lock", self.primary / "repro.lock")
        self.command("add", "--", "repro.lock")
        self.command("commit", "-q", "-m", "Commit the workspace lock")

    def install_legacy_bodies(self, revision: str, value: bytes | None = None) -> None:
        """Write one revision's captured legacy bodies, optionally baking `value`
        into the refusal-record assignment of each body that carries one."""
        hooks = self.primary / ".git/hooks"
        fixtures = LEGACY_FIXTURES[revision]
        expected = INSTALLER.LEGACY_MANAGED_BODY_SETS[revision]
        names = sorted(p.name for p in fixtures.iterdir())
        self.assertEqual(names, sorted(expected))
        for name in names:
            body = (fixtures / name).read_bytes()
            self.assertEqual(
                hashlib.sha256(body).hexdigest(), expected[name],
                f"legacy fixture is not the qualified {revision[:7]} body: {name}")
            self.assertNotEqual(expected[name], INSTALLER.KNOWN_MANAGED_BODIES.get(name),
                                f"legacy {revision[:7]} body equals the current one: {name}")
            if value is not None and INSTALLER.split_refusal_record_dir(body)[1] is not None:
                body = INSTALLER.render_refusal_record_dir(body, value)
            (hooks / name).write_bytes(body)
            (hooks / name).chmod(0o755)

    def test_legacy_sets_are_explicit_and_distinct(self) -> None:
        self.assertEqual(set(INSTALLER.LEGACY_MANAGED_BODY_SETS), set(LEGACY_FIXTURES))
        for revision, bodies in INSTALLER.LEGACY_MANAGED_BODY_SETS.items():
            with self.subTest(revision=revision):
                self.assertEqual(set(bodies), set(INSTALLER.KNOWN_MANAGED_BODIES))
                for name, digest in bodies.items():
                    self.assertIn(digest, INSTALLER.LEGACY_MANAGED_BODIES[name])
                    self.assertTrue(INSTALLER.qualified_managed_prior(name, digest))
        self.assertFalse(INSTALLER.qualified_managed_prior("pre-push.repro-managed", "0" * 64))

    def run_installer(self, bootstrap: bool) -> subprocess.CompletedProcess:
        repro = os.environ.get("REPROBUILD_REPRO") or shutil.which("repro")
        if not repro:
            self.fail("missing declared matching engine")
        env = os.environ.copy()
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        return subprocess.run(
            [os.sys.executable, str(self.primary / "tools/install-canonical-hooks.py"), "--repro", repro]
            + (["--bootstrap-managed"] if bootstrap else []),
            cwd=self.primary, env=env, capture_output=True, text=True,
        )

    def migrate(self) -> dict[str, bytes]:
        for bootstrap in (True, False):
            result = self.run_installer(bootstrap)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return self.current_values()

    def migrated_workspace_values(self) -> dict[str, bytes]:
        self.make_workspace_checkout()
        self.install_legacy_bodies("76659f5730ecf698b1963c656494d2cb66eb256d")
        return self.migrate()

    def current_values(self) -> dict[str, bytes]:
        hooks = self.primary / ".git/hooks"
        values = {}
        for name, digest in INSTALLER.KNOWN_MANAGED_BODIES.items():
            target = hooks / name
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o755)
            canonical, value = INSTALLER.split_refusal_record_dir(target.read_bytes())
            self.assertEqual(hashlib.sha256(canonical).hexdigest(), digest, name)
            if value is not None:
                values[name] = value
        return values

    def test_workspace_checkout_migrates_legacy_bodies_to_current(self) -> None:
        values = self.migrated_workspace_values()
        self.assertEqual(len(values), 5, values)
        self.assertEqual(len(set(values.values())), 1, values)
        value = next(iter(values.values()))
        self.assertNotEqual(value, b"''", "workspace checkout did not exercise a baked refusal-record directory")
        INSTALLER.validate_refusal_record_dir(value, self.primary.resolve())
        record = Path(os.fsdecode(INSTALLER.unquote_shell_posix_word(value)))
        self.assertIn(record.parents[4], {self.primary.resolve(), *self.primary.resolve().parents})

    def test_workspace_checkout_migrates_v0220_bodies_to_current(self) -> None:
        # The real current engine writes this checkout's refusal-record value
        # first; the v0.2.20 bodies then carry that same value, exactly as a
        # v0.2.20 ensure in this workspace checkout would have left them.
        value = next(iter(self.migrated_workspace_values().values()))
        self.assertNotEqual(value, b"''")
        self.install_legacy_bodies("a650b2034947eba87c809b1a7c9169f489aebd24", value)
        hooks = self.primary / ".git/hooks"
        for name, digest in INSTALLER.LEGACY_MANAGED_BODY_SETS["a650b2034947eba87c809b1a7c9169f489aebd24"].items():
            canonical, carried = INSTALLER.split_refusal_record_dir((hooks / name).read_bytes())
            self.assertEqual(hashlib.sha256(canonical).hexdigest(), digest, name)
            self.assertIn(carried, (None, value), name)
        values = self.migrate()
        self.assertEqual(len(values), 5, values)
        self.assertEqual(set(values.values()), {value})

    def test_v0220_body_with_foreign_value_refuses(self) -> None:
        self.make_workspace_checkout()
        outside = INSTALLER.quote_shell_posix(
            b"/nonexistent-elsewhere/.repro/build/reports/managed-hook-refusals/" + b"0" * 64)
        self.install_legacy_bodies("a650b2034947eba87c809b1a7c9169f489aebd24", outside)
        self.assert_installer_refuses("outside the checkout and its ancestors", bootstrap=True)

    def rewrite_refusal_value(self, name: str, value: bytes) -> None:
        target = self.primary / ".git/hooks" / name
        canonical, _ = INSTALLER.split_refusal_record_dir(target.read_bytes())
        target.write_bytes(INSTALLER.render_refusal_record_dir(canonical, value))

    def test_workspace_refusal_dir_faults_refuse_by_name(self) -> None:
        values = self.migrated_workspace_values()
        hooks = self.primary / ".git/hooks"
        originals = {name: (hooks / name).read_bytes() for name in INSTALLER.KNOWN_MANAGED_BODIES}
        outside = INSTALLER.quote_shell_posix(
            b"/nonexistent-elsewhere/.repro/build/reports/managed-hook-refusals/" + b"0" * 64)
        faults = {
            "outside the checkout and its ancestors": lambda: [self.rewrite_refusal_value(n, outside) for n in values],
            "carry different refusal-record directories": lambda: self.rewrite_refusal_value("post-merge.repro-managed", b"''"),
            "unknown or modified managed hook template": lambda: (hooks / "pre-push.repro-managed").write_bytes(
                originals["pre-push.repro-managed"].replace(b"set +x\n", b"set -x\n", 1)),
        }
        for expected, inject in faults.items():
            with self.subTest(fault=expected):
                inject()
                try:
                    self.assert_installer_refuses(expected, bootstrap=True)
                finally:
                    for name, body in originals.items():
                        (hooks / name).write_bytes(body)
                        (hooks / name).chmod(0o755)

    def test_refusal_dir_admits_only_canonical_values_beneath_the_checkout(self) -> None:
        key = b"a" * 64
        root = self.primary.resolve()
        tail = b"/.repro/build/reports/managed-hook-refusals/" + key
        for workspace in (root, root.parent):
            INSTALLER.validate_refusal_record_dir(INSTALLER.quote_shell_posix(os.fsencode(workspace) + tail), root)
        INSTALLER.validate_refusal_record_dir(b"''", root)
        refused = {
            "outside the checkout and its ancestors": INSTALLER.quote_shell_posix(os.fsencode(root / "nested") + tail),
            "not canonical shell quoting": b"'" + os.fsencode(root) + tail + b"'",
            "not 64 lowercase hex": INSTALLER.quote_shell_posix(os.fsencode(root) + tail[:-1] + b"A"),
            "not beneath .repro/build/reports/managed-hook-refusals": INSTALLER.quote_shell_posix(os.fsencode(root) + b"/.repro/other/" + key),
            "normalized absolute path": INSTALLER.quote_shell_posix(os.fsencode(root) + b"/../x" + tail),
        }
        for expected, value in refused.items():
            with self.subTest(expected=expected):
                with self.assertRaisesRegex(RuntimeError, expected):
                    INSTALLER.validate_refusal_record_dir(value, root)
        with self.assertRaisesRegex(RuntimeError, "more than once"):
            INSTALLER.split_refusal_record_dir(b"REPRO_REFUSAL_RECORD_DIR=''\nREPRO_REFUSAL_RECORD_DIR=''\n")

if __name__ == "__main__":
    unittest.main()
