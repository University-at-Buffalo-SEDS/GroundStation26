"""Account packaging checks use fake fixtures only, never local users.json."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[1]


class ContainerAccountPackagingTests(unittest.TestCase):
    def test_accounts_are_not_copied_but_empty_directories_and_mount_remain(self):
        dockerfile=(ROOT/'Dockerfile').read_text()
        copies=[line.strip() for line in dockerfile.splitlines() if line.strip().startswith('COPY ')]
        self.assertFalse(any('/users' in line or 'backend/users' in line for line in copies))
        self.assertIn('RUN mkdir -p backend/users',dockerfile)
        self.assertIn('/app/backend/users',dockerfile)
        self.assertIn('./backend/users:/app/backend/users',(ROOT/'docker-compose.yml').read_text())
        rules=set((ROOT/'.dockerignore').read_text().splitlines())
        self.assertTrue({'backend/users/','**/users.json','.pypirc','.netrc','.sedsnet-release.toml','**/credentials.toml','**/*.key'}<=rules)

    def test_empty_private_mount_can_be_bootstrapped_with_first_admin(self):
        # Load the real headless management helper without opening its default path.
        path=ROOT/'backend/tools/users_config_gui.py'
        spec=importlib.util.spec_from_file_location('isolated_account_bootstrap',path)
        users=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(users)
        with tempfile.TemporaryDirectory() as directory:
            private=Path(directory)/'mounted-users/users.json'
            cfg=users.load_config(private)
            self.assertEqual(cfg['users'],[])
            users.upsert_user(cfg,'first-admin',password='synthetic-bootstrap-fixture',view_data=True,
                send_commands=False,calibration_view=False,calibration_edit=False,disabled=False,allowed_commands=[])
            users.set_user_roles(cfg,'first-admin',['admin'])
            users.save_config(private,cfg)
            saved=users.load_config(private)
            self.assertEqual(saved['users'][0]['roles'],['admin'])
            self.assertNotIn('synthetic-bootstrap-fixture',private.read_text())
            self.assertEqual(private.stat().st_mode&0o777,0o600)

    @unittest.skipUnless(os.environ.get('GS_DOCKER_CONTEXT_TEST')=='1' and shutil.which('docker'), 'Opt-in Docker context test')
    def test_real_context_excludes_only_fake_runtime_credential_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            base=Path(directory);context=base/'context';context.mkdir();output=base/'export'
            shutil.copyfile(ROOT/'.dockerignore',context/'.dockerignore')
            for relative in ['backend/users/users.json','backend/other/users.json','.pypirc','.netrc','.sedsnet-release.toml','backend/config/credentials.toml','backend/config/server.key']:
                file=context/relative;file.parent.mkdir(parents=True,exist_ok=True);file.write_text('FAKE-CONTEXT-SECRET-SENTINEL')
            safe=context/'backend/src/main.rs';safe.parent.mkdir(parents=True);safe.write_text('// public source fixture')
            (context/'Dockerfile.context').write_text('FROM scratch\nCOPY . /context\n')
            subprocess.run(['docker','build','--network=none','--file',str(context/'Dockerfile.context'),
                '--output','type=local,dest='+str(output),str(context)],check=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
            self.assertTrue((output/'context/backend/src/main.rs').is_file())
            self.assertFalse(any(file.name in {'users.json','.pypirc','.netrc','.sedsnet-release.toml','credentials.toml','server.key'} for file in output.rglob('*')))
