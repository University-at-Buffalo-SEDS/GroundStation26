import csv
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('sd',ROOT/'backend/src/recording_sd_import.py')
sd=importlib.util.module_from_spec(spec)
spec.loader.exec_module(sd)

class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.root=Path(self.temp.name)
        self.db=sqlite3.connect(':memory:')
        self.db.execute('CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)')
        self.db.execute('INSERT INTO telemetry VALUES(1,1700000000010,1700000000000,"DAQ","KG50","[9]","[]")')
        self.db.commit()
        self.manifest={'board_id':'DAQ-serial-1','boot_id':'verified-boot-1','stream_id':'raw',
            'launch_session':'test-1','config':{'recorded':True},'sensor_types':{'kg50_raw':'KG50'}}
    def tearDown(self):
        self.db.close()
        self.temp.cleanup()
    def row(self,**updates):
        result=dict(zip(sd.FIELDS,['1700000000000','100','kg50_raw','1.00000000e+00','123','2.0','1.0','network','24.0','1234','100','1000','1']))
        result.update(updates)
        return [result[k] for k in sd.FIELDS]
    def file(self,name,rows,metadata=False):
        path=self.root/name
        with path.open('w',newline='') as out:
            writer=csv.writer(out,lineterminator='\r\n')
            writer.writerow(sd.FIELDS)
            if metadata: writer.writerow(['# calibration','kg50_c0=1','kg50_tare=2'])
            writer.writerows(rows)
        return path
    def test_actual_header_duplicates_high_rate_and_idempotence(self):
        path=self.file('DAQ.csv',[self.row(),self.row(),self.row(record_sequence='2',value='3.0'),
            self.row(record_sequence='3',timestamp_ms='1700000000001',monotonic_ms='101',uptime_ticks='101')],True)
        result=sd.import_csv(self.db,path,self.manifest)
        self.assertEqual(result['samples'],4)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_samples').fetchone()[0],3)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_sample_origins').fetchone()[0],4)
        self.assertTrue(sd.import_csv(self.db,path,self.manifest)['already_imported'])
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_resolved_samples').fetchone()[0],4)
        self.assertEqual(list(sd.iter_sd_rows(self.db))[0]['record_indices'],[2,3]) # calibration record occupies index1
    def test_firmware_generated_golden(self):
        fixture=ROOT/'backend/src/test-fixtures/sd-recovery-current-firmware.csv'
        sd.import_csv(self.db,fixture,self.manifest)
        rows=list(sd.iter_sd_rows(self.db))
        self.assertEqual(rows[0]['fields']['raw_adc_code'],'-123')
        self.assertEqual(rows[0]['fields']['calibrated_value'],'-2.50000000e+00')
        self.assertEqual(rows[0]['calibration_metadata'][0]['fields'][0],'# calibration')
        self.assertEqual(rows[1]['fields']['raw_adc_code'],'-32768 2 32767')
        self.assertEqual(rows[2]['fields']['value'],'quoted, "text"\r\nsecond line')
    def test_unknown_utc_fc_vectors_multiline_and_uint64(self):
        path=self.file('FC.csv',[self.row(sensor='gyroscope_sample',value='-1.0 2.0 nan',raw_adc_code='-32768 2 32767',
            time_source='local',timestamp_ms='100',record_sequence=str(2**64-1)),
            self.row(sensor='diagnostic',value='line1,"quoted"\r\nline2',time_source='local',timestamp_ms='99',uptime_ticks='99')])
        manifest={**self.manifest,'board_id':'FC-1','stream_id':'fc'}
        sd.import_csv(self.db,path,manifest)
        rows=list(sd.iter_sd_rows(self.db))
        self.assertIsNone(rows[0]['utc_timestamp_ms'])
        self.assertEqual(rows[0]['fields']['raw_adc_code'],'-32768 2 32767')
        self.assertEqual(rows[1]['fields']['value'],'line1,"quoted"\r\nline2')
        self.assertEqual(rows[0]['fields']['record_sequence'],str(2**64-1))
    def test_exact_identity_sd_wins_retains_network_and_conflict_is_ambiguous(self):
        sd.import_csv(self.db,self.file('first.csv',[self.row()]),self.manifest)
        key=self.db.execute('SELECT identity_key FROM sd_samples').fetchone()[0]
        self.db.execute('INSERT INTO sd_network_identity VALUES(1,?,?)',(key,json.dumps({'captured_packet_metadata':True})))
        self.db.commit()
        self.assertEqual(self.db.execute('SELECT origin FROM sd_resolved_samples').fetchall(),[('sd',)])
        self.assertEqual(self.db.execute('SELECT values_json FROM telemetry').fetchone()[0],'[9]')
        sd.import_csv(self.db,self.file('conflict.csv',[self.row(value='99')]),self.manifest)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_resolved_samples').fetchone()[0],3)
        self.assertEqual(self.db.execute("SELECT SUM(ambiguous) FROM sd_resolved_samples WHERE origin='sd'").fetchone()[0],2)
    def test_duplicate_network_identity_never_suppressed(self):
        sd.import_csv(self.db,self.file('one.csv',[self.row()]),self.manifest)
        key=self.db.execute('SELECT identity_key FROM sd_samples').fetchone()[0]
        self.db.execute('INSERT INTO telemetry SELECT 2,timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json FROM telemetry')
        self.db.executemany('INSERT INTO sd_network_identity VALUES(?,?,?)',[(1,key,'{}'),(2,key,'{}')])
        self.db.commit()
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_exact_matches').fetchone()[0],0)
    def test_corruption_mixed_boot_limits_and_manifest_conflict_atomic(self):
        for name,rows in [('bad.csv',[self.row(),['broken']]),('reboot.csv',[self.row(),self.row(uptime_ticks='0',monotonic_ms='0',record_sequence='0')])]:
            with self.assertRaises(ValueError): sd.import_csv(self.db,self.file(name,rows),self.manifest)
            self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_samples').fetchone()[0],0)
        path=self.file('good.csv',[self.row(),self.row(record_sequence='2')])
        with self.assertRaises(ValueError): sd.import_csv(self.db,path,self.manifest,max_rows=1)
        sd.import_csv(self.db,path,self.manifest)
        with self.assertRaises(ValueError): sd.import_csv(self.db,path,{**self.manifest,'config':{}})
        with self.assertRaises(ValueError): sd.import_csv(self.db,path,self.manifest,max_bytes=1)
    def test_oversized_physical_line_and_column_bomb_rejected_before_unbounded_row(self):
        path=self.root/'column-bomb.csv'
        path.write_text(','.join(sd.FIELDS)+'\r\n'+','*(sd.MAX_PHYSICAL_LINE_CHARS+1))
        with self.assertRaises(ValueError):sd.import_csv(self.db,path,self.manifest)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_samples').fetchone()[0],0)
    def test_symlink_archive_and_new_derived_copy(self):
        path=self.file('plain.csv',[self.row()])
        link=self.root/'link.csv'; link.symlink_to(path)
        with self.assertRaises(OSError): sd.import_csv(self.db,link,self.manifest)
        with self.assertRaises(ValueError): sd.import_csv(self.db,self.file('archive.zip',[['PK']]),self.manifest)
        source=self.root/'original.db'
        with sd.closing(sqlite3.connect(source)) as db: self.db.backup(db)
        before=source.read_bytes()
        target=self.root/'recovered.db'
        sd.create_derived_recording(source,target)
        with sd.closing(sqlite3.connect(target)) as db: sd.import_csv(db,path,self.manifest)
        self.assertEqual(source.read_bytes(),before)
        with self.assertRaises(FileExistsError): sd.create_derived_recording(source,target)

if __name__=='__main__': unittest.main()
