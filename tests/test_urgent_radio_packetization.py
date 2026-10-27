"""Production Capacity and actual emulator packetizer, without an alternate modem."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SIM = Path(os.environ.get('FIRMWARE_SIM_SOURCE', ROOT.parent / 'FirmwareSimulator'))


@unittest.skipUnless(os.environ.get('CSHARP_TEST_DOTNET_IMAGE') and shutil.which('rustc'),
                     'Explicit existing offline .NET image and Rust compiler required')
class UrgentPacketization(unittest.TestCase):
    def test_critical_completion_flushes_real_modem_without_disabling_normal_batches(self):
        with tempfile.TemporaryDirectory(prefix='gs-urgent-packetization-') as directory:
            fixture = Path(directory)
            (fixture / 'schedule.rs').write_text(
                '#[path = ' + json.dumps(str(ROOT / 'backend/src/comms/rfd_capacity.rs'))
                + '] mod capacity;\n' + r'''
fn main() {
    let close = std::env::var("CLOSE_CRITICAL_BATCH").as_deref() == Ok("1");
    let mut cap = capacity::Capacity::new(64_000, 35, 16);
    let wall = std::time::Instant::now();
    let mut times = Vec::new();
    for now in 0..10_000u64 {
        if cap.ready(wall, Some(now)) {
            cap.committed(wall, Some(now), 42);
            times.push(now.to_string());
            // Only one rare critical frame closes its batch. All subsequent
            // offered traffic follows the unchanged ordinary100ms batching.
            if close && times.len() == 4 { cap.close_packet_batch(wall, Some(now)); }
        }
    }
    println!("[{}]", times.join(","));
}
''')
            subprocess.run(['rustc', '--edition=2024', str(fixture/'schedule.rs'),
                            '-o', str(fixture/'schedule')], check=True, capture_output=True)
            shutil.copyfile(SIM/'tests/radio-medium/Program.cs', fixture/'Program.cs')
            shutil.copyfile(SIM/'renode/peripherals/SedsRadioMedium.cs', fixture/'Model.cs')
            (fixture/'contract.csproj').write_text(
                '<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType>'
                '<TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>')
            command = ['docker', 'run', '--rm', '--network=none',
                '--user', f'{os.getuid()}:{os.getgid()}', '--env', 'DOTNET_CLI_HOME=/tmp',
                '--env', 'RADIO_PACKETIZATION_SCHEDULE=/fixture/schedule.json',
                '--env', 'RADIO_INTERACTIVE_PACKETIZATION=1',
                '--mount', f'type=bind,source={fixture},target=/fixture',
                '--workdir', '/fixture', '--entrypoint', 'dotnet',
                os.environ['CSHARP_TEST_DOTNET_IMAGE'], 'run', '--project', 'contract.csproj']
            counts = []
            for close in ['0', '1']:
                env = dict(os.environ, CLOSE_CRITICAL_BATCH=close)
                schedule = subprocess.check_output([str(fixture/'schedule')], env=env, text=True)
                counts.append(len(json.loads(schedule)))
                (fixture/'schedule.json').write_text(schedule)
                result = subprocess.run(command, capture_output=True, text=True, timeout=120)
                log = result.stdout + result.stderr
                print(f'CRITICAL_BOUNDARY={close}\n{log}')
                if close == '0':
                    self.assertNotEqual(result.returncode, 0, log)
                    self.assertIn('critical per-hop packetization exceeded100ms', log)
                else:
                    self.assertEqual(result.returncode, 0, log)
                    self.assertIn('RADIO_PRODUCTION_PACKETIZATION_LATENCY_PASS', log)
            self.assertGreaterEqual(counts[1], counts[0]-1,
                                    'one rare critical flush cannot disable ordinary throughput')


if __name__ == '__main__':
    unittest.main()
