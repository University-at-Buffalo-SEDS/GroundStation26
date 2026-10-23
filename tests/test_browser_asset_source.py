import json
import os
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
            with patch.object(build.sys,'argv',['build.py','--frontend-dev']),patch.object(build,'_frontend_checkout_dir',return_value=source),patch.object(build,'_ensure_frontend_checkout') as ensure,patch.object(build,'in_docker_build',return_value=False),patch.object(build.mp,'Process',Process):
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

    def test_first_backend_only_run_initializes_pinned_ui_without_building_webui(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root);source.rename(root/'ui')
            with patch.object(build,'run') as run:
                self.assertEqual(build.prepare_browser_asset_source(root,refresh=False),(root/'ui').resolve())
                run.assert_not_called()

    def test_missing_pinned_source_is_initialized_once(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            root=Path(tmp);source=self.source(root)
            def initialize(command, cwd):
                self.assertEqual(command,['git','submodule','update','--init','--','ui'])
                self.assertEqual(cwd,root)
                source.rename(root/'ui')
            with patch.object(build,'run',side_effect=initialize) as run:
                self.assertEqual(build.prepare_browser_asset_source(root,refresh=False),(root/'ui').resolve())
                run.assert_called_once()


if __name__=='__main__': unittest.main()
