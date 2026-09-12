#!/usr/bin/env python3
"""Check CLI reads against real server serialization using a disposable local DB."""

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


def check_reads(endpoint, directory):
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
    request("POST", admin + "/storages", {
        "id": "cli-test-fs", "kind": "fs", "root_path": str(root), "capacity_bytes": 1073741824,
    })
    request("POST", admin + "/clients", {"id": "cli-test", "storage_id": "cli-test-fs"})
    key = "sha256:" + "a" * 64
    request("POST", admin + "/clients/cli-test/keys", {"key_hash": key})
    credential = request("POST", admin + "/clients/cli-test/s3-credentials")
    paths = ["/storages", "/clients", "/clients/cli-test/keys", "/clients/cli-test/s3-credentials"]
    before = [request("GET", admin + path) for path in paths]
    env = {k: v for k, v in os.environ.items() if not k.startswith(("FILEGATE_", "GROVE_"))}
    env.pop("DATABASE_URL", None)
    env.update(GROVE_ENDPOINT=endpoint, GROVE_OPERATOR_TOKEN=TOKEN, NO_PROXY="127.0.0.1")
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
        result = subprocess.run(
            [str(CLI), "--output", "json", "--timeout", "5", *args],
            cwd=directory, env=env, capture_output=True, text=True, timeout=10, check=True,
        )
        assert not result.stderr
        output = json.loads(result.stdout)
        assert output["ok"] and output["error"] is None
        assert TOKEN not in result.stdout and credential["secret_key"] not in result.stdout
        if path is not None:
            assert output["data"] == request("GET", admin + path), args
        else:
            assert output["data"]["registry"]["storage_count"] == 1
            assert output["data"]["registry"]["client_count"] == 1
            assert output["data"]["storage_access"] == "not_checked"
        print("PASS", " ".join(args))
    assert before == [request("GET", admin + path) for path in paths]
    print("PASS registry unchanged across CLI reads")


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
                    check_reads(endpoint, directory)
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
