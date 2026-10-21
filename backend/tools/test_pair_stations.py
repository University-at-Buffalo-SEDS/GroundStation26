import importlib.util,json,os,stat,tempfile,unittest
from pathlib import Path
s=importlib.util.spec_from_file_location('pair',Path(__file__).with_name('pair_stations.py'));pair=importlib.util.module_from_spec(s);s.loader.exec_module(pair)
class PairingTest(unittest.TestCase):
 def test_identity_is_private_persistent_and_transport_independent(self):
  with tempfile.TemporaryDirectory() as d:
   root=pair.provision(Path(d)/'pair','https://station.example','http://rrls.local:3000')
   primary=root/'primary';rrls=root/'rrls'
   self.assertEqual((primary/'identity.seed').read_bytes(),(rrls/'identity.seed').read_bytes())
   self.assertEqual(len((primary/'identity.seed').read_bytes()),32)
   a=json.loads((primary/'pairing.json').read_text());b=json.loads((rrls/'pairing.json').read_text())
   self.assertEqual(a['installation_id'],b['installation_id']);self.assertEqual(b['scheme'],'http');self.assertEqual(b['port'],3000)
   self.assertNotIn('tls_cert',a);self.assertEqual(a['peers'][0]['id'],'rrls')
   if os.name!='nt':self.assertEqual(stat.S_IMODE((primary/'identity.seed').stat().st_mode),0o600)
   before=(primary/'identity.seed').read_bytes()
   with self.assertRaises(ValueError):pair.provision(root,'https://one','https://two')
   self.assertEqual(before,(primary/'identity.seed').read_bytes())
 def test_normalizes_browser_origins_without_tls_identity_dependencies(self):
  self.assertEqual(pair.origin('https://GroundStation.local:443/'),'https://groundstation.local')
  self.assertEqual(pair.origin('http://RRLS.local:3000'),'http://rrls.local:3000')
 def test_no_credential_or_path_in_peer_addresses(self):
  for v in ['http://host:invalid','ftp://host','https://user:secret@host','https://host/api','https://host?token=secret','https://host/#fragment']:
   with self.assertRaises(Exception):pair.origin(v)
if __name__=='__main__':unittest.main()
