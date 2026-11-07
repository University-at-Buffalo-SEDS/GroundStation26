import test_recording_sd_import as fixture
ROOT,sd=fixture.ROOT,fixture.sd
import json
import sqlite3
import unittest

# Same namespace composition as the actual embedded Python HTTP worker.
exec(compile((ROOT/'backend/src/recording_sd_resolve.py').read_text(),'recording_sd_resolve.py','exec'),sd.__dict__)

class CoverageTests(unittest.TestCase):
    row=fixture.RecoveryTests.row
    file=fixture.RecoveryTests.file
    tearDown=fixture.RecoveryTests.tearDown
    def manifest(self,boot='boot1'):
        schema={'types':[{'name':'KG50','element':{'kind':'Static','data_type':'Float32','count':1}}]}
        return {**self.manifest_base,'boot_id':boot,'stream_id':'telemetry',
            'mapping_profile':'daq-20261101','firmware_head':sd.DAQ_HEAD,'selected_sensors':['kg50_network'],
            'config':{'schema':schema},'assignment':{'sender_id':'DAQ','boot_and_session_verified':True,
                'units_axes_calibration_verified':True,'start_utc_ms':1700000000000,'end_utc_ms':1700000010000,'max_gap_ms':20}}
    def setUp(self):
        fixture.RecoveryTests.setUp(self);self.manifest_base=self.manifest;del self.manifest
    def test_coverage_prefers_highrate_sd_and_preserves_sequence_and_time_gaps(self):
        base=1700000000000
        self.db.execute('DELETE FROM telemetry')
        self.db.executemany('INSERT INTO telemetry VALUES(?,?,?,?,?,?,?)',[(i+1,base+50000,base+t,'DAQ','KG50','[99]','[]') for i,t in enumerate([2,7,15,100])])
        self.db.execute('INSERT INTO telemetry VALUES(9,?,?,"DAQ","KG50","[777]","[]")',(base+3,3))
        self.db.commit()
        rows=[self.row(sensor='kg50_network',value=str(seq),timestamp_ms=str(base+t),uptime_ticks=str(100+t),monotonic_ms=str(100+t),record_sequence=str(seq)) for seq,t in [(1,0),(2,5),(3,10),(5,20),(6,30),(7,200)]]
        sd.import_csv(self.db,self.file('rate.csv',rows),self.manifest())
        summary=sd.resolve_recording(self.db)
        self.assertEqual(summary['network_suppressed_in_resolved_view'],2)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM main.telemetry').fetchone()[0],5)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_replay_telemetry').fetchone()[0],8)
        resolved=list(self.db.execute('SELECT timestamp_ms,values_json,provenance_json FROM sd_replay_telemetry ORDER BY id'))
        self.assertTrue(any(t==base+15 and values=='[99]' for t,values,_ in resolved))
        self.assertFalse(any(t==base+50000 for t,_,_ in resolved))
        for _,_,provenance in resolved:
            p=json.loads(provenance)
            if p['origin']=='sd':self.assertIsNone(p['received_timestamp_ms']);self.assertIn('fields',p)
    def test_requires_exact_format_schema_and_verified_assignment(self):
        sd.import_csv(self.db,self.file('safe.csv',[self.row(sensor='kg50_network')]),self.manifest())
        for field,value in [('firmware_head','old'),('config',{'schema':{}}),('assignment',{})]:
            bad=self.manifest();bad[field]=value
            with self.assertRaises(ValueError):sd._resolution_manifest(bad)
    def test_unknown_sd_utc_and_corrupt_values_never_replace_network(self):
        path=self.file('local.csv',[self.row(sensor='kg50_network',time_source='local',timestamp_ms='100')])
        sd.import_csv(self.db,path,self.manifest())
        summary=sd.resolve_recording(self.db)
        self.assertEqual(summary['network_suppressed_in_resolved_view'],0)
        self.assertEqual(summary['unresolved_samples'],1)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_samples').fetchone()[0],1)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_replay_telemetry').fetchone()[0],1)

if __name__=='__main__':unittest.main()
