"""Unit tests for nix/ci-provision-git.py's environment rewrite.

The rewrite is exercised directly. The global-config cases run the real
`git config --file` against real files, as the wrapper does; no network is
involved.
"""
import importlib.util
import os
from pathlib import Path
import shutil
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "nix" / "ci-provision-git.py"
SPEC = importlib.util.spec_from_file_location("ci_provision_git", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

ORG_KEY = "http.https://github.com/metacraft-labs/.extraHeader"
GIT = shutil.which("git")


class GitEnvironment(unittest.TestCase):
    def test_token_adds_exactly_one_org_header(self):
        env = MODULE.git_environment({
            "EVM_CI_GITHUB_TOKEN": "abc",
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": ORG_KEY,
            "GIT_CONFIG_VALUE_0": "AUTHORIZATION: basic stale",
        })
        self.assertEqual(env["GIT_CONFIG_COUNT"], "2")
        self.assertEqual(env["GIT_CONFIG_KEY_0"], ORG_KEY)
        self.assertTrue(env["GIT_CONFIG_VALUE_0"].startswith("AUTHORIZATION: basic "))
        self.assertNotEqual(env["GIT_CONFIG_VALUE_0"], "AUTHORIZATION: basic stale")
        self.assertEqual(env["GIT_TERMINAL_PROMPT"], "0")

    def test_no_token_adds_no_header(self):
        env = MODULE.git_environment({
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": ORG_KEY,
            "GIT_CONFIG_VALUE_0": "x",
        })
        self.assertNotIn("GIT_CONFIG_COUNT", env)
        self.assertNotIn("GIT_CONFIG_KEY_0", env)

    def global_config(self, text):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "config"
        path.write_text(text)
        return str(path)

    def test_caller_global_config_with_the_org_header_owns_authentication(self):
        # setup-dev-env's private global config already carries the header;
        # a second one is a duplicate GitHub refuses.
        path = self.global_config(
            '[http "https://github.com/metacraft-labs/"]\n'
            "\textraHeader = AUTHORIZATION: basic private\n")
        env = MODULE.git_environment({
            "EVM_CI_GITHUB_TOKEN": "abc",
            "GIT_CONFIG_GLOBAL": path,
            "PATH": os.environ.get("PATH", ""),
        }, GIT)
        self.assertEqual(env["GIT_CONFIG_GLOBAL"], path)
        self.assertNotIn("GIT_CONFIG_COUNT", env)
        self.assertNotIn("GIT_CONFIG_KEY_0", env)

    def test_global_config_without_the_org_header_still_gets_one(self):
        # A job-wide global config with no credential of its own must not
        # suppress the wrapper's header, or the fetch goes unauthenticated.
        path = self.global_config("[core]\n\tlongpaths = true\n")
        env = MODULE.git_environment({
            "EVM_CI_GITHUB_TOKEN": "abc",
            "GIT_CONFIG_GLOBAL": path,
            "PATH": os.environ.get("PATH", ""),
        }, GIT)
        self.assertEqual(env["GIT_CONFIG_COUNT"], "2")
        self.assertEqual(env["GIT_CONFIG_KEY_0"], ORG_KEY)

    def test_invalid_token_is_refused(self):
        with self.assertRaises(ValueError):
            MODULE.git_environment({"EVM_CI_GITHUB_TOKEN": "a b"})


if __name__ == "__main__":
    unittest.main()
