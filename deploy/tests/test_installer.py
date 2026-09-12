import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "installed bin"
        self.bin.mkdir()
        self.destination = self.bin / "gscli"
        self.destination.write_text("old binary")
        self.mocks = self.root / "mocks"
        self.mocks.mkdir()
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.log = self.root / "downloads"
        self.env = dict(os.environ, PATH=f"{self.mocks}:{os.environ['PATH']}",
                        TEST_ASSETS=str(self.assets), TEST_LOG=str(self.log),
                        TEST_OS="Linux", TEST_ARCH="x86_64")
        self.executable("uname", '#!/bin/sh\ncase "$1" in -s) echo "$TEST_OS";; -m) echo "$TEST_ARCH";; esac\n')
        self.executable("curl", f"#!{sys.executable}\n" + '''
import os, pathlib, shutil, sys
args = sys.argv[1:]
assert args[args.index('--proto') + 1] == '=https'
assert args[args.index('--proto-redir') + 1] == '=https'
url = next(a for a in args if a.startswith('https://'))
assert url.startswith('https://github.com/cagojeiger/filegate/releases/download/v1.2.3/')
with open(os.environ['TEST_LOG'], 'a') as log:
    log.write(url + '\\n')
shutil.copyfile(pathlib.Path(os.environ['TEST_ASSETS']) / url.rsplit('/', 1)[1], args[args.index('--output') + 1])
''')
        self.asset = self.assets / "gscli-x86_64-unknown-linux-gnu"
        self.asset.write_text(self.fixture_binary())
        self.hash_asset()

    @staticmethod
    def fixture_binary():
        # The native-binary smoke below exercises the real install implementation.
        return '''#!/bin/sh
case "$1" in
    --version) echo "gscli 1.2.3" ;;
    __install) cp -p "$0" "$3/gscli" ;;
    *) exit 2 ;;
esac
'''

    def executable(self, name, contents):
        path = self.mocks / name
        path.write_text(contents)
        path.chmod(0o755)

    def hash_asset(self):
        self.asset.with_suffix('.sha256').write_text(hashlib.sha256(self.asset.read_bytes()).hexdigest() + '\n')

    def run_installer(self, *args):
        result = subprocess.run(['sh', str(ROOT / 'deploy/cli/install.sh'),
                                 '--bin-dir', str(self.bin), *args],
                                env=self.env, capture_output=True, text=True)
        self.assertEqual(list(self.bin.glob('.gscli-install.*')), [])
        return result

    def test_pinned_install_and_reinstall(self):
        for _ in range(2):
            result = self.run_installer('--version', '1.2.3')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(subprocess.check_output([self.destination, '--version'], text=True).strip(), 'gscli 1.2.3')
        self.assertEqual(len(self.log.read_text().splitlines()), 4)

    def test_all_platform_asset_names(self):
        for system, arch, target in (
            ('Linux', 'aarch64', 'aarch64-unknown-linux-gnu'),
            ('Darwin', 'x86_64', 'x86_64-apple-darwin'),
            ('Darwin', 'arm64', 'aarch64-apple-darwin'),
        ):
            with self.subTest(target=target):
                self.env.update(TEST_OS=system, TEST_ARCH=arch)
                self.asset = self.assets / f'gscli-{target}'
                self.asset.write_text(self.fixture_binary())
                self.hash_asset()
                result = self.run_installer('--version', '1.2.3')
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_checksum_failure_preserves_existing_binary_without_execution(self):
        marker = self.root / 'executed'
        self.asset.write_text(f'#!/bin/sh\ntouch "{marker}"\n')
        result = self.run_installer('--version', '1.2.3')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(marker.exists())
        self.assertEqual(self.destination.read_text(), 'old binary')

    def test_binary_version_mismatch_preserves_existing_binary(self):
        self.asset.write_text('#!/bin/sh\necho "gscli 9.9.9"\n')
        self.hash_asset()
        self.assertNotEqual(self.run_installer('--version', '1.2.3').returncode, 0)
        self.assertEqual(self.destination.read_text(), 'old binary')

    def test_missing_download_preserves_existing_binary(self):
        self.asset.unlink()
        self.assertNotEqual(self.run_installer('--version', '1.2.3').returncode, 0)
        self.assertEqual(self.destination.read_text(), 'old binary')

    def test_malformed_checksum_is_rejected(self):
        self.asset.with_suffix('.sha256').write_text('bad checksum\n')
        self.assertNotEqual(self.run_installer('--version', '1.2.3').returncode, 0)
        self.assertEqual(self.destination.read_text(), 'old binary')

    def test_unsupported_platform_does_not_download(self):
        self.env['TEST_OS'] = 'Windows'
        self.assertNotEqual(self.run_installer('--version', '1.2.3').returncode, 0)
        self.assertFalse(self.log.exists())

    def test_invalid_or_missing_version_does_not_download(self):
        for args in ((), ('--version',), ('--version', '../bad'), ('--version', '01.2.3'),
                     ('--version', '1.2.3\nbad'), ('--unknown',)):
            with self.subTest(args=args):
                self.assertNotEqual(self.run_installer(*args).returncode, 0)
                self.assertFalse(self.log.exists())

    def test_symlink_and_directory_destinations_are_preserved(self):
        self.destination.unlink()
        self.destination.symlink_to(self.root / 'absent')
        self.assertNotEqual(self.run_installer('--version', '1.2.3').returncode, 0)
        self.assertTrue(self.destination.is_symlink())
        self.destination.unlink()
        self.destination.mkdir()
        self.assertNotEqual(self.run_installer('--version', '1.2.3').returncode, 0)
        self.assertTrue(self.destination.is_dir())
        self.assertFalse(self.log.exists())

    def test_native_binary_install_when_requested(self):
        binary = os.environ.get('GSCLI_TEST_BINARY')
        if not binary:
            self.skipTest('set GSCLI_TEST_BINARY for native release-binary installation smoke')
        data = Path(binary).read_bytes()
        self.asset.write_bytes(data)
        self.hash_asset()
        version = subprocess.check_output([binary, '--version'], text=True).strip().split()[1]
        mock = self.mocks / 'curl'
        mock.write_text(mock.read_text().replace('v1.2.3/', f'v{version}/'))
        result = self.run_installer('--version', version)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.destination.read_bytes(), data)
        receipt = json.loads((self.bin / 'gscli-install-receipt.json').read_text())
        self.assertEqual(receipt['schema_version'], 1)
        self.assertEqual(receipt['managed_by'], 'gscli-installer')
        self.assertEqual(receipt['repository'], 'cagojeiger/filegate')
        self.assertEqual(Path(receipt['install_path']), self.destination.resolve())
        self.assertNotIn('installed_version', receipt)
