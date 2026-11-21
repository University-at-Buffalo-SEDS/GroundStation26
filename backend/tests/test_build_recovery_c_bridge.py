"""Dependency routing and environment ownership; C compilation is mocked here."""
import importlib.util
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest import mock


class BridgeDependencySelectionTests(unittest.TestCase):
    def setUp(self):
        script = Path(__file__).with_name('build_recovery_c_bridge.py')
        spec = importlib.util.spec_from_file_location('bridge_tooling_under_test', script)
        self.tool = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.tool)
        override = os.environ.get('GS_RECOVERY_NATIVE_SDK_HELPER')
        helper = Path(override) if override is not None else Path(__file__).resolve().parents[3] / 'ActuatorBoard26/tests/native_sdk.py'
        if not helper.is_file():
            if override is not None:
                self.fail(f'Explicit GS_RECOVERY_NATIVE_SDK_HELPER is not a file: {helper}')
            self.skipTest('Optional sibling ActuatorBoard26 native SDK resolver fixture is unavailable')
        original = {key: os.environ.get(key) for key in self.tool.ENVIRONMENT_KEYS}
        saved_path = sys.path[:]
        saved_modules = {key: sys.modules.get(key) for key in ['native_sdk', 'test_no_telemetry']}
        def restore_python_imports():
            sys.path[:] = saved_path
            for key, value in saved_modules.items():
                if value is None:
                    sys.modules.pop(key, None)
                else:
                    sys.modules[key] = value
        self.addCleanup(restore_python_imports)
        self.addCleanup(self.tool.restore_environment, original)
        self.original = original
        self.tool.restore_environment({key: None for key in original})
        self.tmp = tempfile.TemporaryDirectory(prefix='bridge-dependency-routing-')
        self.addCleanup(self.tmp.cleanup)
        self.workspace = Path(self.tmp.name)
        self.roots = {}
        for short, folder in [('ab', 'ActuatorBoard26'), ('vb', 'ValveBoard26')]:
            root = self.workspace / folder
            (root / 'tests').mkdir(parents=True)
            shutil.copy2(helper, root / 'tests/native_sdk.py')
            sdk, lc = self.dependency(short + '-sdk'), self.dependency(short + '-launchcore')
            cache = root / 'build/native/CMakeCache.txt'
            cache.parent.mkdir(parents=True)
            cache.write_text(f'FETCHCONTENT_SOURCE_DIR_SEDSNET:PATH={sdk}\nFETCHCONTENT_SOURCE_DIR_SEDSLAUNCHCORE:PATH={lc}\n')
            self.roots[short] = (root, sdk, lc)

    def dependency(self, name):
        path = self.workspace / name
        path.mkdir()
        (path / 'CMakeLists.txt').write_text('# mock configure dependency\n')
        header = path / 'bootloader/include/launchcore/persist.h'
        header.parent.mkdir(parents=True)
        header.write_text('launchcore_persist_get_strict\n')
        return path

    def capture(self, records, fail=None):
        def compile_one(root, out, short, sdk, lc):
            # Selection and environment installation are real tooling code.
            # Only actual C compiler work is replaced by this observer.
            self.assertEqual(os.environ['SEDSNET_TEST_SOURCE'], str(sdk))
            self.assertEqual(os.environ['SDK_FREE_LAUNCHCORE_SOURCE'], str(lc))
            self.assertEqual(os.environ['RECOVERY_LAUNCH_TEST_SOURCE'], str(root))
            records.append((short, root, sdk, lc))
            if short == fail:
                raise RuntimeError('injected C compiler failure')
        return compile_one

    def test_default_dependencies_use_each_boards_own_real_resolver(self):
        records = []
        with mock.patch.object(self.tool, 'compile_board', self.capture(records)):
            self.tool.build_bridges(self.workspace, self.workspace / 'out')
        self.assertEqual(records, [(short, *self.roots[short]) for short in ['ab', 'vb']])
        self.assertEqual({key: os.environ.get(key) for key in self.original}, {key: None for key in self.original})

    def test_explicit_caller_overrides_apply_to_both_and_are_restored(self):
        sdk, lc = self.dependency('shared-sdk'), self.dependency('shared-launchcore')
        original = {'SEDSNET_TEST_SOURCE': str(sdk), 'SDK_FREE_LAUNCHCORE_SOURCE': str(lc), 'RECOVERY_LAUNCH_TEST_SOURCE': 'caller-source'}
        self.tool.restore_environment(original)
        records = []
        with mock.patch.object(self.tool, 'compile_board', self.capture(records)):
            self.tool.build_bridges(self.workspace, self.workspace / 'out')
        self.assertEqual(records, [(short, self.roots[short][0], sdk, lc) for short in ['ab', 'vb']])
        self.assertEqual({key: os.environ.get(key) for key in original}, original)

    def test_partial_override_does_not_replace_each_boards_default_launchcore(self):
        sdk = self.dependency('shared-sdk')
        os.environ['SEDSNET_TEST_SOURCE'] = str(sdk)
        records = []
        with mock.patch.object(self.tool, 'compile_board', self.capture(records)):
            self.tool.build_bridges(self.workspace, self.workspace / 'out')
        self.assertEqual(records, [(short, self.roots[short][0], sdk, self.roots[short][2]) for short in ['ab', 'vb']])
        self.assertEqual(os.environ['SEDSNET_TEST_SOURCE'], str(sdk))
        self.assertNotIn('SDK_FREE_LAUNCHCORE_SOURCE', os.environ)

    def test_compiler_exception_restores_original_caller_environment(self):
        os.environ['RECOVERY_LAUNCH_TEST_SOURCE'] = 'caller-source'
        before = {key: os.environ.get(key) for key in self.original}
        records = []
        with mock.patch.object(self.tool, 'compile_board', self.capture(records, fail='vb')):
            with self.assertRaisesRegex(RuntimeError, 'C compiler failure'):
                self.tool.build_bridges(self.workspace, self.workspace / 'out')
        self.assertEqual(len(records), 2)
        self.assertEqual({key: os.environ.get(key) for key in before}, before)

    def test_second_board_resolution_failure_does_not_leak_first_board_roots(self):
        (self.roots['vb'][0] / 'build/native/CMakeCache.txt').unlink()
        records = []
        with mock.patch.object(self.tool, 'compile_board', self.capture(records)):
            with self.assertRaisesRegex(RuntimeError, 'Configure the current firmware'):
                self.tool.build_bridges(self.workspace, self.workspace / 'out')
        self.assertEqual(records, [('ab', *self.roots['ab'])])
        self.assertEqual({key: os.environ.get(key) for key in self.original}, {key: None for key in self.original})


if __name__ == '__main__':
    unittest.main()
