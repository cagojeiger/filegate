import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('manifest', ROOT / 'deploy/cli/manifest.py')
manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manifest)


class ManifestTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        for target in manifest.TARGETS:
            asset = self.root / f'gscli-{target}'
            asset.write_bytes(target.encode())
            asset.with_suffix('.sha256').write_text(hashlib.sha256(asset.read_bytes()).hexdigest() + '\n')

    def test_complete_manifest(self):
        result = manifest.build_manifest(self.root, '1.2.3')
        self.assertEqual(result['schema_version'], 1)
        self.assertEqual(result['version'], '1.2.3')
        self.assertEqual(len(result['assets']), 4)
        for target, asset in result['assets'].items():
            self.assertEqual(asset['size'], len(target))
            self.assertEqual(asset['name'], f'gscli-{target}')
            self.assertEqual(asset['sha256'], hashlib.sha256(target.encode()).hexdigest())

    def test_missing_target_fails(self):
        (self.root / f'gscli-{manifest.TARGETS[0]}').unlink()
        with self.assertRaises(FileNotFoundError):
            manifest.build_manifest(self.root, '1.2.3')

    def test_corrupt_target_fails(self):
        (self.root / f'gscli-{manifest.TARGETS[0]}').write_bytes(b'corrupted')
        with self.assertRaises(ValueError):
            manifest.build_manifest(self.root, '1.2.3')

    def test_invalid_version_fails(self):
        with self.assertRaises(ValueError):
            manifest.build_manifest(self.root, 'latest')
