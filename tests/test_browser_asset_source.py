import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import build


class BrowserAssetSourceTests(unittest.TestCase):
    def source(self, root):
        source = root / 'frontend-source'
        (source / 'station_ui').mkdir(parents=True)
        (source / 'station_ui/page.html').write_text('first version')
        (source / 'station_ui/manifest.json').write_text(json.dumps({'version':1,'exports':[
            {'name':'PAGE_HTML','path':'station_ui/page.html','kind':'text'}]}))
        return source

    def test_backend_only_honors_explicit_dev_before_building_backend(self):
        order = []
        with patch.dict(os.environ, {}, clear=True), patch.object(build.sys, 'argv', ['build.py', 'backend_only', '--frontend-dev']), patch.object(build, '_run_frontend_build', side_effect=lambda *args: order.append(('web', args[-1]))), patch.object(build, 'prepare_browser_asset_source', side_effect=lambda *args, **kwargs: order.append(('reuse', kwargs['refresh']))), patch.object(build, '_run_script', side_effect=lambda *args: order.append(('backend', True))):
            build.main()
        self.assertEqual(order, [('web', True), ('reuse', False), ('backend', True)])

    def test_parallel_build_selects_dev_once_before_children(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root);processes=[]
            class Process:
                exitcode=0
                def __init__(self, target, args):
                    self.target=target;self.args=args;processes.append(self)
                    self.source=os.environ.get(build.BROWSER_ASSET_SOURCE_ENV)
                def start(self): pass
                def join(self): pass
            with patch.object(build.sys,'argv',['build.py','--frontend-dev']),patch.object(build,'_frontend_checkout_dir',return_value=source),patch.object(build,'_immutable_frontend_checkout',return_value=source) as ensure,patch.object(build,'in_docker_build',return_value=False),patch.object(build.mp,'Process',Process):
                build.main()
            ensure.assert_called_once_with(source,'dev')
            self.assertEqual(len(processes),2)
            self.assertEqual(processes[0].args[-1],source.resolve())
            self.assertEqual([p.source for p in processes],[str(source.resolve())]*2)

    def test_reusing_preselected_source_never_fetches_again(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root)
            with patch.object(build,'_ensure_frontend_checkout') as ensure,patch.object(build,'_resolve_external_frontend_script',return_value=source/'build.py'),patch.object(build,'_run_frontend_script_with_wasm_opt_fallback'),patch.object(build,'_sync_frontend_public_assets'):
                build._run_frontend_build(root,False,False,False,None,True,source)
            ensure.assert_not_called()
            receipt=json.loads(build._browser_source_receipt(root).read_text())
            self.assertEqual(receipt['source_dir'],str(source.resolve()))

    def test_launcher_reuse_requires_same_asset_bytes_as_built_webui(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root);build._record_browser_asset_source(root,source)
            with patch.object(build,'_ensure_frontend_checkout') as ensure:
                self.assertEqual(build.prepare_browser_asset_source(root,refresh=False),source.resolve())
                ensure.assert_not_called()
            os.environ.pop(build.BROWSER_ASSET_SOURCE_ENV)
            (source/'station_ui/page.html').write_text('changed behind existing WebUI')
            with self.assertRaisesRegex(RuntimeError,'changed since the WebUI build'):
                build.prepare_browser_asset_source(root,refresh=False)

    def test_explicit_source_supports_archive_builds_without_git(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root);os.environ[build.BROWSER_ASSET_SOURCE_ENV]=str(source)
            with patch.object(build,'_ensure_frontend_checkout') as ensure:
                self.assertEqual(build.prepare_browser_asset_source(root,True),source.resolve())
                ensure.assert_not_called()

    def test_source_must_contain_manifest_and_exports_within_checkout(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);source=self.source(root);manifest=source/'station_ui/manifest.json'
            for path in ['../outside.html',str(root/'outside.html')]:
                manifest.write_text(json.dumps({'version':1,'exports':[{'path':path}]}))
                with self.assertRaisesRegex(RuntimeError,'relative'):
                    build._browser_asset_digest(source)
            (root/'outside.html').write_text('outside')
            (source/'station_ui/link.html').symlink_to(root/'outside.html')
            manifest.write_text(json.dumps({'version':1,'exports':[{'path':'station_ui/link.html'}]}))
            with self.assertRaisesRegex(RuntimeError,'outside'):
                build._browser_asset_digest(source)
            manifest.unlink()
            with self.assertRaisesRegex(RuntimeError,'no valid station assets'):
                build._browser_asset_digest(source)

    def test_digest_prepares_missing_generated_assets_without_frontend_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);source=self.source(root)
            (source/'scripts').mkdir()
            (source/'scripts/browser_assets.py').write_text("import pathlib,sys\nroot=pathlib.Path(sys.argv[sys.argv.index('--root')+1])\n(root/'generated.js').write_text('prepared by selected frontend')\n")
            (source/'station_ui/manifest.json').write_text(json.dumps({'version':1,'exports':[{'name':'GENERATED_JS','path':'generated.js','kind':'text'}]}))
            digest=build._browser_asset_digest(source)
            self.assertEqual((source/'generated.js').read_text(),'prepared by selected frontend')
            self.assertEqual(build._browser_asset_digest(source),digest)

    def test_first_backend_only_run_fetches_main_and_ignores_legacy_ui(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root);(root/'ui').mkdir()
            cache=root/'external-cache'
            with patch.object(build,'_frontend_checkout_dir',return_value=cache), patch.object(build,'_immutable_frontend_checkout',return_value=source) as checkout:
                self.assertEqual(build.prepare_browser_asset_source(root,refresh=False),source.resolve())
                checkout.assert_called_once_with(cache,'main')

    def test_fresh_backend_source_can_select_dev_without_a_submodule(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root)
            cache=root/'external-cache'
            with patch.object(build,'_frontend_checkout_dir',return_value=cache), patch.object(build,'_immutable_frontend_checkout',return_value=source) as checkout:
                self.assertEqual(build.prepare_browser_asset_source(root,True,refresh=False),source.resolve())
                checkout.assert_called_once_with(cache,'dev')

    def test_cargo_helper_returns_selected_archive_path_with_spaces(self):
        with tempfile.TemporaryDirectory(prefix='frontend source ') as tmp:
            root=Path(tmp);source=self.source(root);receipt=root/'result.json'
            environment=os.environ.copy()
            environment[build.BROWSER_ASSET_SOURCE_ENV]=str(source)
            helper=Path(build.__file__).parent/'scripts/prepare_frontend_source.py'
            subprocess.run([sys.executable,str(helper),'--output',str(receipt)],env=environment,check=True,capture_output=True,text=True)
            self.assertEqual(json.loads(receipt.read_text()),{'version':1,'source_dir':str(source.resolve())})


if __name__=='__main__': unittest.main()
