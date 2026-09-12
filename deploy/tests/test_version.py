import importlib.util
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('check_version', ROOT / 'deploy/ci/check-version.py')
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class VersionTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        (self.root / 'cli').mkdir()
        (self.root / 'cli/Cargo.toml').write_text('[package]\nname="gscli"\nversion.workspace=true\n')
        self.set_version('1.2.3')
        shutil.copytree(ROOT / 'deploy/ci', self.root / 'deploy/ci')
        self.git('init', '-q')
        self.git('config', 'user.email', 'test@example.invalid')
        self.git('config', 'user.name', 'Release Test')
        self.commit()

    def set_version(self, version):
        (self.root / 'VERSION').write_text(version + '\n')
        (self.root / 'Cargo.toml').write_text(f'[workspace]\nmembers=["cli"]\n[workspace.package]\nversion="{version}"\n')
        (self.root / 'Cargo.lock').write_text(f'[[package]]\nname="gscli"\nversion="{version}"\n')

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root, text=True).strip()

    def commit(self):
        self.git('add', '.')
        self.git('-c', 'commit.gpgsign=false', 'commit', '-qm', 'test', '--allow-empty')

    def resolve(self, current=None, release=None):
        output = self.root / 'output'
        output.unlink(missing_ok=True)
        sha = self.git('rev-parse', 'HEAD')
        result = subprocess.run(['bash', 'deploy/ci/resolve-release.sh'], cwd=self.root,
                                env=dict(os.environ, RELEASE_SHA=release or sha,
                                         CURRENT_MAIN_SHA=current or sha, GITHUB_OUTPUT=str(output)),
                                capture_output=True, text=True)
        values = dict(line.split('=', 1) for line in output.read_text().splitlines()) if output.exists() else {}
        return result, values

    def test_shared_version(self):
        self.assertEqual(checker.check_version(self.root), '1.2.3')

    def test_workspace_drift(self):
        (self.root / 'VERSION').write_text('1.2.4')
        with self.assertRaises(ValueError):
            checker.check_version(self.root)

    def test_lock_drift(self):
        (self.root / 'Cargo.lock').write_text('[[package]]\nname="gscli"\nversion="1.2.2"\n')
        with self.assertRaises(ValueError):
            checker.check_version(self.root)

    def test_new_unpublished_version(self):
        result, values = self.resolve()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(values, dict(version='1.2.3', tag='v1.2.3', should_release='true'))

    def test_existing_tag_is_immutable(self):
        self.git('tag', 'v1.2.3')
        result, values = self.resolve()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(values['should_release'], 'false')

    def test_ordinary_commit_with_published_version_skips(self):
        self.git('tag', 'v1.2.3')
        self.commit()
        result, values = self.resolve()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(values['should_release'], 'false')

    def test_unpublished_version_recovered_by_later_commit(self):
        self.commit()
        result, values = self.resolve()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(values['should_release'], 'true')

    def test_stale_commit_skips(self):
        result, values = self.resolve(current='0' * 40)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(values['should_release'], 'false')

    def test_wrong_checkout_fails(self):
        result, _ = self.resolve(release='0' * 40)
        self.assertNotEqual(result.returncode, 0)

    def test_reusing_an_old_version_fails(self):
        self.git('tag', 'v1.2.3')
        self.set_version('1.2.4')
        self.commit()
        self.set_version('1.2.3')
        self.commit()
        result, _ = self.resolve()
        self.assertNotEqual(result.returncode, 0)

    def test_reusing_an_old_version_fails_after_follow_up_commit(self):
        self.git('tag', 'v1.2.3')
        self.set_version('1.2.4')
        self.commit()
        self.set_version('1.2.3')
        self.commit()
        (self.root / 'README.md').write_text('Follow-up fix\n')
        self.commit()
        result, _ = self.resolve()
        self.assertNotEqual(result.returncode, 0)

    def test_invalid_version_fails(self):
        self.set_version('latest')
        result, _ = self.resolve()
        self.assertNotEqual(result.returncode, 0)
