"""Actual Cargo shared-target regressions; no UI tools or network required."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import signal
import unittest
from unittest.mock import patch
import build


class TargetProvenanceTests(unittest.TestCase):
    def source(self, root, marker, vendor=True):
        source = root / (marker * 40)
        (source / 'src').mkdir(parents=True)
        (source / 'station_ui').mkdir()
        (source / 'station_ui/manifest.json').write_text('{"version": 1, "exports": [{"name":"PAGE_HTML","path":"station_ui/page","kind":"text"}]}')
        (source / "station_ui/page").write_text(marker)
        (source / '.gitignore').write_text('target/\ndist/\nCargo.lock\n')
        manifest = '[package]\nname="renamed_app"\nversion="0.1.0"\nedition="2021"\n[[bin]]\nname="custom_executable"\npath="src/main.rs"\n'
        if vendor:
            manifest += '[dependencies]\nlocal_dependency={path="vendor"}\n'
            (source / 'vendor/src').mkdir(parents=True)
            (source / 'vendor/Cargo.toml').write_text('[package]\nname="local_dependency"\nversion="0.1.0"\nedition="2021"\n')
            (source / 'vendor/src/lib.rs').write_text('pub fn value()->&\'static str {"' + marker + '"}\n')
        stable = root / 'stable'
        if not stable.exists():
            (stable / 'src').mkdir(parents=True)
            (stable / 'Cargo.toml').write_text('[package]\nname="stable_external"\nversion="0.1.0"\nedition="2021"\n')
            (stable / 'src/lib.rs').write_text('pub fn value()->u32 {42}\n')
        if '[dependencies]' not in manifest:
            manifest += '[dependencies]\n'
        manifest += 'stable_external={path="../stable"}\n'
        (source / 'build.rs').write_text('fn main(){println!("cargo:rustc-env=BUILD_MARKER=' + marker + '");}\n')
        (source / 'Cargo.toml').write_text(manifest)
        expr = 'local_dependency::value()' if vendor else '"' + marker + '"'
        (source / 'src/main.rs').write_text('fn main(){assert_eq!(stable_external::value(),42);println!("{}|BUILD={}|{}",env!("CARGO_MANIFEST_DIR"),env!("BUILD_MARKER"),' + expr + ');}\n')
        (source / 'build.py').write_text('''import os, subprocess, time, sys
from pathlib import Path
if 'existing' in sys.argv:
 if os.environ.get('FIXTURE_FALLBACK'): print('wasm-opt failed');raise SystemExit(1)
 raise SystemExit(0)
if os.environ.get('FIXTURE_FAIL'): raise SystemExit(7)
if os.environ.get('FIXTURE_WAIT'):
 Path(os.environ['FIXTURE_WAIT']).touch()
 while not os.environ.get('FIXTURE_RELEASE') or not Path(os.environ['FIXTURE_RELEASE']).exists(): time.sleep(.01)
subprocess.run(['cargo','build','--offline','--jobs','2'],check=True)
p=Path('dist/public');p.mkdir(parents=True,exist_ok=True)
p.joinpath('index.html').write_text(subprocess.check_output([str(Path(os.environ['CARGO_TARGET_DIR'])/'debug/custom_executable')],text=True))
if os.environ.get('FIXTURE_FALLBACK'):
 print('wasm-opt failed');raise SystemExit(1)
''')
        subprocess.run(['git', 'init', '-q', str(source)], check=True)
        subprocess.run(['git', '-C', str(source), 'add', '.'], check=True)
        return source

    def invoke(self, root, source):
        build._run_frontend_build(root, False, False, False, None, False, source)
        return (root / 'frontend/dist/public/index.html').read_text().strip()

    def test_real_a_b_reused_a_vendor_and_custom_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); target = root / 'shared'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                a = self.source(root, 'a'); self.assertTrue(self.invoke(root, a).endswith('|BUILD=a|a'))
                stable_before = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in target.glob('debug/deps/libstable_external*')}
                b = self.source(root, 'b'); self.assertTrue(self.invoke(root, b).endswith('|BUILD=b|b'))
                before = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in build._frontend_source_files(a)}
                dx = target / 'dx/renamed_app'; dx.mkdir(parents=True); (dx / 'stale').touch()
                unrelated = target / 'dx/unrelated'; unrelated.mkdir(); (unrelated / 'keep').touch()
                self.assertTrue(self.invoke(root, a).endswith('|BUILD=a|a'))
                self.assertFalse(dx.exists()); self.assertTrue((unrelated / 'keep').exists())
                self.assertTrue(stable_before)
                self.assertEqual(stable_before, {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in stable_before})
                self.assertEqual(before, {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in before})
                with patch.object(build, '_clean_frontend_package', wraps=build._clean_frontend_package) as clean:
                    self.assertTrue(self.invoke(root, a).endswith('|BUILD=a|a')); clean.assert_not_called()

    def test_failure_sync_failure_and_fallback_commit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a', False); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                self.invoke(root, source)
                stamp = target / '.gs-frontend-source.json'; self.assertTrue(stamp.exists())
                with patch.dict(os.environ, {'FIXTURE_FAIL': '1'}), self.assertRaises(subprocess.CalledProcessError):
                    self.invoke(root, source)
                self.assertFalse(stamp.exists())
                with patch.object(build, '_sync_frontend_public_assets', side_effect=OSError('sync failed')), self.assertRaises(OSError):
                    self.invoke(root, source)
                self.assertFalse(stamp.exists())
                with patch.dict(os.environ, {'FIXTURE_FALLBACK': '1'}):
                    self.assertTrue(self.invoke(root, source).endswith('|a'))
                self.assertTrue(stamp.exists())

    def test_relative_target_and_removed_vendor(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); a = self.source(root, 'a'); b = self.source(root, 'b', False)
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': '../shared', 'CARGO_INCREMENTAL': '0'}):
                self.invoke(root, a)
                with patch.object(build, '_clean_frontend_package', wraps=build._clean_frontend_package) as clean:
                    self.invoke(root, b)
                self.assertIn('local_dependency', [c.args[1]['name'] for c in clean.call_args_list])
                self.assertTrue((root / 'shared/.gs-frontend-source.json').exists())

    def test_concurrent_builders_and_cancellation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); a = self.source(root, 'a'); b = self.source(root, 'b'); target = root / 'target'
            env = {**os.environ, 'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0', 'PYTHONPATH': str(Path(build.__file__).parent)}
            code = 'import build,sys;from pathlib import Path;s=Path(sys.argv[1]);build._run_frontend_script_with_wasm_opt_fallback(s/"build.py",s,[])'
            ready = root / 'ready'
            first = subprocess.Popen([sys.executable, '-c', code, str(a)], env={**env, 'FIXTURE_WAIT': str(ready)}, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            second = None
            try:
                deadline = time.monotonic() + 10
                while not ready.exists():
                    self.assertLess(time.monotonic(), deadline); time.sleep(.01)
                second = subprocess.Popen([sys.executable, '-c', code, str(b)], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                time.sleep(.15); self.assertIsNone(second.poll())
                self.assertFalse((target / '.gs-frontend-source.json').exists())
                first.send_signal(signal.SIGINT); first.wait(timeout=8)
                output, _ = second.communicate(timeout=15); self.assertEqual(second.returncode, 0, output)
                receipt = json.loads((target / '.gs-frontend-source.json').read_text()); self.assertEqual(receipt['source'], str(b))
                self.assertTrue((b / 'dist/public/index.html').read_text().strip().endswith('|b'))
            finally:
                for child in (first, second):
                    if child is not None and child.poll() is None:
                        child.kill(); child.wait()

    def successful_builders(self, distinct_targets=False, distinct_destinations=False, same_source=False):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); a = self.source(root, 'a'); b = a if same_source else self.source(root, 'b')
            expected = 'a' if same_source else 'b'
            target = root / 'target'; second_target = root / 'second-target' if distinct_targets else target
            first_destination = root / 'first-destination' if distinct_destinations else root
            second_destination = root / 'second-destination' if distinct_destinations else root
            ready = root / 'ready'; release = root / 'release'
            env = {**os.environ, 'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0',
                   'PYTHONPATH': str(Path(build.__file__).parent)}
            code = 'import build,sys;from pathlib import Path;build._run_frontend_build(Path(sys.argv[1]),False,False,False,None,False,Path(sys.argv[2]))'
            first = subprocess.Popen([sys.executable, '-c', code, str(first_destination), str(a)],
                                     env={**env, 'FIXTURE_WAIT': str(ready), 'FIXTURE_RELEASE': str(release)},
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            second = None
            try:
                deadline = time.monotonic() + 10
                while not ready.exists():
                    self.assertLess(time.monotonic(), deadline); time.sleep(.01)
                second = subprocess.Popen([sys.executable, '-c', code, str(second_destination), str(b)], env={**env, 'CARGO_TARGET_DIR': str(second_target)},
                                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                time.sleep(.15); self.assertIsNone(second.poll())
                if distinct_targets:
                    self.assertFalse(second_target.exists())
                release.touch()
                for child in (first, second):
                    output, _ = child.communicate(timeout=15); self.assertEqual(child.returncode, 0, output)
                self.assertTrue((second_destination / 'frontend/dist/public/index.html').read_text().strip().endswith('|BUILD=' + expected + '|' + expected))
                receipt = json.loads((second_target / '.gs-frontend-source.json').read_text())
                self.assertEqual(receipt['source'], str(b))
                if distinct_destinations:
                    self.assertTrue((first_destination / 'frontend/dist/public/index.html').read_text().strip().endswith('|BUILD=a|a'))
            finally:
                for child in (first, second):
                    if child is not None and child.poll() is None:
                        child.kill(); child.wait()

    def test_two_successful_builders_serialize_build_and_sync(self):
        self.successful_builders()

    def test_different_explicit_targets_serialize_same_destination(self):
        self.successful_builders(distinct_targets=True)

    def test_different_destinations_serialize_shared_target(self):
        self.successful_builders(distinct_destinations=True)

    def test_renamed_application_and_stale_dx_are_cleaned(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); a = self.source(root, 'a'); b = self.source(root, 'b')
            (b / 'Cargo.toml').write_text((b / 'Cargo.toml').read_text().replace('name="renamed_app"', 'name="successor_app"'))
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(root / 'target'), 'CARGO_INCREMENTAL': '0'}):
                self.invoke(root, a)
                cache = root / 'target/dx/renamed_app'; cache.mkdir(parents=True)
                (cache / 'stale').touch()
                with patch.object(build, '_clean_frontend_package', wraps=build._clean_frontend_package) as clean:
                    self.invoke(root, b)
                self.assertEqual({c.args[1]['name'] for c in clean.call_args_list}, {'renamed_app', 'successor_app', 'local_dependency'})
                self.assertFalse(cache.exists())

    def test_descriptor_and_external_manifest_bounds(self):
        with self.assertRaises(RuntimeError):
            build._validate_frontend_package({'name': '../escape', 'version': '1', 'targets': []})
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a')
            external = root / 'outside'; external.mkdir()
            (external / 'Cargo.toml').write_text('not a valid manifest')
            (source / 'external').symlink_to(external, target_is_directory=True)
            subprocess.run(['git', '-C', str(source), 'add', 'external'], check=True)
            self.assertNotIn(source / 'external', build._frontend_source_files(source))
            manifest = source / 'Cargo.toml'
            with patch.object(build.subprocess, 'run', wraps=build.subprocess.run) as read:
                package = build._frontend_package(manifest)
            self.assertEqual(package['name'], 'renamed_app')
            self.assertIn('--quiet', read.call_args.args[0])
            self.assertFalse((source / 'Cargo.lock').exists())

    @unittest.skipUnless(os.name == "posix", "Actual POSIX process-group control")
    def test_cancellation_stops_writer_after_parent_already_exited(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a', False)
            writer = root / 'writer.py'; ticks = root / 'ticks'; pidfile = root / 'pid'
            writer.write_text('import os,signal,time,sys;from pathlib import Path\n'
                              'signal.signal(signal.SIGTERM,signal.SIG_IGN)\n'
                              'Path(sys.argv[2]).write_text(str(os.getpid()))\n'
                              'while True:\n Path(sys.argv[1]).write_text(str(time.time_ns()));time.sleep(.02)\n')
            (source / 'build.py').write_text('import subprocess,sys\n'
                + 'subprocess.Popen([sys.executable,' + repr(str(writer)) + ',' + repr(str(ticks)) + ',' + repr(str(pidfile)) + '])\n')
            env = {**os.environ, 'CARGO_TARGET_DIR': str(root / 'target'),
                   'PYTHONPATH': str(Path(build.__file__).parent)}
            code = 'import build,sys;from pathlib import Path;s=Path(sys.argv[1]);build._run_frontend_script_with_wasm_opt_fallback(s/"build.py",s,[])'
            wrapper = subprocess.Popen([sys.executable, '-c', code, str(source)], env=env,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            writer_pid = None
            try:
                deadline = time.monotonic() + 10
                while not pidfile.exists() or not ticks.exists():
                    self.assertLess(time.monotonic(), deadline); time.sleep(.01)
                writer_pid = int(pidfile.read_text())
                time.sleep(.05)  # frontend leader exits; descendant retains stdout
                wrapper.send_signal(signal.SIGINT); wrapper.wait(timeout=8)
                before = ticks.read_bytes(); time.sleep(.1); self.assertEqual(before, ticks.read_bytes())
                self.assertFalse((root / 'target/.gs-frontend-source.json').exists())
                # Lock is usable only after the whole owned writer group stopped.
                with build._frontend_cache_lock(root / '.target.gs-frontend-build.lock'):
                    self.assertEqual(before, ticks.read_bytes())
            finally:
                if wrapper.poll() is None:
                    wrapper.kill(); wrapper.wait()
                if writer_pid is not None:
                    try: os.kill(writer_pid, signal.SIGKILL)
                    except ProcessLookupError: pass

    @unittest.skipUnless(os.name == 'posix', 'Actual POSIX abrupt-parent control')
    def test_uncatchable_parent_death_blocks_target_without_killing_stale_pid(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a', False)
            script = source / 'build.py'
            script.write_text(script.read_text().replace("Path(os.environ['FIXTURE_WAIT']).touch()", "Path(os.environ['FIXTURE_WAIT']).write_text(str(os.getpid()))"))
            ready = root / 'ready'; target = root / 'target'
            env = {**os.environ, 'CARGO_TARGET_DIR': str(target), 'FIXTURE_WAIT': str(ready),
                   'PYTHONPATH': str(Path(build.__file__).parent)}
            code = 'import build,sys;from pathlib import Path;s=Path(sys.argv[1]);build._run_frontend_script_with_wasm_opt_fallback(s/"build.py",s,[])'
            wrapper = subprocess.Popen([sys.executable, '-c', code, str(source)], env=env,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            group = None
            try:
                deadline = time.monotonic() + 10
                while not ready.exists() or not ready.read_text():
                    self.assertLess(time.monotonic(), deadline); time.sleep(.01)
                group = int(ready.read_text()); wrapper.kill(); wrapper.wait(timeout=5)
                marker = root / '.target.gs-frontend-in-progress.json'
                self.assertTrue(marker.exists())
                with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target)}), patch.object(build, '_clean_frontend_package') as clean:
                    with self.assertRaisesRegex(RuntimeError, 'interrupted/uncertain'):
                        self.invoke(root, source)
                    clean.assert_not_called()
                os.kill(group, 0)  # marker rejection did not kill a recorded/stale PID
                self.assertTrue(marker.exists())
            finally:
                if wrapper.poll() is None:
                    wrapper.kill(); wrapper.wait()
                if group is not None:
                    try: os.killpg(group, signal.SIGKILL)
                    except ProcessLookupError: pass

    def test_same_checkout_old_mtime_source_change_invalidates_local_library(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                self.invoke(root, source)
                library = source / 'vendor/src/lib.rs'; original = library.stat()
                library.write_text(library.read_text().replace('"a"', '"changed"'))
                os.utime(library, ns=(original.st_atime_ns, original.st_mtime_ns))
                self.assertTrue(self.invoke(root, source).endswith('|BUILD=a|changed'))

    def test_provenance_replace_failure_leaves_no_stamp_or_temporary_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                self.invoke(root, source)
                real_replace = Path.replace
                def fail_receipt(path, destination):
                    if Path(destination).name == '.gs-frontend-source.json':
                        raise OSError('injected atomic provenance failure')
                    return real_replace(path, destination)
                with patch.object(Path, 'replace', fail_receipt), self.assertRaisesRegex(OSError, 'provenance failure'):
                    self.invoke(root, source)
                self.assertFalse((target / '.gs-frontend-source.json').exists())
                self.assertEqual(list(target.glob('.gs-source-*')), [])
                self.assertFalse((root / '.target.gs-frontend-in-progress.json').exists())
                self.assertTrue(self.invoke(root, source).endswith('|BUILD=a|a'))

    def test_normal_revision_layout_a_b_cached_a_without_target_override(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ):
            root = Path(temporary); revisions = root / 'source-revisions'; revisions.mkdir()
            os.environ.pop('CARGO_TARGET_DIR', None); os.environ['CARGO_INCREMENTAL'] = '0'
            a = self.source(revisions, 'a'); self.assertTrue(self.invoke(root, a).endswith('|BUILD=a|a'))
            b = self.source(revisions, 'b'); self.assertTrue(self.invoke(root, b).endswith('|BUILD=b|b'))
            self.assertTrue(self.invoke(root, a).endswith('|BUILD=a|a'))
            receipt = json.loads((revisions / 'target/.gs-frontend-source.json').read_text())
            self.assertEqual(receipt['source'], str(a))
            self.assertEqual(json.loads((root / 'frontend/dist/station-source.json').read_text())['source_dir'], str(a))

    def test_source_change_early_hint_failure_restores_prior_dist_without_stamping(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                old = self.invoke(root, source)
                destination = root / 'frontend/dist/public/index.html'; old_receipt = (root / 'frontend/dist/station-source.json').read_bytes()
                nested = source / 'frontend/dist/public'; nested.mkdir(parents=True); (nested / 'prior').write_text('nested old')
                library = source / 'vendor/src/lib.rs'; library.write_text(library.read_text().replace('"a"', '"changed"'))
                (source / 'build.py').write_text('print("wasm-opt failed");raise SystemExit(1)\n')
                with self.assertRaises(FileNotFoundError):
                    self.invoke(root, source)
                self.assertEqual(destination.read_text().strip(), old)
                self.assertEqual((root / 'frontend/dist/station-source.json').read_bytes(), old_receipt)
                self.assertEqual((source / 'dist/public/index.html').read_text().strip(), old)
                self.assertEqual((nested / 'prior').read_text(), 'nested old')
                self.assertFalse((target / '.gs-frontend-source.json').exists())
                self.assertEqual(list(source.rglob('.gs-public-prior-*')), [])

    def test_fresh_fallback_replaces_prior_dist_and_failed_partial_is_not_reusable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                old = self.invoke(root, source)
                original_script = (source / 'build.py').read_text()
                (source / 'build.py').write_text('from pathlib import Path\np=Path("dist/public");p.mkdir(parents=True);(p/"index.html").write_text("partial")\nraise SystemExit(2)\n')
                with self.assertRaises(subprocess.CalledProcessError): self.invoke(root, source)
                self.assertEqual((source / 'dist/public/index.html').read_text().strip(), old)
                (source / 'build.py').write_text(original_script)
                library = source / 'vendor/src/lib.rs'; library.write_text(library.read_text().replace('"a"', '"fresh"'))
                with patch.dict(os.environ, {'FIXTURE_FALLBACK': '1'}):
                    self.assertTrue(self.invoke(root, source).endswith('|BUILD=a|fresh'))
                self.assertEqual(list(source.rglob('.gs-public-prior-*')), [])

    def test_ignored_lockfile_explicit_provenance_changes_clean_once_then_warm(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                self.invoke(root, source)
                receipt = json.loads((target / '.gs-frontend-source.json').read_text())
                self.assertIsNone(receipt['lock_before_build']['Cargo.lock'])
                self.assertIsNotNone(receipt['dependency_locks']['Cargo.lock'])
                with patch.object(build, '_clean_frontend_package', wraps=build._clean_frontend_package) as clean:
                    self.invoke(root, source); clean.assert_not_called()
                lock = source / 'Cargo.lock'; lock.write_text(lock.read_text() + '\n# deliberate ignored-lock provenance change\n')
                with patch.object(build, '_clean_frontend_package', wraps=build._clean_frontend_package) as clean:
                    self.invoke(root, source); self.assertGreater(clean.call_count, 0)
                updated = json.loads((target / '.gs-frontend-source.json').read_text())
                self.assertEqual(receipt['digest'], updated['digest'])  # ignored lock is explicit, not invented source coverage
                self.assertNotEqual(receipt['dependency_locks'], updated['lock_before_build'])
                self.assertEqual(receipt['dependency_locks'], updated['dependency_locks'])  # actual Cargo normalizes the added comment
                with patch.object(build, '_clean_frontend_package', wraps=build._clean_frontend_package) as clean:
                    self.invoke(root, source); clean.assert_not_called()

    def test_existing_requires_matching_provenance_and_never_accepts_hint_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                self.invoke(root, source)
                with patch.dict(os.environ, {'FIXTURE_FALLBACK': '1'}), self.assertRaises(subprocess.CalledProcessError):
                    build._run_frontend_script_with_wasm_opt_fallback(source / 'build.py', source, ['existing'])
                with self.assertRaisesRegex(RuntimeError, 'no matching source/dependency provenance'):
                    build._run_frontend_script_with_wasm_opt_fallback(source / 'build.py', source, ['existing'])

    def test_matching_existing_reuse_preserves_proven_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                old = self.invoke(root, source)
                with patch.object(build, '_clean_frontend_package', wraps=build._clean_frontend_package) as clean:
                    build._run_frontend_build(root, False, False, True, None, False, source)
                    clean.assert_not_called()
                self.assertEqual((root / 'frontend/dist/public/index.html').read_text().strip(), old)
                self.assertEqual(list(source.rglob('.gs-public-prior-*')), [])
                library = source / 'vendor/src/lib.rs'; library.write_text(library.read_text().replace('"a"', '"changed"'))
                with self.assertRaisesRegex(RuntimeError, 'no matching source/dependency provenance'):
                    build._run_frontend_build(root, False, False, True, None, False, source)
                self.assertEqual((root / 'frontend/dist/public/index.html').read_text().strip(), old)

    def test_public_validation_and_uncertain_writer_never_delete_prior_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = root / 'source'; public = source / 'dist/public'
            public.mkdir(parents=True); (public / 'index').write_text('prior')
            for reuse in (False, True):
                with self.assertRaises(RuntimeError):
                    with build._frontend_public_attempt(source, reuse, {'stopped': True}, public):
                        self.fail('aliased destination must reject before staging')
            self.assertEqual((public / 'index').read_text(), 'prior')
            with self.assertRaisesRegex(RuntimeError, 'uncertain'):
                with build._frontend_public_attempt(source, False, {'stopped': False}):
                    public.mkdir(); (public / 'new').write_text('possible live writer')
                    raise RuntimeError('uncertain writer')
            self.assertEqual((public / 'new').read_text(), 'possible live writer')
            backups = list(source.rglob('.gs-public-prior-*'))
            self.assertEqual(len(backups), 1)
            self.assertEqual((backups[0] / 'public/index').read_text(), 'prior')

    @unittest.skipUnless(os.name == 'posix', 'Actual POSIX quarantine cancellation')
    def test_cancel_restores_quarantined_prior_without_changing_gs_destination(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = self.source(root, 'a'); target = root / 'target'
            with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0'}):
                old = self.invoke(root, source)
            ready = root / 'ready'
            env = {**os.environ, 'CARGO_TARGET_DIR': str(target), 'CARGO_INCREMENTAL': '0',
                   'FIXTURE_WAIT': str(ready), 'PYTHONPATH': str(Path(build.__file__).parent)}
            code = 'import build,sys;from pathlib import Path;s=Path(sys.argv[1]);build._run_frontend_script_with_wasm_opt_fallback(s/"build.py",s,[])'
            wrapper = subprocess.Popen([sys.executable, '-c', code, str(source)], env=env,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 10
                while not ready.exists():
                    self.assertLess(time.monotonic(), deadline); time.sleep(.01)
                self.assertFalse((source / 'dist/public').exists())
                self.assertEqual(len(list(source.rglob('.gs-public-prior-*'))), 1)
                wrapper.send_signal(signal.SIGINT); wrapper.wait(timeout=8)
                self.assertEqual((source / 'dist/public/index.html').read_text().strip(), old)
                self.assertEqual((root / 'frontend/dist/public/index.html').read_text().strip(), old)
                self.assertFalse((target / '.gs-frontend-source.json').exists())
                self.assertEqual(list(source.rglob('.gs-public-prior-*')), [])
            finally:
                if wrapper.poll() is None:
                    wrapper.kill(); wrapper.wait()

    def test_same_checkout_different_targets_and_destinations_serialize_output(self):
        self.successful_builders(distinct_targets=True, distinct_destinations=True, same_source=True)
