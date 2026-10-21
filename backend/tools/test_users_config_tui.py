import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import users_config_gui as users

class TerminalUserManagementTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/'users.json'
        self.cfg=users.default_config()
        users.upsert_user(self.cfg,'operator',password='synthetic-test-password',view_data=True,send_commands=False,calibration_view=False,calibration_edit=False,disabled=False,allowed_commands=[],voice_transmit=False)
        users.save_config(self.path,self.cfg)

    def test_role_management_over_a_headless_terminal_preserves_password(self):
        before=self.cfg['users'][0]['password']
        answers=['9','OPERATOR','y','n','n','n','7']
        output=io.StringIO()
        with patch('builtins.input',side_effect=answers), contextlib.redirect_stdout(output):
            self.assertEqual(users.tui_mode(self.path),0)
        saved=json.loads(self.path.read_text())['users'][0]
        self.assertEqual(saved['roles'],['admin'])
        self.assertEqual(saved['password'],before)
        self.assertNotIn('hash_b64',output.getvalue())
        self.assertNotIn('synthetic-test-password',output.getvalue())

    def test_last_admin_cannot_be_deleted_demoted_or_disabled(self):
        users.set_user_roles(self.cfg,'operator',['admin'])
        for mutate in (lambda:users.remove_user(self.cfg,'OPERATOR'),lambda:users.set_user_roles(self.cfg,'operator',[]),lambda:users.upsert_user(self.cfg,'OPERATOR',password=None,view_data=True,send_commands=False,calibration_view=False,calibration_edit=False,disabled=True,allowed_commands=[])):
            with self.assertRaisesRegex(ValueError,'at least one'):
                mutate()
        self.assertEqual(len(self.cfg['users']),1)
        self.assertFalse(self.cfg['users'][0]['disabled'])

    def test_stale_save_preserves_another_administrators_changes(self):
        original=self.path.read_bytes()
        changed={**self.cfg,'session_ttl_seconds':1234}
        users.save_config(self.path,changed)
        with self.assertRaisesRegex(ValueError,'changed on disk'):
            users.save_config(self.path,self.cfg,expected=original)
        self.assertEqual(json.loads(self.path.read_text())['session_ttl_seconds'],1234)
        self.assertFalse(list(self.path.parent.glob('.users-*')))

    def test_cancel_and_case_insensitive_edit_do_not_create_duplicates(self):
        original=self.path.read_bytes()
        with patch('builtins.input',side_effect=EOFError), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(users.tui_mode(self.path),0)
        self.assertEqual(self.path.read_bytes(),original)
        users.upsert_user(self.cfg,'OPERATOR',password=None,view_data=True,send_commands=False,calibration_view=False,calibration_edit=False,disabled=False,allowed_commands=[],voice_transmit=True)
        self.assertEqual(len(self.cfg['users']),1)
        self.assertTrue(self.cfg['users'][0]['permissions']['voice_transmit'])

if __name__=='__main__':unittest.main()
