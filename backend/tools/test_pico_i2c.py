import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('pico_i2c', Path(__file__).with_name('pico_i2c.py'))
pico = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pico)

class PicoDeploymentTests(unittest.TestCase):
    def test_clock_update_preserves_other_parameters_and_sections(self):
        text = '[all]\ndtparam=audio=on,i2c_arm=on,i2c_arm_baudrate=400000,spi=on\n[pi4]\narm_boost=1\n'
        result = pico.clock_config(text, 1000000)
        self.assertIn('dtparam=audio=on,i2c_arm=on,spi=on', result)
        self.assertIn('[pi4]\narm_boost=1', result)
        self.assertTrue(result.endswith('[all]\ndtparam=i2c_arm=on,i2c_arm_baudrate=1000000\n'))
        self.assertEqual(pico.clock_config(result, 400000).count('i2c_arm_baudrate='), 1)

    def test_refuses_wrong_firmware_before_flash(self):
        data = bytearray(512)
        struct.pack_into('<8I', data, 0, 0x0A324655, 0x9E5D5157, 0x2000, 0x10000000, 256, 0, 1, 0xE48BFF56)
        struct.pack_into('<I', data, 508, 0x0AB16F30)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'firmware.uf2'
            path.write_bytes(data)
            self.assertEqual(pico.validate_uf2(path), data)
            for offset, value in [(28, 0), (12, 0x20000000), (20, 1), (24, 2), (508, 0)]:
                wrong = bytearray(data); struct.pack_into('<I', wrong, offset, value)
                path.write_bytes(wrong)
                with self.assertRaises(ValueError): pico.validate_uf2(path)

    def test_protocol_mismatch_and_oversize_are_not_idle(self):
        self.assertEqual(pico.decode_header(pico.SELECT), (0, 0))
        for header in [b'I2\x01\x00', bytes([0xD2, 0, 1, 0]), bytes([0xD2, 1, 0xff, 0xff])]:
            with self.assertRaises(ValueError): pico.decode_header(header)

if __name__ == '__main__': unittest.main()
