"""Text/files-only integration contracts. Never run a native service or namespace."""
import ast
import json
from pathlib import Path
import tempfile
import unittest
import extract
import provenance


class IntegrationTests(unittest.TestCase):
    def test_committed_extraction_and_dependency_identity(self):
        extract.verify()
        self.assertIn('src/terminal/url.rs', provenance.record()['files'])

    def test_mutated_production_and_extracted_bytes_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            root = repo/'scripts/link-recovery'
            for source in ['src/terminal/url.rs', 'src/terminal/url_recovery.rs']:
                (repo/source).parent.mkdir(parents=True, exist_ok=True)
                (repo/source).write_bytes((extract.REPO/source).read_bytes())
            for name, data in extract.outputs(repo).items():
                (root/name).parent.mkdir(parents=True, exist_ok=True)
                (root/name).write_bytes(data)
            (root/'extraction.json').write_text(json.dumps(extract.identities(root, repo)))
            extract.verify(root, repo)
            generated = root/'src/extracted.rs'
            generated.write_bytes(generated.read_bytes()+b'\n')
            with self.assertRaisesRegex(RuntimeError, 'stale production'):
                extract.verify(root, repo)
            generated.write_bytes(extract.outputs(repo)['src/extracted.rs'])
            source = repo/'src/terminal/url.rs'
            source.write_bytes(source.read_bytes()+b'\n')
            with self.assertRaisesRegex(RuntimeError, 'identity mismatch'):
                extract.verify(root, repo)

    def test_wrapper_is_fail_closed_without_inherited_environment(self):
        text = (extract.ROOT/'run-hosted.sh').read_text()
        self.assertIn('/usr/bin/env -i PATH=/usr/bin:/bin LANG=C.UTF-8', text)
        self.assertIn('/usr/bin/unshare --user --map-root-user --net', text)
        self.assertNotIn('sudo', text)
        self.assertNotIn('||', text.split('shift', 1)[1])
        self.assertNotIn('PYTHONOPTIMIZE', text)

    def test_failure_artifacts_follow_case_cleanup(self):
        tree = ast.parse((extract.ROOT/'run_suite.py').read_text())
        run_case = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'run_case')
        self.assertTrue(any(isinstance(n, ast.Try) and any(
            isinstance(c, ast.Call) and isinstance(c.func, ast.Name) and c.func.id == 'stop_children'
            for item in n.finalbody for c in ast.walk(item)) for n in ast.walk(run_case)))
        text = (extract.ROOT/'run_suite.py').read_text()
        self.assertIn("status['completed_cases'].append(case)", text)
        self.assertIn("status['failure_type'] = type(error).__name__", text)
        self.assertNotIn('str(error)', text)
        self.assertIn('signal.signal(signal.SIGTERM, interrupted)', text)

    def test_workflow_pins_actions_and_uploads_failure_artifacts(self):
        text = (extract.REPO/'.github/workflows/link-recovery.yml').read_text()
        self.assertIn('cancel-in-progress: false', text)
        self.assertIn('timeout-minutes: 20', text)
        self.assertIn('persist-credentials: false', text)
        self.assertIn('if: ${{ always() }}', text)
        self.assertIn('--offline --locked', text)
        self.assertNotIn('pull_request_target', text)
        self.assertNotIn('continue-on-error', text)
        import re
        actions = re.findall(r'uses: (\S+)', text)
        self.assertEqual(len(actions), 2)
        self.assertTrue(all(re.fullmatch(r'actions/[a-z-]+@[0-9a-f]{40}', a) for a in actions))


if __name__ == '__main__':
    unittest.main()
