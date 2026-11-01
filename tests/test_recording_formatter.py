"""Recording formatter acceptance, including the ACTUAL production DAQ C helpers."""
import csv
import importlib.util
import io
import json
import sqlite3
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("recording_report", ROOT / "backend/src/recording_report.py")
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)


def data_rows(data, delimiter=","):
    # Provenance JSON is on single comment lines; scalar network values contain
    # no embedded newlines. Imported multiline escaping is tested separately.
    clean = "".join(line for line in data.decode().splitlines(keepends=True) if not line.startswith("# "))
    return list(csv.DictReader(io.StringIO(clean, newline=""), delimiter=delimiter))


class Formatter(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.name = "groundstation_recording_2026-11-01_00-00-00.db"
        self.path = self.root / self.name
        self.utc = 1793491200000
        with sqlite3.connect(self.path) as db:
            db.execute("CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)")
            for t, source, sender, kind, values in [
                (self.utc, self.utc-12, "DAQ", "KG1000", [1.25]),
                (self.utc+1, 1234, "DAQ", "KG1000", [None]),
                (self.utc+2, 1236, "DAQ", "KG1000", [2.5]),
                (self.utc+3, self.utc-9, "DAQ", "LOADCELL_WEIGHT_KG", [99.5]),
                (self.utc+4, self.utc-8, "other", "KG1000", [1.25]),
                (self.utc+5, self.utc-7, "VB", "VALVE_STATE", [1, 0, None, 1, 0, 1, 0, 1]),
            ]:
                db.execute("INSERT INTO telemetry VALUES(NULL,?,?,?,?,?,?)", (t,source,sender,kind,json.dumps(values),"[]"))
        self.request = {"root":str(self.root),"recording":self.name,"format":"daq_csv",
                        "channels":[json.dumps(["DAQ","KG1000",0],separators=(",",":"))]}

    def tearDown(self):
        self.tmp.cleanup()

    def test_exact_channel_recording_range_and_missing_values(self):
        rows = data_rows(report.run(self.request))
        self.assertEqual(len(rows),2)
        self.assertEqual([row["sensor"] for row in rows],["kg1000_network"]*2)
        self.assertEqual(float(rows[0]["value"]),1.25)
        selected = data_rows(report.run(dict(self.request,start_ms=self.utc+1,end_ms=self.utc+3)))
        self.assertEqual(len(selected),1)
        self.assertEqual(float(selected[0]["value"]),2.5)
        with self.assertRaisesRegex(ValueError,"Select one recording"):
            report.run(dict(self.request,recording=""))
        for channels in [[],["unknown"]]:
            with self.assertRaises(ValueError):report.run(dict(self.request,channels=channels))

    def test_unknown_adc_uptime_and_clock_never_fabricated(self):
        payload = report.run(self.request)
        rows = data_rows(payload)
        self.assertEqual(rows[0]["timestamp_ms"],str(self.utc-12))
        self.assertEqual(rows[0]["time_source"],"network")
        self.assertEqual(rows[1]["timestamp_ms"],"")
        self.assertEqual(rows[1]["time_source"],"unknown")
        for row in rows:
            for field in ["raw_adc_code","raw_value","calibrated_value","monotonic_ms","uptime_ticks","uptime_frequency_hz","record_sequence"]:
                self.assertEqual(row[field],"")
        provenance = [json.loads(line.split("=",1)[1]) for line in payload.decode().splitlines() if line.startswith("# recorded_sample=")]
        self.assertEqual(provenance[0]["received_timestamp_ms"],self.utc)
        self.assertEqual(provenance[0]["source_age_ms"],12)
        self.assertEqual(provenance[1]["source_timestamp_ms"],1236)
        self.assertIsNone(provenance[1]["source_age_ms"])

    def test_tsv_has_actual_tabs_and_does_not_cross_source_dedupe(self):
        preview=json.loads(report.run(dict(self.request,format="preview",channels=None)))
        keys=[c["key"] for c in preview["channels"] if c["data_type"]=="KG1000"]
        payload=report.run(dict(self.request,format="daq_tsv",channels=keys))
        self.assertIn(b"timestamp_ms\tmonotonic_ms",payload)
        rows=data_rows(payload,"\t")
        self.assertEqual(len(rows),3)
        self.assertTrue(all(set(row)==set(report.DAQ_COLUMNS) for row in rows))

    def test_raw_derived_units_and_valve_labels_remain_explicit(self):
        request=dict(self.request,format="preview",channels=None,
                     channel_metadata=[{"data_type":"KG1000","index":0,"unit":"V"}])
        preview=json.loads(report.run(request))
        raw=next(c for c in preview["channels"] if c["sender"]=="DAQ" and c["data_type"]=="KG1000")
        self.assertEqual(raw["value_kind"],"recorded_raw_input")
        self.assertEqual(raw["unit"],"V")
        self.assertEqual(raw["finite_count"],2)
        derived=next(c for c in preview["channels"] if c["data_type"]=="LOADCELL_WEIGHT_KG")
        self.assertEqual(derived["value_kind"],"recorded_derived")
        valve=next(c for c in preview["channels"] if c["data_type"]=="VALVE_STATE" and c["index"]==0)
        payload=report.run(dict(self.request,channels=[valve["key"]]))
        self.assertIn(b'"state_label":"Open"',payload)
        unknown=next(c for c in preview["channels"] if c["data_type"]=="VALVE_STATE" and c["index"]==7)
        self.assertIn(b'"state_label":""',report.run(dict(self.request,channels=[unknown["key"]])))

    def test_higher_recorded_precision_is_not_coerced_to_binary32(self):
        self.assertEqual(report.daq_float(1.1234567891234567),repr(1.1234567891234567))
        self.assertEqual(report.daq_float(2**60+3),str(2**60+3))
        fields=dict.fromkeys(report.DAQ_COLUMNS,"")
        fields.update(sensor='sensor,\t"name"',value='a,\t"quoted"\nsecond line',time_source="local",timestamp_ms="1234",monotonic_ms="1234")
        for delimiter in [",","\t"]:
            text=delimiter.join(report.DAQ_COLUMNS)+"\r\n"+report.daq_record(fields,delimiter)
            self.assertEqual(next(csv.DictReader(io.StringIO(text,newline=""),delimiter=delimiter)),fields)

    def test_sd_timeline_is_not_fabricated_receipt_and_partial_vectors_are_rejected(self):
        fields=dict.fromkeys(report.DAQ_COLUMNS,"")
        fields.update(timestamp_ms=str(self.utc),monotonic_ms="345",sensor="vector",value="1 2",time_source="network",uptime_ticks="345",uptime_frequency_hz="1000",record_sequence="9")
        origin={"origin":"sd","received_timestamp_ms":None,"source_timestamp_ms":self.utc,
                "utc_timestamp_ms":self.utc,"sd_sample_id":42,"fields":fields,"mapping":{"indices":[0,1]}}
        with sqlite3.connect(self.path) as db:
            db.execute("CREATE TABLE sd_replay_telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT,provenance_json TEXT)")
            db.execute("INSERT INTO sd_replay_telemetry VALUES(1,?,?,?,?,?,?,?)",(self.utc,self.utc,"DAQ","VECTOR","[1,2]","[]",json.dumps(origin)))
        one=json.dumps(["DAQ","VECTOR",0],separators=(",",":"))
        two=json.dumps(["DAQ","VECTOR",1],separators=(",",":"))
        with self.assertRaisesRegex(ValueError,"whole physical SD records"):
            report.run(dict(self.request,channels=[one]))
        selected=list(csv.DictReader(io.StringIO(report.run(dict(self.request,format="csv",channels=[one])).decode())))
        self.assertEqual(selected[0]["received_timestamp_ms"],"")
        self.assertEqual(selected[0]["value"],"1")
        metadata=json.loads(selected[0]["regression_json"])["sample_provenance"]
        self.assertNotIn("fields",metadata)
        all_fields=data_rows(report.run(dict(self.request,channels=[one,two])))
        self.assertEqual(all_fields,[fields])
        from openpyxl import load_workbook
        from pypdf import PdfReader
        book=load_workbook(io.BytesIO(report.run(dict(self.request,format="xlsx",channels=[one]))))
        sheet=book["Channel 1"]
        self.assertEqual(sheet.cell(1,1).value,"Timeline ms")
        self.assertEqual(sheet.cell(2,1).value,self.utc)
        self.assertEqual(sheet.cell(1,7).value,"Received ms")
        self.assertIsNone(sheet.cell(2,7).value)
        book.close()
        pdf=PdfReader(io.BytesIO(report.run(dict(self.request,format="pdf",channels=[one]))))
        text="\n".join(page.extract_text() for page in pdf.pages)
        self.assertIn("canonical replay",text)
        self.assertIn("Timeline ms",text)
        with sqlite3.connect(self.path) as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM telemetry").fetchone()[0],6)

    def test_actual_daq_c_formatter_golden_fields_and_float_precision(self):
        daq = next(parent/"DAQ-Board" for parent in Path(__file__).resolve().parents if (parent/"DAQ-Board/Core/Inc/sd_record_format.h").is_file())
        program=r'''
#include "sd_record_format.h"
#include "sd_raw_record_format.h"
#include <stdio.h>
int main(void) {
 char line[1024],value[24];sd_raw_format_cache_t cache={0};
 fputs(SD_CSV_HEADER,stdout);
 sd_format_float(value,1.25f);
 sd_format_csv_record(line,sizeof(line),1793491200000ULL,4294967297ULL,"kg1000_network",value,42);fputs(line,stdout);
 sd_format_csv_record(line,sizeof(line),0,4294967298ULL,"Message","a,\"quoted\"\nsecond line",43);fputs(line,stdout);
 sd_format_csv_raw_record(line,sizeof(line),1793491200001ULL,4294967299ULL,"mcp3564r_raw",-123,1.25f,250.0f,26.125f,-5,44,&cache);fputs(line,stdout);
 const uint32_t bits[]={0,0x80000000U,1,0x7f7fffffU,0x00800000U,0x3eaaaaabU};
 fputs("__FLOATS__\n",stdout);
 for (unsigned i=0;i<sizeof(bits)/sizeof(bits[0]);++i) {float v;memcpy(&v,&bits[i],4);sd_format_float(value,v);printf("%u:%s\n",bits[i],value);}
 return 0;
}
'''
        with tempfile.TemporaryDirectory() as temporary:
            binary=Path(temporary)/"actual-daq-formatter"
            subprocess.run(["cc","-std=c11","-O2","-I",str(daq/"Core/Inc"),"-x","c","-","-o",str(binary)],input=program.encode(),check=True)
            golden=subprocess.check_output([str(binary)])
        golden,floats=golden.split(b"__FLOATS__\n",1)
        for line in floats.decode().splitlines():
            bits,expected=line.split(":",1)
            value=struct.unpack("<f",struct.pack("<I",int(bits)))[0]
            self.assertEqual(report.daq_float(value),expected)
        actual=list(csv.DictReader(io.StringIO(golden.decode(),newline="")))
        self.assertEqual(list(actual[0]),list(report.DAQ_COLUMNS))
        fields="".join(report.daq_record(row) for row in actual).encode()
        self.assertEqual(fields,golden.split(b"\r\n",1)[1])
        self.assertEqual(report.daq_float(1.25),actual[0]["value"])
        restored=report.export_daq_sd_rows([{"sample_id":1,"utc_timestamp_ms":None,"fields":actual[1],"ambiguous":False}])
        self.assertEqual(data_rows(restored)[0],actual[1])


if __name__ == "__main__": unittest.main()
