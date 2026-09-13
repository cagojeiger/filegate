#!/usr/bin/env python3
"""Exercise the gscli registry lifecycle against a disposable FileGate API."""

import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid


ROOT = Path(__file__).resolve().parent.parent
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
SERVER = TARGET / "debug" / "filegate"
CLI = TARGET / "debug" / "gscli"
TOKEN = "cli-local-integration-token"


def docker(*args):
    return subprocess.check_output(["docker", *args], text=True, timeout=90).strip()


def check_lifecycle(endpoint, directory):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            endpoint + path, data=data, method=method,
            headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
        )
        with opener.open(req, timeout=5) as response:
            payload = response.read()
            return None if not payload else json.loads(payload)

    deadline = time.monotonic() + 20
    while True:
        try:
            if request("GET", "/readyz") == {"status": "ready"}:
                break
        except (urllib.error.URLError, TimeoutError):
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError("Test server did not become ready")
        time.sleep(0.1)

    admin = "/api/admin/v1"
    root = Path(directory) / "objects"
    root.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("FILEGATE_", "GROVE_"))}
    env.pop("DATABASE_URL", None)
    env.update(GROVE_ENDPOINT=endpoint, GROVE_OPERATOR_TOKEN=TOKEN, NO_PROXY="127.0.0.1")

    def run_cli(*args):
        result = subprocess.run(
            [str(CLI), "--output", "json", "--timeout", "5", *args],
            cwd=directory, env=env, capture_output=True, text=True, timeout=10,
        )
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        assert not result.stderr
        output = json.loads(result.stdout)
        assert output["ok"] and output["error"] is None
        assert TOKEN not in result.stdout
        print("PASS", " ".join(args))
        return output, result

    storage_spec = Path(directory) / "storage.json"
    storage_spec.write_text(json.dumps({
        "kind": "fs", "root_path": str(root), "capacity_bytes": 1073741824,
    }))
    run_cli("storage", "create", "cli-test-fs", "--from", str(storage_spec))
    run_cli("client", "create", "cli-test", "--storage", "cli-test-fs")

    raw_key = "cli-test-native-key"
    key_file = Path(directory) / "client-key"
    key_file.write_text(raw_key + "\n")
    key = "sha256:" + hashlib.sha256(raw_key.encode()).hexdigest()
    output, result = run_cli(
        "client-key", "register", "--client", "cli-test", "--key-file", str(key_file),
    )
    assert output["data"]["key_hash"] == key
    assert raw_key not in result.stdout

    secret_file = Path(directory) / "s3-credential.json"
    output, result = run_cli(
        "credential", "create", "--client", "cli-test", "--secret-out", str(secret_file),
    )
    credential = json.loads(secret_file.read_text())
    assert output["data"]["access_key_id"] == credential["access_key_id"]
    assert output["data"]["file_state"] == "saved"
    assert credential["secret_key"] not in result.stdout
    assert secret_file.stat().st_mode & 0o777 == 0o600

    storage_spec.write_text(json.dumps({
        "kind": "fs", "root_path": str(root), "capacity_bytes": 2147483648,
    }))
    run_cli("storage", "replace", "cli-test-fs", "--from", str(storage_spec), "--yes")

    cases = [
        (["status"], None),
        (["storage", "list"], "/storages"),
        (["storage", "show", "cli-test-fs"], "/storages/cli-test-fs"),
        (["client", "list"], "/clients"),
        (["client", "show", "cli-test"], "/clients/cli-test"),
        (["credential", "list", "--client", "cli-test"], "/clients/cli-test/s3-credentials"),
        (["client-key", "list", "--client", "cli-test"], "/clients/cli-test/keys"),
        (["usage", "storages"], "/usage"),
        (["usage", "clients"], "/usage/clients"),
        (["usage", "history", "--days", "7"], "/usage/history?days=7"),
    ]
    for args, path in cases:
        output, result = run_cli(*args)
        assert credential["secret_key"] not in result.stdout
        if path is not None:
            assert output["data"] == request("GET", admin + path), args
        else:
            assert output["data"]["registry"]["storage_count"] == 1
            assert output["data"]["registry"]["client_count"] == 1
            assert output["data"]["storage_access"] == "not_checked"

    access_key_id = credential["access_key_id"]
    run_cli("credential", "delete", "--client", "cli-test", access_key_id, "--yes")
    run_cli("client-key", "delete", "--client", "cli-test", key, "--yes")
    run_cli("client", "delete", "cli-test", "--yes")
    run_cli("storage", "delete", "cli-test-fs", "--yes")
    assert request("GET", admin + "/clients") == []
    assert request("GET", admin + "/storages") == []
    print("PASS registry lifecycle completed without Terraform")


def main():
    if not SERVER.is_file() or not CLI.is_file():
        raise RuntimeError("Run cargo build --bin filegate --bin gscli --locked first")
    container = "filegate-cli-e2e-" + uuid.uuid4().hex[:12]
    try:
        docker("run", "--rm", "-d", "--name", container,
               "-e", "POSTGRES_USER=filegate", "-e", "POSTGRES_PASSWORD=filegate",
               "-e", "POSTGRES_DB=filegate", "-p", "127.0.0.1::5432", "postgres:17-alpine")
        db_port = docker("port", container, "5432").rsplit(":", 1)[1]
        with tempfile.TemporaryDirectory(prefix="filegate-cli-e2e-") as directory:
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            endpoint = f"http://127.0.0.1:{port}"
            env = {k: v for k, v in os.environ.items() if not k.startswith("FILEGATE_")}
            env.update(
                FILEGATE_DATABASE_URL=f"postgres://filegate:filegate@127.0.0.1:{db_port}/filegate",
                FILEGATE_ENC_ROOT_SECRET="local-cli-integration-root-secret-32bytes",
                FILEGATE_OPERATOR_TOKENS=TOKEN, FILEGATE_BIND=f"127.0.0.1:{port}",
                FILEGATE_PUBLIC_URL=endpoint, FILEGATE_LOG_FORMAT="json",
            )
            deadline = time.monotonic() + 20
            while subprocess.run(["docker", "exec", container, "pg_isready", "-h", "127.0.0.1", "-U", "filegate"],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5).returncode:
                if time.monotonic() >= deadline:
                    raise RuntimeError("Test database did not become ready")
                time.sleep(0.2)
            with tempfile.TemporaryFile() as log:
                server = subprocess.Popen([str(SERVER)], env=env, cwd=directory, stdout=log, stderr=log)
                try:
                    check_lifecycle(endpoint, directory)
                finally:
                    server.terminate()
                    try:
                        server.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait(timeout=5)
    finally:
        subprocess.run(["docker", "rm", "-f", container],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30, check=True)
    print("PASS disposable server, database, and files cleaned up")


if __name__ == "__main__":
    main()
