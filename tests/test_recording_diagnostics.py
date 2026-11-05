import csv
import importlib.util
import io
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('recording_report_diagnostics', Path(__file__).parents[1]/'backend/src/recording_report.py')
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)

class RecordedDiagnostics(unittest.TestCase):
    def test_selected_exact_counters_named_temperature_and_unknown_are_saved_data(self):
        with tempfile.TemporaryDirectory() as directory:
            name='groundstation_recording_2026-11-03_00-00-00_000.db'
            with sqlite3.connect(Path(directory)/name) as db:
                db.execute('CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)')
                for kind,value,source in [('REC_SIDE_COUNTERS',2**64-1,1234),('REC_BOARD_DIE_TEMPERATURE',-1.23,1235),('REC_BOARD_DIE_TEMPERATURE',None,None)]:
                    db.execute('INSERT INTO telemetry VALUES(NULL,?,?,?,?,?,?)',(1700000000000,source,'GS/radio' if kind=='REC_SIDE_COUNTERS' else 'FC',kind,json.dumps([value]),json.dumps({'context':{'version':1},'counter':str(2**64-1)})))
            schema={'version':1,'streams':[{'data_type':'REC_SIDE_COUNTERS','channels':['TX packets']},{'data_type':'REC_BOARD_DIE_TEMPERATURE','channels':['MCU die temperature'],'unit':'°C'}]}
            request={'root':directory,'recording':name,'recording_diagnostic_streams':schema}
            preview=json.loads(report.run(request))
            counter=next(v for v in preview['channels'] if v['label']=='TX packets')
            temp=next(v for v in preview['channels'] if v['label']=='MCU die temperature')
            self.assertEqual(temp['unit'],'°C')
            self.assertEqual(temp['unit_source'],'recorded_diagnostic_schema_v1')
            rows=list(csv.DictReader(io.StringIO(report.run(dict(request,format='csv',channels=[counter['key']])).decode())))
            self.assertEqual(rows[0]['value'],str(2**64-1))
            rows=list(csv.DictReader(io.StringIO(report.run(dict(request,format='csv',channels=[temp['key']])).decode())))
            self.assertEqual([r['value'] for r in rows],['-1.23',''])
            self.assertEqual([r['source_timestamp_ms'] for r in rows],['1235',''])
            # Unknown diagnostic versions do not acquire guessed labels/units.
            with sqlite3.connect(Path(directory)/name) as db:
                db.execute("UPDATE telemetry SET payload_json='{}' WHERE data_type='REC_BOARD_DIE_TEMPERATURE'")
            preview=json.loads(report.run(request))
            temp=next(v for v in preview['channels'] if 'REC_BOARD_DIE_TEMPERATURE' in v['key'])
            self.assertIsNone(temp['unit'])
            self.assertNotEqual(temp['label'],'MCU die temperature')
