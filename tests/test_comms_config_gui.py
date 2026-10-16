"""Configuration tests use no display, sockets or hardware devices."""
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('comms_config_gui', Path(__file__).resolve().parents[1] / 'backend/tools/comms_config_gui.py')
m = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = m
spec.loader.exec_module(m)

class CommsSetupTests(unittest.TestCase):
    def config(self, **options):
        cfg=m.normalize_config({})
        cfg['fill_box'].update(interface='pico_fi_tcp', **options)
        return cfg

    def test_default_filename_matches_backend(self):
        self.assertEqual(m.default_config_path().name,'comms.json')

    def test_new_install_and_shipped_gateway_settings_use_main_network(self):
        cfg=m.validate_config(m.normalize_config({}))
        shipped=m.validate_config(m.load_config(m.default_config_path()))
        for config in (cfg,shipped):
            self.assertEqual(config['fill_box']['interface'],'pico_fi_tcp')
            self.assertEqual(config['fill_box']['mode'],'client')
            self.assertEqual(config['fill_box']['address'],'192.168.3.20:9000')

    def test_tcp_client_and_server_round_trip_preserve_other_link(self):
        for mode,address,peer in [('client','192.168.3.8:9000',None),('server','0.0.0.0:9000','192.168.3.8'),('server','[::1]:9000',None)]:
            cfg=m.validate_config(self.config(mode=mode,address=address,allowed_peer=peer,port=''))
            with tempfile.TemporaryDirectory() as folder:
                path=Path(folder)/'comms.json';m.save_config(path,cfg)
                self.assertEqual(m.validate_config(m.load_config(path)),cfg)
            self.assertEqual(cfg['av_bay']['interface'],'usb_serial')

    def test_public_server_requires_peer_and_rejects_invalid_address(self):
        for options in [dict(mode='server',address='0.0.0.0:9000'),dict(address='example.org:9000'),dict(address='127.0.0.1:0'),dict(address='127.0.0.1:65536'),dict(address='::1:9000'),dict(allowed_peer='not-an-ip')]:
            with self.subTest(options=options),self.assertRaises(ValueError):m.validate_config(self.config(**options))

    def test_i2c_does_not_need_a_serial_device(self):
        cfg=m.normalize_config({})
        cfg['fill_box'].update(interface='i2c',bus=0,addr=0x55,protocol_version=2,port='')
        self.assertEqual(m.validate_config(cfg)['fill_box']['bus'],0)
        cfg['fill_box']['addr']=128
        with self.assertRaises(ValueError):m.validate_config(cfg)

    def test_cli_tcp_flags(self):
        with patch.object(sys,'argv',['setup','--cli','--fill-box-interface','pico_fi_tcp','--fill-box-tcp-mode','server','--fill-box-tcp-address','0.0.0.0:9000','--fill-box-allowed-peer','192.168.3.8']):
            args=m.parse_args()
        cfg=m.apply_cli_overrides({},args)
        self.assertEqual(cfg['fill_box']['mode'],'server')
        self.assertEqual(cfg['fill_box']['address'],'0.0.0.0:9000')
        self.assertEqual(cfg['fill_box']['allowed_peer'],'192.168.3.8')

if __name__=='__main__':unittest.main()
