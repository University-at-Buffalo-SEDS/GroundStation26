import importlib.util, unittest
from unittest.mock import patch
from pathlib import Path
spec=importlib.util.spec_from_file_location('download_map',Path(__file__).resolve().parents[1]/'download_map.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
class WrapperTests(unittest.TestCase):
 def invoke(self,args):
  with patch('sys.argv',['download_map.py']+args),patch.dict(module.os.environ,{},clear=True),patch.object(module,'run')as run:
   module.main()
  return run.call_args
 def test_test_flag_forwarded_to_rust_binary(self):
  args=self.invoke(['--test']);self.assertEqual(args.args[0],['cargo','run','--release','-p','map_downloader','--','--test'])
 def test_direct_interface_and_existing_configuration_preserved(self):
  args=self.invoke(['--test','--direct-interface','eth0','--max-concurrent','1024','--max-bandwidth-mibps','0']);self.assertEqual(args.kwargs['env']['MAP_DIRECT_INTERFACE'],'eth0');self.assertEqual(args.kwargs['env']['MAP_MAX_CONCURRENT'],'1024');self.assertEqual(args.kwargs['env']['MAP_MAX_BANDWIDTH_MIBPS'],'0.0')
 def test_auto_interface_flag_and_test_compatible(self):
  args=self.invoke(['--test','--direct-interface']);self.assertEqual(args.kwargs['env']['MAP_DIRECT_INTERFACE'],'auto')
 def test_rrls_profile_is_explicit_and_preserves_other_configuration(self):
  args=self.invoke(['--profile','rrls','--test','--max-concurrent','1024']);self.assertEqual(args.kwargs['env']['MAP_PROFILE'],'rrls');self.assertEqual(args.kwargs['env']['MAP_MAX_CONCURRENT'],'1024');self.assertEqual(args.args[0][-2:],['--','--test'])
 def test_profile_overrides_only_profile_and_preserves_explicit_region(self):
  with patch('sys.argv',['download_map.py','--profile','rrls','--test']),patch.dict(module.os.environ,{'MAP_PROFILE':'groundstation','MAP_REGION':'custom'},clear=True),patch.object(module,'run')as run:
   module.main()
  self.assertEqual(run.call_args.kwargs['env']['MAP_PROFILE'],'rrls');self.assertEqual(run.call_args.kwargs['env']['MAP_REGION'],'custom')
 def test_targeted_repairs_forward_only_selected_coordinates(self):
  args=self.invoke(['--repair-tile','17/37437/48311','--repair-tile','17/36836/48184']);self.assertEqual(args.args[0][-5:],['--','--repair-tile','17/37437/48311','--repair-tile','17/36836/48184'])
 def test_probe_and_repair_are_rejected_before_starting_cargo(self):
  with self.assertRaises(SystemExit)as error:self.invoke(['--test','--repair-tile','0/0/0'])
  self.assertEqual(error.exception.code,2)
 def test_no_bundle_selects_legacy_files_instead_of_a_silent_direct_noop(self):
  args=self.invoke(['--no-bundle']);self.assertEqual(args.kwargs['env']['MAP_BUILD_BUNDLE'],'0');self.assertEqual(args.kwargs['env']['MAP_DIRECT_TO_BUNDLE'],'0')
 def test_ordinary_download_command_unchanged(self):
  args=self.invoke([]);self.assertEqual(args.args[0],['cargo','run','--release','-p','map_downloader'])
if __name__=='__main__':unittest.main()
