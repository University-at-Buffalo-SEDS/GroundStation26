import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import build


class FrontendSourceSnapshotTests(unittest.TestCase):
    def repository(self, root):
        repo = root / 'remote'
        repo.mkdir()
        def git(*args):
            subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
        git('init', '--initial-branch=main')
        git('config', 'user.name', 'Synthetic fixture')
        git('config', 'user.email', 'fixture@example.invalid')
        git('config', 'commit.gpgsign', 'false')
        (repo / 'station_ui').mkdir()
        (repo / 'station_ui/page.html').write_text('main page')
        (repo / 'station_ui/manifest.json').write_text(json.dumps({'version': 1, 'exports': [
            {'name': 'PAGE_HTML', 'path': 'station_ui/page.html', 'kind': 'text'}]}))
        git('add', '.')
        git('commit', '-m', 'main fixture')
        git('switch', '-c', 'dev')
        (repo / 'station_ui/page.html').write_text('dev page')
        git('commit', '-am', 'dev fixture')
        git('switch', 'main')
        return repo

    def test_one_build_keeps_its_source_when_another_selects_a_different_branch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = self.repository(root)
            cache = root / 'frontend-cache'
            with patch.object(build, 'FRONTEND_REPO_URL', str(repo)), patch.object(build, '_frontend_checkout_dir', return_value=cache):
                with patch.dict(os.environ, {}, clear=True):
                    first = build.prepare_browser_asset_source(root, False)
                    digest = build._browser_asset_digest(first)
                with patch.dict(os.environ, {}, clear=True):
                    second = build.prepare_browser_asset_source(root, True)
                with patch.dict(os.environ, {}, clear=True):
                    reused = build.prepare_browser_asset_source(root, False)
            self.assertNotEqual(first, second)
            self.assertEqual(reused, first)
            self.assertEqual((first / 'station_ui/page.html').read_text(), 'main page')
            self.assertEqual((second / 'station_ui/page.html').read_text(), 'dev page')
            self.assertEqual(build._browser_asset_digest(first), digest)
            self.assertEqual(build._frontend_shared_target(first), build._frontend_shared_target(second))

    def test_changed_snapshot_is_not_silently_reused_or_overwritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = self.repository(root)
            cache = root / 'frontend-cache'
            with patch.object(build, 'FRONTEND_REPO_URL', str(repo)):
                source = build._immutable_frontend_checkout(cache, 'main')
                page = source / 'station_ui/page.html'
                page.write_text('local modification')
                with self.assertRaisesRegex(RuntimeError, 'has changed'):
                    build._immutable_frontend_checkout(cache, 'main')
                self.assertEqual(page.read_text(), 'local modification')

    def test_failed_clone_does_not_publish_an_incomplete_revision(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = self.repository(root)
            cache = root / 'frontend-cache'
            original_run = build.run
            def interrupted(args, **kwargs):
                if '--no-hardlinks' in args:
                    Path(args[-1]).mkdir()
                    (Path(args[-1]) / 'partial.txt').write_text('incomplete clone')
                    raise subprocess.CalledProcessError(1, args)
                return original_run(args, **kwargs)
            with patch.object(build, 'FRONTEND_REPO_URL', str(repo)):
                with patch.object(build, 'run', side_effect=interrupted):
                    with self.assertRaises(subprocess.CalledProcessError):
                        build._immutable_frontend_checkout(cache, 'main')
                revisions = cache.with_name(cache.name + '-revisions')
                self.assertEqual(list(revisions.iterdir()), [])
                recovered = build._immutable_frontend_checkout(cache, 'main')
                self.assertEqual((recovered / 'station_ui/page.html').read_text(), 'main page')

    def test_simultaneous_builders_select_independent_real_git_revisions(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = self.repository(root)
            env = {**os.environ, 'PYTHONPATH': str(Path(build.__file__).parent)}
            env.pop(build.BROWSER_ASSET_SOURCE_ENV, None)
            code = """import sys,json
from pathlib import Path
import build
root=Path(sys.argv[1]);build.FRONTEND_REPO_URL=str(root/'remote')
build._frontend_checkout_dir=lambda:root/'frontend-cache'
source=build.prepare_browser_asset_source(root,sys.argv[2]=='dev')
(root/(sys.argv[2]+'.json')).write_text(json.dumps({'source':str(source)}))
"""
            children = []
            try:
                for name in ['main', 'dev']:
                    children.append(subprocess.Popen([sys.executable, '-c', code, str(root), name], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True))
                for child in children:
                    output, _ = child.communicate(timeout=30)
                    self.assertEqual(child.returncode, 0, output)
                selected = [Path(json.loads((root / (name + '.json')).read_text())['source']) for name in ['main', 'dev']]
                self.assertNotEqual(*selected)
                self.assertEqual([(source / 'station_ui/page.html').read_text() for source in selected], ['main page', 'dev page'])
            finally:
                for child in children:
                    if child.poll() is None:
                        child.kill()
                        child.wait()

    def test_compiler_cache_is_shared_without_changing_parent_environment(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root = Path(tmp)
            checkout = root / 'source-revisions' / ('a' * 40)
            checkout.mkdir(parents=True)
            script = checkout / 'build.py'
            output = checkout / 'selected-target.txt'
            script.write_text('import os\nfrom pathlib import Path\nPath("selected-target.txt").write_text(os.environ["CARGO_TARGET_DIR"])\n')
            build._run_frontend_script_with_wasm_opt_fallback(script, checkout, [])
            self.assertEqual(output.read_text(), str(checkout.parent / 'target'))
            self.assertNotIn('CARGO_TARGET_DIR', os.environ)
            os.environ['CARGO_TARGET_DIR'] = str(root / 'explicit-target')
            build._run_frontend_script_with_wasm_opt_fallback(script, checkout, [])
            self.assertEqual(output.read_text(), str(root / 'explicit-target'))

    def test_lock_serializes_independent_processes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            env = {**os.environ, 'PYTHONPATH': str(Path(build.__file__).parent)}
            code = '''import time, sys
from pathlib import Path
from build import _frontend_cache_lock
root=Path(sys.argv[1]);name=sys.argv[2]
(root/(name+"-ready")).touch()
with _frontend_cache_lock(root/"cache.lock"):
 with (root/"trace").open("a") as f:f.write(name+"-enter\\n")
 if name=="a":
  while not (root/"release").exists():time.sleep(.01)
 with (root/"trace").open("a") as f:f.write(name+"-exit\\n")
'''
            children = []
            def wait_for(predicate):
                end = time.monotonic() + 5
                while not predicate():
                    self.assertLess(time.monotonic(), end, 'fixture child timed out')
                    time.sleep(.01)
            try:
                a = subprocess.Popen([sys.executable, '-c', code, str(root), 'a'], env=env)
                children.append(a)
                wait_for(lambda: (root / 'trace').exists())
                b = subprocess.Popen([sys.executable, '-c', code, str(root), 'b'], env=env)
                children.append(b)
                wait_for(lambda: (root / 'b-ready').exists())
                self.assertEqual((root / 'trace').read_text().splitlines(), ['a-enter'])
                (root / 'release').touch()
                for child in children:
                    self.assertEqual(child.wait(timeout=5), 0)
                self.assertEqual((root / 'trace').read_text().splitlines(), ['a-enter', 'a-exit', 'b-enter', 'b-exit'])
            finally:
                for child in children:
                    if child.poll() is None:
                        child.kill()
                        child.wait()
