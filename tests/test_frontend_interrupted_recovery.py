"""Local Git/fake frontend controls for uncertain source/target isolation."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import build


class InterruptedFrontendRecoveryTests(unittest.TestCase):
    def source(self, root):
        remote = root / 'remote'
        (remote / 'src').mkdir(parents=True)
        (remote / 'station_ui').mkdir()
        (remote / 'Cargo.toml').write_text('[package]\nname="isolated_fixture"\nversion="0.1.0"\nedition="2021"\n')
        (remote / 'src/main.rs').write_text('fn main() {}\n')
        (remote / '.gitignore').write_text('target/\ndist/\nCargo.lock\nuntracked-poison\n')
        (remote / 'value.txt').write_text('committed-safe-output')
        (remote / 'station_ui/page').write_text('committed-station-assets')
        (remote / 'station_ui/manifest.json').write_text(json.dumps({'version': 1, 'exports': [{'name':'PAGE_HTML','path':'station_ui/page','kind':'text'}]}))
        (remote / 'build.py').write_text('''import json,os,time
from pathlib import Path
target=Path(os.environ['CARGO_TARGET_DIR']);target.mkdir(parents=True,exist_ok=True)
(target/'CACHEDIR.TAG').write_text('Signature: 8a477f597d28d172789f06886806bc55\\n')
if os.environ.get('ISOLATION_READY'):
 ready=Path(os.environ['ISOLATION_READY']);ready.write_text(str(os.getpid()))
 target=Path(os.environ['CARGO_TARGET_DIR']);target.mkdir(parents=True,exist_ok=True)
 while not Path(os.environ['ISOLATION_RELEASE']).exists():
  with (target/'live-writer').open('a') as f:f.write('alive\\n')
  time.sleep(.02)
if os.environ.get('ISOLATION_FAIL'):raise SystemExit(9)
p=Path('dist/public');p.mkdir(parents=True,exist_ok=True)
p.joinpath('index.html').write_text(json.dumps({'source':str(Path.cwd()),'target':os.environ['CARGO_TARGET_DIR'],'value':Path('value.txt').read_text()}))
''')
        for args in [('init','-q'),('config','user.name','Local fixture'),('config','user.email','fixture@example.invalid'),('config','commit.gpgsign','false'),('add','.'),('commit','-qm','Committed local fixture')]:
            subprocess.run(['git','-C',str(remote),*args],check=True,capture_output=True)
        revision=subprocess.check_output(['git','-C',str(remote),'rev-parse','HEAD'],text=True).strip()
        source=root/'frontend-revisions'/revision
        subprocess.run(['git','clone','--quiet','--no-hardlinks',str(remote),str(source)],check=True,capture_output=True)
        return source

    def invoke(self, destination, source, existing=False):
        build._run_frontend_build(destination,False,False,existing,None,True,source)
        return json.loads((destination/'frontend/dist/public/index.html').read_text())

    def source_marker(self, source):
        return source.parent/('.'+source.name+'.gs-frontend-output-in-progress.json')

    def mark(self, path):
        path.write_text(json.dumps({'version':1,'pid':os.getpid(),'source':'untrusted marker path','target':'untrusted marker path'}))
        return path.read_bytes()

    def clean_env(self):
        values={k:v for k,v in os.environ.items() if k not in ['CARGO_TARGET_DIR',build.BROWSER_ASSET_SOURCE_ENV,'ISOLATION_READY','ISOLATION_RELEASE','ISOLATION_FAIL']}
        return patch.dict(os.environ,values,clear=True)

    def test_public_wrapper_recovers_source_marker_without_touching_pid_output_or_quarantine(self):
        with tempfile.TemporaryDirectory() as temp, self.clean_env():
            root=Path(temp);source=self.source(root);marker=self.source_marker(source);before=self.mark(marker)
            prior=source/'dist/.gs-public-prior-old/public';prior.mkdir(parents=True);(prior/'index.html').write_text('quarantined')
            (source/'untracked-poison').write_text('must-not-be-cloned')
            result=self.invoke(root/'gs',source)
            recovered=Path(result['source']);target=source.parent/'target-recovery'
            self.assertEqual(result['value'],'committed-safe-output');self.assertEqual(result['target'],str(target))
            self.assertNotEqual(recovered,source);self.assertFalse((recovered/'untracked-poison').exists())
            self.assertFalse((recovered/'.git/objects/info/alternates').exists())
            self.assertNotEqual((recovered/'value.txt').stat().st_ino,(source/'value.txt').stat().st_ino)
            self.assertEqual(marker.read_bytes(),before);self.assertEqual((prior/'index.html').read_text(),'quarantined')
            receipt=json.loads((root/'gs/frontend/dist/station-source.json').read_text());self.assertEqual(receipt['source_dir'],str(recovered))
            self.assertEqual(os.environ[build.BROWSER_ASSET_SOURCE_ENV],str(recovered))
            self.assertEqual(json.loads((target/'.gs-frontend-source.json').read_text())['source'],str(recovered))
            self.invoke(root/'gs',source)
            snapshots=[p for p in(source.parent/'.gs-frontend-recovery-sources').iterdir()if p.is_dir()]
            self.assertEqual([p.name for p in snapshots],[source.name])
            self.assertEqual([p.name for p in source.parent.glob('target*') if p.is_dir()],['target-recovery'])

    def test_target_only_uncertainty_also_uses_independent_source_and_fixed_target(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);marker=source.parent/'.target.gs-frontend-in-progress.json';before=self.mark(marker)
            result=self.invoke(root/'gs',source)
            self.assertNotEqual(result['source'],str(source));self.assertEqual(result['target'],str(source.parent/'target-recovery'))
            self.assertEqual(marker.read_bytes(),before)

    def test_uncertain_recovery_lane_never_recurses_or_touches_its_writer_marker(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);self.mark(self.source_marker(source));result=self.invoke(root/'gs',source)
            target=Path(result['target']);marker=target.parent/('.'+target.name+'.gs-frontend-in-progress.json');before=self.mark(marker)
            with self.assertRaisesRegex(RuntimeError,'also interrupted'):
                self.invoke(root/'gs',source)
            self.assertEqual(marker.read_bytes(),before);self.assertEqual(len(list(source.parent.glob('target*-recovery*'))),1)

    def test_explicit_target_unmanaged_source_and_existing_requests_keep_guard(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);self.mark(self.source_marker(source))
            with patch.dict(os.environ,{'CARGO_TARGET_DIR':str(root/'explicit')}),self.assertRaisesRegex(RuntimeError,'uncertain'):
                self.invoke(root/'gs',source)
            with self.assertRaisesRegex(RuntimeError,'uncertain'):self.invoke(root/'gs',source,True)
            unmanaged=root/'user-source';source.rename(unmanaged);self.mark(self.source_marker(unmanaged))
            with self.assertRaisesRegex(RuntimeError,'uncertain'):self.invoke(root/'gs',unmanaged)
            self.assertFalse((root/'frontend-revisions/.gs-frontend-recovery-sources').exists())

    def test_recovery_source_slots_are_bounded_and_dirty_sources_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);self.mark(self.source_marker(source));cache=source.parent/'.gs-frontend-recovery-sources';cache.mkdir()
            for n in range(8):(cache/('.snapshot-'+str(n))).mkdir()
            with self.assertRaisesRegex(RuntimeError,'8-slot'):self.invoke(root/'gs',source)
            for n in range(8):(cache/('.snapshot-'+str(n))).rmdir()
            result=self.invoke(root/'gs',source);recovered=Path(result['source']);(recovered/'value.txt').write_text('modified')
            with self.assertRaisesRegex(RuntimeError,'changed'):self.invoke(root/'gs',source)
            self.assertEqual((recovered/'value.txt').read_text(),'modified')

    def test_preexisting_recovery_marker_allocates_no_source_or_new_target(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);old=self.mark(self.source_marker(source));marker=source.parent/'.target-recovery.gs-frontend-in-progress.json';before=self.mark(marker)
            with self.assertRaisesRegex(RuntimeError,'also interrupted'):self.invoke(root/'gs',source)
            self.assertFalse((source.parent/'.gs-frontend-recovery-sources').exists())
            self.assertFalse((source.parent/'target-recovery').exists())
            self.assertEqual(marker.read_bytes(),before);self.assertEqual(self.source_marker(source).read_bytes(),old)

    def test_unowned_or_symlinked_fixed_target_is_never_cleaned(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);self.mark(self.source_marker(source))
            recovery=source.parent/'target-recovery';recovery.mkdir();sentinel=recovery/'unrelated';sentinel.write_text('keep')
            with self.assertRaisesRegex(RuntimeError,'without owned provenance'):self.invoke(root/'gs',source)
            self.assertEqual(sentinel.read_text(),'keep');sentinel.unlink();recovery.rmdir()
            normal=source.parent/'target';normal.mkdir();recovery.symlink_to(normal,target_is_directory=True)
            with self.assertRaisesRegex(RuntimeError,'independent local'):self.invoke(root/'gs',source)
            self.assertTrue(recovery.is_symlink())

    def test_failed_recovery_never_publishes_or_commits_provenance(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);self.mark(self.source_marker(source));public=root/'gs/frontend/dist/public';public.mkdir(parents=True);(public/'index.html').write_text('published-good')
            with patch.dict(os.environ,{'ISOLATION_FAIL':'1'}),self.assertRaises(subprocess.CalledProcessError):self.invoke(root/'gs',source)
            self.assertEqual((public/'index.html').read_text(),'published-good')
            self.assertFalse((source.parent/'target-recovery/.gs-frontend-source.json').exists())
            self.assertTrue(self.source_marker(source).exists())
            result=self.invoke(root/'gs',source);self.assertEqual(result['value'],'committed-safe-output')

    def test_local_file_snapshot_handles_mac_style_spaces_without_shell_commands(self):
        with tempfile.TemporaryDirectory(prefix='Mac source with spaces ') as temp,self.clean_env():
            root=Path(temp);source=self.source(root);self.mark(self.source_marker(source))
            result=self.invoke(root/'GS destination with spaces',source)
            self.assertEqual(result['value'],'committed-safe-output')
            self.assertIn('Mac source with spaces',result['source'])

    @unittest.skipUnless(os.name=='posix','Actual POSIX active recovery writer ownership control')
    def test_active_fixed_recovery_writer_is_not_killed_or_replaced(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);original=self.mark(self.source_marker(source));ready=root/'ready';release=root/'release'
            env={**os.environ,'PYTHONPATH':str(Path(build.__file__).parent),'ISOLATION_READY':str(ready),'ISOLATION_RELEASE':str(release)}
            code='import build,sys;from pathlib import Path;build._run_frontend_build(Path(sys.argv[1]),False,False,False,None,True,Path(sys.argv[2]))'
            writer=subprocess.Popen([sys.executable,'-c',code,str(root/'first-gs'),str(source)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            group=None
            try:
                until=time.monotonic()+10
                while not ready.exists() or not ready.read_text():self.assertLess(time.monotonic(),until);time.sleep(.01)
                group=int(ready.read_text());marker=source.parent/'.target-recovery.gs-frontend-in-progress.json';before=marker.read_bytes()
                with self.assertRaisesRegex(RuntimeError,'also interrupted'):self.invoke(root/'second-gs',source)
                self.assertIsNone(writer.poll());os.kill(group,0);self.assertEqual(marker.read_bytes(),before)
                self.assertFalse((root/'second-gs/frontend/dist/public').exists())
                release.touch();self.assertEqual(writer.wait(timeout=10),0)
                self.assertEqual(json.loads((root/'first-gs/frontend/dist/public/index.html').read_text())['value'],'committed-safe-output')
                self.assertEqual(self.source_marker(source).read_bytes(),original)
            finally:
                if writer.poll()is None:writer.kill();writer.wait()
                if group is not None:
                    try:os.killpg(group,signal.SIGKILL)
                    except ProcessLookupError:pass

    @unittest.skipUnless(os.name=='posix','Actual POSIX parent-death and surviving process-group control')
    def test_actual_killed_wrapper_surviving_child_remains_alive_during_safe_retry(self):
        with tempfile.TemporaryDirectory() as temp,self.clean_env():
            root=Path(temp);source=self.source(root);public=source/'dist/public';public.mkdir(parents=True);(public/'index.html').write_text('old-good')
            ready=root/'ready';release=root/'release';env={**os.environ,'PYTHONPATH':str(Path(build.__file__).parent),'ISOLATION_READY':str(ready),'ISOLATION_RELEASE':str(release)}
            code='import build,sys;from pathlib import Path;build._run_frontend_build(Path(sys.argv[1]),False,False,False,None,True,Path(sys.argv[2]))'
            wrapper=subprocess.Popen([sys.executable,'-c',code,str(root/'first-gs'),str(source)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            group=None
            try:
                until=time.monotonic()+10
                while not ready.exists() or not ready.read_text():self.assertLess(time.monotonic(),until);time.sleep(.01)
                group=int(ready.read_text());wrapper.kill();wrapper.wait(timeout=5)
                source_marker=self.source_marker(source);target_marker=source.parent/'.target.gs-frontend-in-progress.json';source_before=source_marker.read_bytes();target_before=target_marker.read_bytes()
                quarantines=list((source/'dist').glob('.gs-public-prior-*/public/index.html'));self.assertEqual(len(quarantines),1)
                result=self.invoke(root/'second-gs',source)
                os.kill(group,0);self.assertEqual(result['value'],'committed-safe-output')
                self.assertEqual(source_marker.read_bytes(),source_before);self.assertEqual(target_marker.read_bytes(),target_before)
                self.assertEqual(quarantines[0].read_text(),'old-good');self.assertTrue((source.parent/'target/live-writer').exists())
            finally:
                if wrapper.poll()is None:wrapper.kill();wrapper.wait()
                if group is not None:
                    try:os.killpg(group,signal.SIGKILL)
                    except ProcessLookupError:pass

if __name__=='__main__':unittest.main()
