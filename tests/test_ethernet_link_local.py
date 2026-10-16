import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('ethernet_link_local',Path(__file__).resolve().parents[1]/'backend/tools/ethernet_link_local.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
class EthernetConfigurationTests(unittest.TestCase):
    def test_networkd_keeps_dhcp_and_enables_both_link_local_families(self):
        text=m.configuration('networkd','eth0',netplan_id='wired')
        self.assertIn('    wired:\n',text)
        self.assertIn('dhcp4: true',text)
        self.assertIn('link-local: [ipv4, ipv6]',text)
        self.assertNotIn('renderer:',text)
        self.assertNotIn('addresses:',text)
    def test_networkmanager_uses_native_property_and_quotes_existing_profile(self):
        text=m.configuration('networkmanager','eth0',"Wired connection '1'")
        self.assertIn('ipv4.method auto ipv4.link-local enabled',text)
        self.assertIn("'Wired connection '",text)
        self.assertNotIn('connection add',text)
    def test_refuses_ambiguous_or_injected_interfaces_and_missing_profiles(self):
        for args in [('networkmanager','eth0'),('networkd','eth0\nother'),('other','eth0')]:
            with self.assertRaises(ValueError):m.configuration(*args)
if __name__=='__main__':unittest.main()
