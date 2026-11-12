#!/usr/bin/env python3
"""Keep org-scoped CI Git authentication across the SDK repository-env scrub.

The SDK deliberately removes indexed Git configuration at a cross-repository
boundary. This declared Git wrapper reconstructs only the caller's authorized
CI org header immediately before the genuine Git executable. It writes no
credential files and places no credential on an argument vector.

When the caller's own global configuration (``GIT_CONFIG_GLOBAL``) already
sets an ``extraHeader`` for the org URL, that file owns authentication for
the command and the wrapper adds nothing. setup-dev-env's locked-sibling fetch
does exactly that: it renders the org header into a private global config file
for the one command. Adding the wrapper's header as well gave Git the same
``extraHeader`` twice, and GitHub refused the request ("Duplicate header:
\"Authorization\"", HTTP 400). A global config that carries no such header
(the job-wide one some steps point at) does not count: there the wrapper's
header is still the only credential.

The token comes from the job credential broker when one owns the job's GitHub
credential, and from ``EVM_CI_GITHUB_TOKEN`` otherwise. The workflow sets
``EVM_CI_GITHUB_TOKEN`` from the GitHub App installation token minted at job
start, and such a token expires an hour later. A setup that builds reprobuild
from source runs past that hour, so the job-start token rebuilt here replaced
the fresh header setup-dev-env had handed its sibling clone, and the clone of a
public repository failed with Git exit 128 (codetracer-evm-recorder PR #17,
job 131418346111). setup-dev-env's broker keeps a current token in the job
credential directory (``MCL_JOB_CREDENTIALS_DIR``) and rotates it in place, so
the file is read on every invocation. The broker token is used only where the
caller authorized the org header by setting ``EVM_CI_GITHUB_TOKEN``; without it
the wrapper still adds nothing.

The ownership test is the one metacraft-github-actions applies in
``refresh-build-auth/job-credentials.cjs`` (``brokerGithubToken`` and
``isBrokerProcess``) and in setup-nix's ``broker_owns_token``:
https://github.com/metacraft-labs/metacraft-github-actions/blob/80d5d9e496882eea0dacce37bbd22cb7e9814f03/refresh-build-auth/job-credentials.cjs
Keep the three in step.
"""
import base64
import os
import re
import subprocess
import sys

TOKEN_ENV = "EVM_CI_GITHUB_TOKEN"
CREDENTIALS_DIR_ENV = "MCL_JOB_CREDENTIALS_DIR"
ORG_URL = "https://github.com/metacraft-labs/"

# File names inside the job credential directory, as job-credentials.cjs
# writes them.
BROKER_PID = "broker.pid"
BROKER_STOP = "broker.stop"
NIX_TOKENS = "nix-access-tokens.conf"
# JavaScript's Number.MAX_SAFE_INTEGER; job-credentials.cjs rejects larger pids.
MAX_SAFE_INTEGER = 2**53 - 1


def is_broker_process(pid_text):
    """Whether ``pid_text`` names a live job credential broker.

    A positive decimal pid of a live process whose argument vector ends in
    ``job-credentials.cjs daemon``. Without procfs liveness alone decides; with
    procfs, an unreadable entry is a pid that exited after the signal.
    """
    if not re.fullmatch(r"[1-9][0-9]*", pid_text):
        return False
    pid = int(pid_text)
    if pid > MAX_SAFE_INTEGER:
        return False
    try:
        os.kill(pid, 0)
    except (OSError, OverflowError):
        return False
    try:
        with open("/proc/%d/cmdline" % pid, "rb") as handle:
            argv = handle.read().decode("utf-8", "replace").split("\0")
    except OSError:
        return not os.path.exists("/proc/self")
    if argv and argv[-1] == "":
        argv.pop()
    return (
        len(argv) >= 2
        and argv[-1] == "daemon"
        and os.path.basename(argv[-2]) == "job-credentials.cjs"
    )


def read_access_tokens(directory):
    """The ``host=token`` map of the broker's ``nix-access-tokens.conf``.

    A malformed entry raises ValueError, as job-credentials.cjs throws: the
    file is the broker's own output, so a malformed one is a fault to report,
    not a reason to fall back silently.
    """
    path = os.path.join(directory, NIX_TOKENS)
    try:
        with open(path, encoding="utf-8", errors="replace", newline="") as handle:
            text = handle.read()
    except FileNotFoundError:
        return {}
    tokens = {}
    for line in text.split("\n"):
        # JavaScript's "." stops at a line terminator, so a line carrying one
        # (a CRLF file) is not an entry there either.
        match = re.fullmatch(r"\s*access-tokens\s*=([^\r\u2028\u2029]*)", line)
        if not match:
            continue
        for entry in filter(None, re.split(r"\s+", match.group(1).strip())):
            at = entry.find("=")
            if at <= 0:
                raise ValueError("malformed access-tokens entry in job credentials")
            tokens[entry[:at]] = entry[at + 1:]
    return tokens


def broker_github_token(directory):
    """The GitHub token a running broker has installed, or "" when none owns it."""
    if not directory:
        return ""
    if os.path.exists(os.path.join(directory, BROKER_STOP)):
        return ""
    try:
        pid_path = os.path.join(directory, BROKER_PID)
        with open(pid_path, encoding="utf-8", errors="replace") as handle:
            pid_text = handle.read().strip()
    except OSError:
        return ""
    if not is_broker_process(pid_text):
        return ""
    token = read_access_tokens(directory).get("github.com", "")
    return "" if re.search(r"\s", token) else token


def global_config_sets_org_header(git, source):
    """Whether the ``GIT_CONFIG_GLOBAL`` file in ``source`` sets the org header.

    Read with the genuine Git so the file's own syntax rules apply; only the
    config path is on the argument vector, never a credential.
    """
    path = source.get("GIT_CONFIG_GLOBAL", "")
    if not path or git is None:
        return False
    probe = subprocess.run(
        [git, "config", "--file", path, "--get-all", "http." + ORG_URL + ".extraheader"],
        env={"PATH": source.get("PATH", ""), "HOME": source.get("HOME", "")},
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return probe.returncode == 0 and bool(probe.stdout.strip())


def git_environment(source, git=None):
    result = dict(source)
    for name in tuple(result):
        if name in {"GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"} or name.startswith(
            ("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_")
        ):
            del result[name]
    result["GIT_TERMINAL_PROMPT"] = "0"
    token = result.get(TOKEN_ENV, "")
    if not token or global_config_sets_org_header(git, result):
        return result
    token = broker_github_token(result.get(CREDENTIALS_DIR_ENV, "")) or token
    if any(ord(c) < 33 or ord(c) > 126 for c in token):
        raise ValueError("CI Git credential has an invalid character")
    for name in tuple(result):
        if name.startswith("GIT_TRACE") or name == "GIT_CURL_VERBOSE":
            del result[name]
    header = "AUTHORIZATION: basic " + base64.b64encode(
        ("x-access-token:" + token).encode("ascii")
    ).decode("ascii")
    result.update(
        GIT_CONFIG_COUNT="2",
        GIT_CONFIG_KEY_0="http." + ORG_URL + ".extraHeader",
        GIT_CONFIG_VALUE_0=header,
        GIT_CONFIG_KEY_1="http." + ORG_URL + ".followRedirects",
        GIT_CONFIG_VALUE_1="false",
    )
    return result


def main():
    if len(sys.argv) < 2 or not os.path.isabs(sys.argv[1]):
        raise ValueError("Declared absolute Git executable is required")
    git = sys.argv[1]
    os.execve(git, [git, *sys.argv[2:]], git_environment(os.environ, git))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError):
        print("Declared CI Git authentication setup refused", file=sys.stderr)
        sys.exit(78)
