import importlib.util
import os
from pathlib import Path
import runpy
import sys
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location("gs_launcher",ROOT/"run_groundstation.py")
launcher=importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)

class LauncherTests(unittest.TestCase):
    def invoke(self,argv,pi=False):
        with patch.object(sys,"argv",["run_groundstation.py",*argv]),patch.dict(os.environ,{},clear=True),patch.object(launcher,"is_raspberry_pi",return_value=pi),patch.object(launcher.subprocess,"run") as build,patch.object(launcher,"refresh_sedsnet_dependency"),patch.object(launcher,"run") as backend:
            launcher.main()
            return build,backend,dict(os.environ)

    def test_frontend_dev_spellings_forward_to_builder_for_both_roles(self):
        for role in [[],["--rrls"]]:
            for option in ["--frontend-dev","--frontend_dev","frontend_dev","frontend-dev"]:
                with self.subTest(role=role,option=option):
                    build,_,_=self.invoke([*role,option])
                    self.assertIn("--frontend-dev",build.call_args.args[0])
                    self.assertIn("frontend_web",build.call_args.args[0])

    def test_default_remains_stable_and_backend_only_does_not_rebuild_frontend(self):
        build,_,_=self.invoke([])
        self.assertNotIn("--frontend-dev",build.call_args.args[0])
        build,backend,_=self.invoke(["--rrls","--backend-only-build","--frontend-dev"])
        build.assert_not_called()
        backend.assert_called_once()

    def test_rrls_on_pi_sets_role_without_enabling_local_gpio(self):
        _,backend,env=self.invoke(["--rrls"],pi=True)
        self.assertEqual(env["GS_RRLS_ROLE"],"rrls")
        self.assertNotIn("raspberry_pi",backend.call_args.args[0])

    def test_rrls_wrapper_preserves_frontend_choice_and_selects_own_config(self):
        wrapper=runpy.run_path(str(ROOT/"run_rrls.py"),run_name="test_rrls")
        with patch.dict(os.environ,{},clear=True),patch.object(sys,"argv",["run_rrls.py","frontend_dev","--radio-port","/dev/cu.test-radio","--comms-config","/tmp/test-rrls.json"]),patch("runpy.run_path") as delegated:
            wrapper["main"]()
            self.assertEqual(sys.argv[1:],["--rrls","frontend_dev"])
            self.assertEqual(os.environ["GS_RRLS_RADIO_PORT"],"/dev/cu.test-radio")
            self.assertEqual(Path(os.environ["GS_RRLS_COMMS_LINK_CONFIG"]),Path("/tmp/test-rrls.json").resolve())
            self.assertEqual(delegated.call_args.kwargs["run_name"],"__main__")

if __name__=="__main__":unittest.main()
