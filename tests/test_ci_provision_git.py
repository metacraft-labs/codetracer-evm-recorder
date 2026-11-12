"""Unit tests for nix/ci-provision-git.py's environment rewrite.

The rewrite is exercised directly. The global-config cases run the real
`git config --file` against real files, as the wrapper does; no network is
involved. The job-credential-broker cases use a real credential directory and
a real stand-in broker process: a sleeping Python interpreter whose argument
vector ends in `job-credentials.cjs daemon`, which is all the ownership test
reads of the real broker. One case replaces the wrapper's token reader, and
only to reach the whitespace guard behind it: the reader splits entries on
whitespace, so no file can deliver such a token, and the guard is kept because
the reference implementation keeps it.
"""
import base64
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
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


def org_header_token(env):
    """The token carried by the org header the wrapper added."""
    value = env["GIT_CONFIG_VALUE_0"]
    prefix = "AUTHORIZATION: basic "
    assert value.startswith(prefix), value
    user, _, token = base64.b64decode(value[len(prefix):]).decode().partition(":")
    assert user == "x-access-token", user
    return token


class JobCredentialBroker(unittest.TestCase):
    """The broker's current token replaces the job-start one while it runs."""

    JOB_START = "jobstarttoken"
    FRESH = "brokertoken"

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.dir = Path(directory.name)
        self.write_tokens(
            "access-tokens = gitlab.com=other github.com=%s\n" % self.FRESH)

    def write_tokens(self, text):
        (self.dir / "nix-access-tokens.conf").write_text(text)

    def start(self, argv):
        process = subprocess.Popen(
            argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL)

        def stop():
            process.kill()
            process.wait()
        self.addCleanup(stop)
        return process

    def start_broker(self):
        # The script path is never opened: `-c` runs the sleep, and the
        # remaining arguments only shape /proc/<pid>/cmdline.
        process = self.start([
            sys.executable, "-c", "import time; time.sleep(1e9)",
            str(self.dir / "stand-in" / "job-credentials.cjs"), "daemon"])
        (self.dir / "broker.pid").write_text("%d\n" % process.pid)
        return process

    def token(self, extra=None):
        source = {
            "EVM_CI_GITHUB_TOKEN": self.JOB_START,
            "MCL_JOB_CREDENTIALS_DIR": str(self.dir),
        }
        source.update(extra or {})
        return org_header_token(MODULE.git_environment(source))

    def test_running_broker_supplies_the_token(self):
        self.start_broker()
        self.assertEqual(self.token(), self.FRESH)

    def test_rotated_token_is_read_on_every_invocation(self):
        self.start_broker()
        self.assertEqual(self.token(), self.FRESH)
        self.write_tokens("access-tokens = github.com=rotatedtoken\n")
        self.assertEqual(self.token(), "rotatedtoken")

    def test_stopped_broker_owns_nothing(self):
        self.start_broker()
        (self.dir / "broker.stop").write_text("")
        self.assertEqual(self.token(), self.JOB_START)

    def test_dead_pid_owns_nothing(self):
        process = self.start_broker()
        process.kill()
        process.wait()
        self.assertEqual(self.token(), self.JOB_START)

    def test_unrelated_live_process_owns_nothing(self):
        process = self.start(
            [sys.executable, "-c", "import time; time.sleep(1e9)"])
        (self.dir / "broker.pid").write_text("%d\n" % process.pid)
        self.assertEqual(self.token(), self.JOB_START)

    def test_malformed_pid_owns_nothing(self):
        process = self.start_broker()
        for text in (
            "", "0", "-%d" % process.pid, "0%d" % process.pid,
            "%d x" % process.pid, "9" * 30,
        ):
            with self.subTest(pid=text):
                (self.dir / "broker.pid").write_text(text)
                self.assertEqual(self.token(), self.JOB_START)

    def test_missing_pid_file_owns_nothing(self):
        self.assertEqual(self.token(), self.JOB_START)

    def test_no_credential_directory_keeps_the_job_start_token(self):
        self.start_broker()
        self.assertEqual(
            org_header_token(MODULE.git_environment(
                {"EVM_CI_GITHUB_TOKEN": self.JOB_START})),
            self.JOB_START)
        self.assertEqual(
            self.token({"MCL_JOB_CREDENTIALS_DIR": ""}), self.JOB_START)

    def test_broker_without_a_github_token_falls_back(self):
        self.start_broker()
        for text in (
            "access-tokens = gitlab.com=other\n",
            "access-tokens = github.com=\n",
            "access-tokens = github.com=%s\r\n" % self.FRESH,
        ):
            with self.subTest(file=text):
                self.write_tokens(text)
                self.assertEqual(self.token(), self.JOB_START)
        (self.dir / "nix-access-tokens.conf").unlink()
        self.assertEqual(self.token(), self.JOB_START)

    def test_whitespace_in_the_broker_token_falls_back(self):
        # Entries are split on whitespace, so a token that still carries some
        # cannot come from a well-formed file. The guard is exercised through
        # the reader directly.
        self.start_broker()
        original = MODULE.read_access_tokens
        MODULE.read_access_tokens = lambda _: {"github.com": "fresh token"}
        self.addCleanup(setattr, MODULE, "read_access_tokens", original)
        self.assertEqual(self.token(), self.JOB_START)

    def test_malformed_broker_file_is_refused(self):
        self.start_broker()
        self.write_tokens("access-tokens = github.com\n")
        with self.assertRaises(ValueError):
            self.token()

    def test_broker_token_needs_the_callers_authorization(self):
        # Without EVM_CI_GITHUB_TOKEN the caller asked for no org header, and
        # a running broker does not change that.
        self.start_broker()
        env = MODULE.git_environment(
            {"MCL_JOB_CREDENTIALS_DIR": str(self.dir)})
        self.assertNotIn("GIT_CONFIG_COUNT", env)

    def test_invalid_broker_token_is_refused(self):
        self.start_broker()
        self.write_tokens("access-tokens = github.com=caf\u00e9\n")
        with self.assertRaises(ValueError):
            self.token()

    def test_wrapper_execs_git_with_the_broker_header(self):
        # End to end through the wrapper's own entry point: the genuine Git
        # reads the header back out of its configuration.
        self.start_broker()
        env = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": str(self.dir),
            "EVM_CI_GITHUB_TOKEN": self.JOB_START,
            "MCL_JOB_CREDENTIALS_DIR": str(self.dir),
        }
        result = subprocess.run(
            [
                sys.executable, "-B", "-I", str(SOURCE), GIT, "config",
                "--get", "http.https://github.com/metacraft-labs/.extraheader",
            ],
            env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True,
            check=True)
        self.assertEqual(
            org_header_token({"GIT_CONFIG_VALUE_0": result.stdout.strip()}),
            self.FRESH)


if __name__ == "__main__":
    unittest.main()
