#!/usr/bin/env python3
"""Keep org-scoped CI Git authentication across the SDK repository-env scrub.

The SDK deliberately removes indexed Git configuration at a cross-repository
boundary. This declared Git wrapper reconstructs only the caller's authorized
CI org header immediately before the genuine Git executable. It writes no
credential files and places no credential on an argument vector.
"""
import base64
import os
import sys

TOKEN_ENV = "EVM_CI_GITHUB_TOKEN"
ORG_URL = "https://github.com/metacraft-labs/"


def git_environment(source):
    result = dict(source)
    for name in tuple(result):
        if name in {"GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"} or name.startswith(
            ("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_")
        ):
            del result[name]
    result["GIT_TERMINAL_PROMPT"] = "0"
    token = result.get(TOKEN_ENV, "")
    if not token:
        return result
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
    os.execve(git, [git, *sys.argv[2:]], git_environment(os.environ))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError):
        print("Declared CI Git authentication setup refused", file=sys.stderr)
        sys.exit(78)
