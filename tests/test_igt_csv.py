"""tools/igt_csv.py: integrated-ground-test CSVs for the FC SD card
(docs/integrated-ground-test.md section 4)."""
import csv
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('igt_csv', ROOT / 'tools/igt_csv.py')
igt_csv = importlib.util.module_from_spec(spec)
sys.modules['igt_csv'] = igt_csv
spec.loader.exec_module(igt_csv)


def read_rows(path):
    lines = path.read_text().splitlines()
    return lines[0], [[float(x) for x in line.split(',')] for line in lines[1:]]


class ModelTests(unittest.TestCase):
    def test_model_reaches_the_requested_apogee_and_passes_the_fc_checks(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            self.assertEqual(igt_csv.main(['model', '--out', str(out), '--apogee-m', '700']), 0)
            folder = out / 'IGT'
            self.assertEqual(sorted(p.name for p in folder.iterdir()), ['BARO0.CSV', 'GPS0.CSV', 'IMU0.CSV'])
            header, baro = read_rows(folder / 'BARO0.CSV')
            self.assertEqual(header, igt_csv.HEADERS['BARO'])
            apogee = max(r[1] for r in baro)
            self.assertAlmostEqual(apogee, 700, delta=10)
            # At rest until liftoff at t = 10 s, then back at rest on the ground at the end.
            self.assertTrue(all(abs(r[1]) < 0.01 for r in baro if r[0] < 10000))
            self.assertTrue(all(abs(r[1]) < 0.01 for r in baro[-100:]))
            _, imu = read_rows(folder / 'IMU0.CSV')
            self.assertAlmostEqual(imu[0][3], 9.8066, places=3)          # specific force at rest
            self.assertGreater(max(r[3] for r in imu), 3 * 9.80665)       # boost
            self.assertLess(min(r[3] for r in imu if 14000 < r[0] < 20000), 0)  # coast: drag only
            # Times strictly increase and every line fits the FC's 120-character limit.
            for name in ('IMU0.CSV', 'BARO0.CSV', 'GPS0.CSV'):
                text = (folder / name).read_text()
                self.assertTrue(all(len(line) <= 120 for line in text.splitlines()))
                times = [int(line.split(',')[0]) for line in text.splitlines()[1:]]
                self.assertEqual(times, sorted(set(times)))
            self.assertEqual(igt_csv.check_set(out, igt_csv.Limits()), [])

    def test_descent_rates_and_main_below_the_reef_altitude(self):
        with tempfile.TemporaryDirectory() as tmp:
            igt_csv.main(['model', '--out', tmp, '--apogee-m', '900', '--no-gps'])
            _, baro = read_rows(Path(tmp) / 'IGT/BARO0.CSV')
            self.assertFalse((Path(tmp) / 'IGT/GPS0.CSV').exists())
            vel = [((b[1] - a[1]) / ((b[0] - a[0]) / 1000), b[1]) for a, b in zip(baro, baro[1:])]
            drogue = [v for v, h in vel if 600 < h < 800 and v < 0]
            main = [v for v, h in vel if 50 < h < 300 and v < 0]
            self.assertAlmostEqual(sum(drogue) / len(drogue), -25, delta=1.5)
            self.assertAlmostEqual(sum(main) / len(main), -6, delta=0.5)

    def test_gps_track_moves_in_lat_and_lon_by_a_few_hundred_metres(self):
        import math
        with tempfile.TemporaryDirectory() as tmp:
            igt_csv.main(['model', '--out', tmp, '--apogee-m', '600', '--site', '43.003556,-78.768556,182'])
            _, gps = read_rows(Path(tmp) / 'IGT/GPS0.CSV')
            north = [(g[1] - 43.003556) * 111320 for g in gps]
            east = [(g[2] + 78.768556) * 111320 * math.cos(math.radians(43.003556)) for g in gps]
            track = max(math.hypot(n, e) for n, e in zip(north, east))
            self.assertTrue(150 < track < 1000, track)
            self.assertGreater(max(map(abs, north)), 50)    # latitude moves
            self.assertGreater(max(map(abs, east)), 50)     # and longitude
            self.assertAlmostEqual(max(g[3] for g in gps) - 182, 600, delta=15)  # altitude carries the motion
            self.assertTrue(all(abs(n) < 1 and abs(e) < 1 for n, e, g in zip(north, east, gps) if g[0] < 10000))
            # Checked against the model's own site by default (the station's Launch site setting).
            self.assertEqual(igt_csv.check_set(Path(tmp), igt_csv.Limits(43.003556, -78.768556, 182)), [])
            self.assertTrue(igt_csv.check_set(Path(tmp), igt_csv.Limits()))

    def test_segments(self):
        with tempfile.TemporaryDirectory() as tmp:
            igt_csv.main(['model', '--out', tmp, '--apogee-m', '500', '--segment-s', '40'])
            names = sorted(p.name for p in (Path(tmp) / 'IGT').iterdir())
            self.assertIn('IMU1.CSV', names)
            self.assertIn('BARO2.CSV', names)
            last = int((Path(tmp) / 'IGT/IMU0.CSV').read_text().splitlines()[-1].split(',')[0])
            first = int((Path(tmp) / 'IGT/IMU1.CSV').read_text().splitlines()[1].split(',')[0])
            self.assertGreater(first, last)
            self.assertEqual(igt_csv.check_set(Path(tmp), igt_csv.Limits()), [])


class CheckTests(unittest.TestCase):
    def write(self, folder, name, text):
        folder.mkdir(parents=True, exist_ok=True)
        (folder / name).write_text(text)

    def test_reports_problems_like_the_fc(self):
        imu = igt_csv.HEADERS['IMU'] + '\n' + '\n'.join(f'{t},0,0,9.8,0,0,0' for t in range(0, 6001, 10))
        baro_ok = igt_csv.HEADERS['BARO'] + '\n' + '\n'.join(f'{t},0,101000,20' for t in range(0, 6001, 20))
        cases = {
            'bad header': (baro_ok.replace('alt_m', 'altitude')),
            'time not increasing': baro_ok + '\n6000,0,101000,20',
            'parse error': baro_ok + '\n7000,1e3,101000,20',
            'value out of range': baro_ok + '\n7000,0,1000,20',
        }
        for problem, baro in cases.items():
            with self.subTest(problem), tempfile.TemporaryDirectory() as tmp:
                folder = Path(tmp) / 'IGT'
                self.write(folder, 'IMU0.CSV', imu)
                self.write(folder, 'BARO0.CSV', baro)
                problems = igt_csv.check_set(Path(tmp), igt_csv.Limits())
                self.assertTrue(any(problem in p for p in problems), problems)
        with tempfile.TemporaryDirectory() as tmp:
            self.write(Path(tmp) / 'IGT', 'IMU0.CSV', imu)
            self.assertIn('BARO0.CSV: file missing', igt_csv.check_set(Path(tmp), igt_csv.Limits()))

    def test_parser_matches_the_fc(self):
        self.assertEqual(igt_csv.parse_number('12.5', False), 12.5)
        self.assertEqual(igt_csv.parse_number('-0.031', False), -0.031)
        self.assertIsNone(igt_csv.parse_number('1e3', False))
        self.assertIsNone(igt_csv.parse_number('-5', True))
        self.assertIsNone(igt_csv.parse_number('1234567890', False))


class RecordingTests(unittest.TestCase):
    def test_gs_export_anchored_at_launch(self):
        header = ['id', 'received_timestamp_ms', 'source_timestamp_ms', 'sender_id', 'data_type', 'values_json', 'payload_json']
        rows = [[1, 1000, '', 'FC', 'FLIGHT_STATE', '[7]', '']]
        for i, t in enumerate(range(0, 7000, 20)):
            rows.append([i + 2, 1000 + t, 1000 + t, 'FC', 'IMU_DATA', json.dumps([0, 0, 9.8, 0, 0, 0]), ''])
            rows.append([i + 2, 1000 + t, 1000 + t, 'FC', 'BAROMETER_DATA', json.dumps([t / 1000, 101000, 20]), ''])
        rows.append([999, 1500, 1500, 'RF', 'GPS_DATA', json.dumps([31.758, -102.046, 890]), ''])
        rows.append([998, 1600, 1600, 'RF', 'GPS_DATA', json.dumps([31.758, -102.046, 891]), ''])
        rows.append([997, 900, 900, 'FC', 'BAROMETER_DATA', json.dumps([5, 101000, 20]), ''])  # before T0: dropped
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / 'rec.csv'
            with src.open('w', newline='') as handle:
                w = csv.writer(handle)
                w.writerow(header)
                w.writerows(rows)
            out = Path(tmp) / 'set'
            igt_csv.main(['recording', str(src), '--out', str(out)])
            _, baro = read_rows(out / 'IGT/BARO0.CSV')
            self.assertEqual(baro[0][0], 0)
            self.assertEqual(baro[1][:2], [20, 0.02])
            _, gps = read_rows(out / 'IGT/GPS0.CSV')
            self.assertEqual([g[0] for g in gps], [500, 600])

    def test_fc_sd_log_with_the_format_block(self):
        # FC SD files now carry '# seds-sd' metadata lines after the header.
        header = ('timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,'
                  'time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence')
        lines = [header,
                 '# seds-sd,format=fc,version=2,board=FC,firmware=unknown,tick_hz=1000,schema=e59ae8a7',
                 '# seds-sd-columns,u64:ms,u64:ms,label,vec,vec,vec,vec,clock,f32:degC,i32:code,u64:tick,u32:Hz,u64:seq',
                 '# seds-sd-records,IMU:v=m/s2*3;deg/s*3,BARO:v=m;Pa;degC',
                 '1000,1000,launch_accepted,"Launch","","","",local,,,1000,1000,1']
        seq = 2
        for t in range(0, 7000, 20):
            lines.append(f'{1000 + t},{1000 + t},IMU,"0 0 9.8 0 0 0","","","",local,,,{1000 + t},1000,{seq}')
            lines.append(f'{1000 + t},{1000 + t},BARO,"{t / 1000} 101000 20","","","",local,,,{1000 + t},1000,{seq + 1}')
            seq += 2
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / 'irec_LAUNCH-1-000.csv'
            src.write_text('\r\n'.join(lines) + '\r\n', newline='')
            out = Path(tmp) / 'set'
            igt_csv.main(['recording', str(src), '--out', str(out)])
            _, baro = read_rows(out / 'IGT/BARO0.CSV')
            self.assertEqual(baro[0][0], 0)
            self.assertEqual(baro[1][:2], [20, 0.02])


if __name__ == '__main__':
    unittest.main()
