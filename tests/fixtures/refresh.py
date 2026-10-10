#!/usr/bin/env python3
"""Capture stable API examples from the public Rust Playground and Godbolt APIs."""

import datetime
import json
import pathlib
import re
import sys
import urllib.error
import urllib.request


ROOT = pathlib.Path(__file__).resolve().parent
PLAYGROUND = "https://play.rust-lang.org"
GODBOLT = "https://godbolt.org"


def request(method, url, body=None):
    encoded = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        url,
        data=encoded,
        method=method,
        headers={"Accept": "application/json", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=45) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def capture(name, method, url, body=None):
    status, response = request(method, url, body)
    ROOT.mkdir(parents=True, exist_ok=True)
    (ROOT / name).write_bytes(response)
    (ROOT / f"{name}.status").write_text(f"{status}\n")
    return status, response


def require_json(name, response):
    try:
        return json.loads(response)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"{name} did not return JSON: {error}") from error


def microbench_source():
    # Capture actual measurements using the same runner as the bot, rather
    # than asking Playground to print fabricated benchmark timings.
    source = (ROOT.parent.parent / "src/commands/playground/microbench.rs").read_text()
    runner = re.search(r'const BENCH_FUNCTION: &str = r#"(.*?)"#;', source, re.DOTALL)
    if runner is None:
        raise RuntimeError("Could not find BENCH_FUNCTION in the microbench command")
    return (
        "pub fn first() { let _ = 1; }\n"
        "pub fn second() { let _ = 2; }\n"
        + runner.group(1)
        + '\nfn main() { bench(&[("first", first), ("second", second)]); }'
    )


def main():
    captured = {}

    status, response = capture(
        "playground-execute-success.json",
        "POST",
        f"{PLAYGROUND}/execute",
        {
            "channel": "stable",
            "edition": "2021",
            "code": 'fn main() { println!("Hello, Rust!"); }',
            "crateType": "bin",
            "mode": "debug",
            "tests": False,
        },
    )
    success = require_json("playground success", response)
    if not 200 <= status < 300 or success.get("success") is not True:
        raise RuntimeError("Playground success capture was not a successful execution")
    captured["playground_execute_success"] = status

    status, response = capture(
        "playground-execute-compile-error.json",
        "POST",
        f"{PLAYGROUND}/execute",
        {
            "channel": "stable",
            "edition": "2021",
            "code": 'fn main() { let _: u32 = "hello"; }',
            "crateType": "bin",
            "mode": "debug",
            "tests": False,
        },
    )
    failure = require_json("playground compile error", response)
    if not 200 <= status < 300 or failure.get("success") is not False:
        raise RuntimeError("Playground compile-error capture was not a compiler failure")
    captured["playground_execute_compile_error"] = status

    status, response = capture(
        "playground-execute-microbench-success.json",
        "POST",
        f"{PLAYGROUND}/execute",
        {
            "channel": "stable",
            "edition": "2021",
            "code": microbench_source(),
            "crateType": "bin",
            "mode": "release",
            "tests": False,
        },
    )
    bench = require_json("Playground microbench success", response)
    if not 200 <= status < 300 or bench.get("success") is not True:
        raise RuntimeError("Playground microbench fixture was not a successful execution")
    captured["playground_execute_microbench_success"] = status

    status, response = capture(
        "godbolt-compilers-rust.json",
        "GET",
        f"{GODBOLT}/api/compilers/rust",
    )
    compilers = require_json("Godbolt compiler list", response)
    nightly = next((compiler for compiler in compilers if compiler.get("semver") == "nightly"), None)
    if not 200 <= status < 300 or nightly is None:
        raise RuntimeError("Godbolt compiler metadata had no nightly compiler")
    compiler_id = nightly["id"]
    captured["godbolt_compilers_rust"] = status

    status, response = capture(
        "godbolt-libraries-rust.json",
        "GET",
        f"{GODBOLT}/api/libraries/rust",
    )
    require_json("Godbolt library list", response)
    if not 200 <= status < 300:
        raise RuntimeError("Godbolt library metadata request failed")
    captured["godbolt_libraries_rust"] = status

    for case, source in (
        (
            "success",
            "#[unsafe(no_mangle)] pub fn add_one(x: u32) -> u32 { x + 1 }",
        ),
        (
            "compile-error",
            "pub fn add_one( -> u32 {",
        ),
    ):
        fixture_name = (
            "godbolt-compile-error.json"
            if case == "compile-error"
            else f"godbolt-compile-{case}.json"
        )
        status, response = capture(
            fixture_name,
            "POST",
            f"{GODBOLT}/api/compiler/{compiler_id}/compile",
            {
                "source": source,
                "options": {
                    "userArguments": "-Copt-level=3 --edition=2024",
                    "tools": [],
                },
            },
        )
        result = require_json(f"Godbolt compile {case}", response)
        if not 200 <= status < 300:
            raise RuntimeError(f"Godbolt compile {case} request returned HTTP {status}")
        if case == "success" and not result.get("asm"):
            raise RuntimeError("Godbolt successful compile did not contain assembly")
        if case == "compile-error" and not result.get("stderr"):
            raise RuntimeError("Godbolt compile-error response did not contain diagnostics")
        status_name = (
            "godbolt_compile_error"
            if case == "compile-error"
            else f"godbolt_compile_{case}"
        )
        captured[status_name] = status

    index = {
        "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "source": {
            "playground": PLAYGROUND,
            "godbolt": GODBOLT,
        },
        "godbolt_nightly_compiler_id": compiler_id,
        "http_statuses": captured,
        "notes": "Bodies are direct upstream JSON responses; CI replays these files offline.",
    }
    (ROOT / "index.json").write_text(json.dumps(index, indent=2) + "\n")
    print(json.dumps(index, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, urllib.error.URLError) as error:
        print(f"fixture refresh failed: {error}", file=sys.stderr)
        sys.exit(1)
