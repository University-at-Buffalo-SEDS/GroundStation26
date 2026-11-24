"""SEDS-SD format block: write/parse, selection by (format, version), legacy pins."""
import test_recording_sd_import as fixture
ROOT,sd=fixture.ROOT,fixture.sd
import csv
from contextlib import closing
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import unittest

exec(compile((ROOT/'backend/src/recording_sd_resolve.py').read_text(),'recording_sd_resolve.py','exec'),sd.__dict__)

BASE_UTC=1800000000000
COLUMNS='# seds-sd-columns,u64:ms,u64:ms,label,vec,vec,vec,vec,clock,f32:degC,i32:code,u64:tick,u32:Hz,u64:seq'
DAQ_RECORDS=[
    '# seds-sd-records,kg1000_v:a=code:r=V:c=kg@1000000,kg50_v:a=code:r=V:c=kg@1000000,'
    'kg50_p6_v:a=code:r=V:c=V@1000000,chamber_pressure_v:a=code:r=V:c=psig@1000000',
    '# seds-sd-records,input_voltage_v:a=code:r=V:c=V,input_current_a:a=code:r=V:c=A,'
    'dac_ch1_*:v=V,dac_ch2_*:v=V,valve_pressure_psi:a=code:v=psi,Message:v=text',
]
# DAQ SD format v4 exactly as DAQ-Board Core/Inc/sd_format.h writes it
# (SD_FORMAT_SCHEMA bc9e710a): MCP3564R raw records declare value=ovr, and
# vdda_v is new.
DAQ_V4_SCHEMA='bc9e710a'
DAQ_V4_RECORDS=[
    '# seds-sd-records,kg1000_v:v=ovr:a=code:r=V:c=kg@1000000,kg50_v:v=ovr:a=code:r=V:c=kg@1000000,kg50_p6_v:v=ovr:a=code:r=V:c=V@1000000,chamber_pressure_v:v=ovr:a=code:r=V:c=psig@1000000,excitation_v:v=ovr:a=code:r=V:c=V@1000000',
    '# seds-sd-records,mcp3564r_ch2_v:v=ovr:a=code:r=V:c=V@1000000,mcp3564r_ch3_v:v=ovr:a=code:r=V:c=V@1000000,mcp3564r_ch4_v:v=ovr:a=code:r=V:c=V@1000000,mcp3564r_ch5_v:v=ovr:a=code:r=V:c=V@1000000,mcp3564r_ch6_v:v=ovr:a=code:r=V:c=V@1000000,mcp3564r_ch7_v:v=ovr:a=code:r=V:c=V@1000000',
    '# seds-sd-records,input_voltage_v:a=code:r=V:c=V,input_current_a:a=code:r=V:c=A,vdda_v:a=code:r=V:c=V,valve_pressure_psi:v=psi:a=code:r=code:c=psi',
    '# seds-sd-records,adc1_aux_v:v=V,analog_in_1_v:v=V,analog_in_2_v:v=V,analog_in_3_v:v=V,analog_in_4_v:v=V,analog_in_5_v:v=V,analog_in_6_v:v=V,analog_in_7_v:v=V,analog_in_8_v:v=V',
    '# seds-sd-records,isense1_v:v=V,isense2_v:v=V,isense1_a:v=A,isense2_a:v=A,vmon_v:v=V,imon_v:v=V,analog_out_1_v:v=V,analog_out_2_v:v=V,kg1000_window_code:v=code,kg1000_window_v:v=V,mcp3564r_temp_c:v=degC,kg50_window_v:v=V',
    '# seds-sd-records,dac_ch1_active:v=1,dac_ch1_waveform:v=code,dac_ch1_amplitude_v:v=V,dac_ch1_offset_v:v=V,dac_ch1_frequency_hz:v=Hz,dac_ch1_steps:v=1,dac_ch1_dwell_ms:v=ms,dac_ch1_sequence:v=1',
    '# seds-sd-records,dac_ch1_result:v=code,dac_ch1_off_reason:v=code,dac_ch1_update_hz:v=Hz,dac_ch1_samples:v=1,dac_ch1_duty:v=1,dac_ch1_output_v:v=V,dac_ch1_readback_v:v=V',
    '# seds-sd-records,dac_ch2_active:v=1,dac_ch2_waveform:v=code,dac_ch2_amplitude_v:v=V,dac_ch2_offset_v:v=V,dac_ch2_frequency_hz:v=Hz,dac_ch2_steps:v=1,dac_ch2_dwell_ms:v=ms,dac_ch2_sequence:v=1',
    '# seds-sd-records,dac_ch2_result:v=code,dac_ch2_off_reason:v=code,dac_ch2_update_hz:v=Hz,dac_ch2_samples:v=1,dac_ch2_duty:v=1,dac_ch2_output_v:v=V,dac_ch2_readback_v:v=V',
    '# seds-sd-records,seds_packet:v=text',
]
FC_RECORDS=[
    '# seds-sd-records,barometer_sample:v=Pa;degC;m:r=u24;u24;nan,BARO:v=m;Pa;degC,IMU:v=m/s2*3;deg/s*3,'
    'accelerometer_sample:v=m/s2*3:r=i16*3,gyroscope_sample:v=deg/s*3:r=i16*3',
    '# seds-sd-records,EKF:v=1*4;m;m/s,DKF:v=deg*2;m;m/s,pyro_*:v=text,Message:v=text',
]


def format_block(fmt,version,board,firmware,tick_hz,records,schema=None):
    """Header lines 2.. as a board writes them (spec A), CRLF terminated."""
    if schema is None:
        schema=hashlib.sha256(''.join(line+'\r\n' for line in [COLUMNS]+records).encode()).hexdigest()[:8]
    first=f'# seds-sd,format={fmt},version={version},board={board},firmware={firmware},tick_hz={tick_hz},schema={schema}'
    return ''.join(line+'\r\n' for line in [first,COLUMNS]+records)


def daq_block(firmware='0123456789ab',version=3,records=DAQ_RECORDS,**kw):
    return format_block('daq',version,'DAQ',firmware,1000,records,**kw)


def fc_block(firmware='fedcba987654-dirty',version=2,records=FC_RECORDS):
    return format_block('fc',version,'FC',firmware,1000,records)


class FormatBase(unittest.TestCase):
    row=fixture.RecoveryTests.row
    tearDown=fixture.RecoveryTests.tearDown
    def setUp(self):
        fixture.RecoveryTests.setUp(self)
        self.db.execute('DELETE FROM telemetry');self.db.commit()
    def write(self,name,block,rows,extra='# calibration,kg1000_slope=2.00000000e+00,loadcell_report=median_bin\r\n'):
        path=self.root/name
        with path.open('w',newline='') as out:
            out.write(','.join(sd.FIELDS)+'\r\n'+block+extra)
            csv.writer(out,lineterminator='\r\n').writerows(rows)
        return path
    def make_manifest(self,selected,kinds,**updates):
        schema={'types':[{'name':kind,'element':{'kind':'Static','data_type':'Float32','count':count}} for kind,count in kinds]}
        manifest={'board_id':'board-1','boot_id':'boot1','stream_id':'raw','launch_session':'fire-1',
            'selected_sensors':list(selected),'config':{'schema':schema},'sensor_types':{},
            'assignment':{'sender_id':'DAQ','boot_and_session_verified':True,'units_axes_calibration_verified':True,
                          'start_utc_ms':BASE_UTC-10000,'end_utc_ms':BASE_UTC+600000,'max_gap_ms':60}}
        manifest.update(updates)
        return {k:v for k,v in manifest.items() if v is not None}
    def adc(self,name,uptime_us,volts,calibrated,seq):
        return self.row(sensor=name,timestamp_ms=str(BASE_UTC+uptime_us//1000),monotonic_ms=str(uptime_us//1000),
            uptime_ticks=str(uptime_us),uptime_frequency_hz='1000000',value='',raw_value=repr(volts),
            calibrated_value=repr(calibrated),record_sequence=str(seq))
    def scalar(self,name,uptime_ms,value,seq,**kw):
        return self.row(sensor=name,timestamp_ms=str(BASE_UTC+uptime_ms),monotonic_ms=str(uptime_ms),uptime_ticks=str(uptime_ms),
            uptime_frequency_hz='1000',value=value,raw_value='',calibrated_value='',record_sequence=str(seq),**kw)
    def daq_rows(self):
        rows=[];seq=0
        for i in range(200):  # 1 kHz frames: KG1000, the selected KG50, a DAC row
            t=1000000+i*1000
            seq+=1;rows.append(self.adc('kg1000_v',t+288,0.001*i,0.5*i,seq))
            seq+=1;rows.append(self.adc('kg50_v' if i<100 else 'kg50_p6_v',t+620,0.002*i,0.1*i,seq))
            seq+=1;rows.append(self.scalar('dac_ch1_v',t//1000,'1.5',seq))
        for t in (1050,1150):
            seq+=1;rows.append(self.row(sensor='input_voltage_v',timestamp_ms=str(BASE_UTC+t),monotonic_ms=str(t),uptime_ticks=str(t),
                uptime_frequency_hz='1000',value='',raw_value='1.29',calibrated_value='14.2',record_sequence=str(seq)))
            seq+=1;rows.append(self.row(sensor='input_current_a',timestamp_ms=str(BASE_UTC+t),monotonic_ms=str(t),uptime_ticks=str(t),
                uptime_frequency_hz='1000',value='',raw_value='0.24',calibrated_value='0.5',record_sequence=str(seq)))
        return rows
    def resolved(self):
        return [(t,kind,json.loads(v),json.loads(p)) for t,kind,v,p in self.db.execute(
            'SELECT timestamp_ms,data_type,values_json,provenance_json FROM sd_replay_telemetry ORDER BY id')]


DAQ_KINDS=[('KG1000',1),('KG50',1),('LOADCELL_WEIGHT_KG',1),('LOADCELL_50_WEIGHT_KG',1),('DAQ_POWER_MONITOR',4)]
FC_KINDS=[('BAROMETER_DATA',3),('IMU_DATA',6),('ACCEL_DATA',3),('GYRO_DATA',3),('ASCENT_STATE',6),('DESCENT_STATE',4)]


class FormatBlockParseTests(FormatBase):
    def test_daq_v3_header_round_trip(self):
        path=self.write('DAQ_0001.CSV',daq_block(),self.daq_rows())
        block=sd.read_format_block(path)
        self.assertEqual((block['format'],block['version'],block['board'],block['firmware'],block['tick_hz']),('daq',3,'DAQ','0123456789ab',1000))
        self.assertFalse(block['legacy'])
        self.assertEqual(block['columns'],sd.SD_COLUMN_TYPES)
        kg=block['records']['kg1000_v']
        self.assertEqual(kg['columns'],{'raw_adc_code':['code'],'raw_value':['V'],'calibrated_value':['kg']})
        self.assertEqual(kg['tick_hz'],1000000)
        self.assertEqual(block['records']['input_current_a']['tick_hz'],1000,'default tick_hz')
        self.assertEqual(block['records']['Message']['columns'],{'value':['text']})
        self.assertIs(sd.declared_record(block,'dac_ch1_level_v'),block['records']['dac_ch1_*'])
        self.assertIsNone(sd.declared_record(block,'dac_ch3_v'))
        # The same rows the importer stores parse to the same block.
        result=sd.import_csv(self.db,path,self.make_manifest([],[]))
        self.assertEqual(result['sd_format'],{'format':'daq','version':3,'board':'DAQ','firmware':'0123456789ab',
                                              'schema':block['schema'],'tick_hz':1000,'legacy':False})
        metadata=json.loads(self.db.execute('SELECT metadata_json FROM sd_imports').fetchone()[0])
        self.assertEqual(sd.format_block_from_metadata(metadata),block)

    def test_fc_v2_header_round_trip(self):
        block=sd.parse_format_block(fc_block().encode().splitlines(keepends=True)+[b'1,2,IMU,...\r\n'])
        self.assertEqual((block['format'],block['version'],block['board']),('fc',2,'FC'))
        self.assertEqual(block['records']['IMU']['columns']['value'],['m/s2']*3+['deg/s']*3)
        self.assertEqual(block['records']['barometer_sample']['columns'],{'value':['Pa','degC','m'],'raw_value':['u24','u24','nan']})
        self.assertEqual(block['records']['EKF']['columns']['value'],['1']*4+['m','m/s'])
        self.assertTrue(block['records']['pyro_*']['prefix'])
        self.assertEqual(sd.declared_record(block,'pyro_drogue')['columns'],{'value':['text']})

    def test_schema_ignores_lines_one_and_two(self):
        a=sd.parse_format_block(daq_block(firmware='aaaaaaaaaaaa').splitlines())
        b=sd.parse_format_block(daq_block(firmware='unknown',version=7).splitlines())
        self.assertEqual(a['schema'],b['schema'])
        # LF line endings (a copied file) hash the same: each line is text + CRLF.
        self.assertEqual(sd.parse_format_block(daq_block().replace('\r\n','\n').splitlines(keepends=True))['schema'],a['schema'])

    def test_headerless_is_legacy(self):
        self.assertIsNone(sd.parse_format_block(['# calibration,kg50_c0=1','1,2,3']))
        self.assertIsNone(sd.parse_format_block([]))

    def test_damaged_hash_is_refused(self):
        damaged=daq_block().replace('kg50_p6_v:a=code:r=V:c=V','kg50_p6_v:a=code:r=V:c=kg')
        with self.assertRaisesRegex(ValueError,'SD format header damaged: schema hash mismatch'):
            sd.parse_format_block(damaged.splitlines())
        path=self.write('damaged.csv',damaged,self.daq_rows())
        with self.assertRaisesRegex(ValueError,'schema hash mismatch'):sd.read_format_block(path)
        with self.assertRaisesRegex(ValueError,'schema hash mismatch'):sd.import_csv(self.db,path,self.make_manifest([],[]))
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM sd_imports').fetchone()[0],0)

    def test_malformed_blocks_are_refused(self):
        good=daq_block().splitlines()
        for lines in ([good[0]]+good[2:],               # no columns line
                      good[:2],                          # no records line
                      [good[0].replace(',schema=','x,schema=').replace('version=3','version=three')]+good[1:],
                      ['# calibration,x=1']+good):       # block not right after the header
            with self.assertRaisesRegex(ValueError,'SD format header damaged'):sd.parse_format_block(lines)


class FormatSelectionTests(FormatBase):
    def test_known_daq_v3_with_any_firmware_resolves_without_pins(self):
        firmware='c0ffee000000-dirty'
        path=self.write('DAQ_0001.CSV',daq_block(firmware=firmware),self.daq_rows())
        manifest=self.make_manifest(['kg1000_v','kg50_v','kg1000_kg','input_supply'],DAQ_KINDS)
        self.assertNotIn('mapping_profile',manifest);self.assertNotIn('firmware_head',manifest)
        _,profile,_=sd._resolution_manifest(manifest,sd.read_format_block(path))
        self.assertEqual(profile['sd_format']['profile'],'daq-v3')
        result=sd.import_csv(self.db,path,manifest)
        self.assertEqual(result['sd_format']['firmware'],firmware)
        summary=sd.resolve_recording(self.db)
        self.assertEqual(summary['sd_formats'][0]['format'],'daq')
        self.assertEqual(summary['sd_formats'][0]['version'],3)
        self.assertIs(summary['sd_formats'][0]['legacy'],False)
        rows=self.resolved()
        kinds={kind for _,kind,_,_ in rows}
        self.assertEqual(kinds,{'KG1000','KG50','LOADCELL_WEIGHT_KG','DAQ_POWER_MONITOR'})
        for _,kind,_,p in rows:
            self.assertEqual(p['mapping']['profile'],'daq-v3')
            self.assertEqual(p['mapping']['source_evidence'],firmware)
            self.assertEqual(p['mapping']['sd_format']['schema'],result['sd_format']['schema'])
        kg1000=[(t,v,p) for t,kind,v,p in rows if kind=='KG1000']
        self.assertEqual(len(kg1000),9,'20 ms bins of 200 frames; the last bin stays open')
        self.assertEqual(kg1000[0][2]['mapping']['derived'],'median_bin')
        kg50=[p['mapping']['source_sensor'] for _,kind,_,p in rows if kind=='KG50']
        self.assertIn('kg50_p6_v',kg50,'declared P6 input is read')
        power=[v for _,kind,v,_ in rows if kind=='DAQ_POWER_MONITOR']
        self.assertEqual(power,[[1.29,0.24,14.2,0.5]]*2)
        self.assertEqual(summary['unresolved_samples'],0,'dac_ch* rows are ignored, not unresolved')

    def test_known_fc_v2_resolves_accel_in_m_s2(self):
        rows=[]
        def vec(sensor,t,value,seq):
            return self.row(sensor=sensor,timestamp_ms=str(BASE_UTC+t),monotonic_ms=str(t),uptime_ticks=str(t),uptime_frequency_hz='1000',
                            value=value,raw_adc_code='',raw_value='',calibrated_value=value,record_sequence=str(seq),adc_temperature_c='',adc_temperature_code='')
        for i in range(5):
            rows.append(vec('barometer_sample',100+i,'101325.0 21.5 12.0',3*i+1))
            rows.append(vec('accelerometer_sample',100+i,'0.1 0.2 9.80665',3*i+2))
            rows.append(vec('IMU',100+i,'0.1 0.2 9.80665 1.0 2.0 3.0',3*i+3))
        path=self.write('irec0001.csv',fc_block(),rows,extra='')
        manifest=self.make_manifest(['barometer_sample','accelerometer_sample','IMU'],FC_KINDS)
        manifest['assignment']['sender_id']='FC'
        sd.import_csv(self.db,path,manifest)
        sd.resolve_recording(self.db)
        got={kind:(v,p) for _,kind,v,p in self.resolved()}
        self.assertEqual(got['BAROMETER_DATA'][0],[12.0,101325.0,21.5],'reordered to alt, pressure, temperature')
        self.assertEqual(got['ACCEL_DATA'][0],[0.1,0.2,9.80665])
        self.assertEqual(got['ACCEL_DATA'][1]['mapping']['units'],['m/s2']*3)
        self.assertEqual(got['IMU_DATA'][1]['mapping']['units'],['m/s2']*3+['deg/s']*3)
        self.assertEqual(got['IMU_DATA'][1]['mapping']['profile'],'fc-v2')

    def test_unknown_version_and_format_are_refused_with_the_supported_list(self):
        manifest=self.make_manifest(['kg1000_v'],DAQ_KINDS)
        expected=('SD file declares format daq v5; this ground station reads daq v3, daq v4, fc v2 '
                  '(and legacy headerless daq v1/v2, fc v1). Update the ground station.')
        with self.assertRaises(ValueError) as caught:
            sd._resolution_manifest(manifest,sd.parse_format_block(daq_block(version=5).splitlines()))
        self.assertEqual(str(caught.exception),expected)
        with self.assertRaisesRegex(ValueError,'declares format rf v1'):
            sd._resolution_manifest(manifest,sd.parse_format_block(format_block('rf',1,'RF','x',1000,DAQ_RECORDS).splitlines()))
        with self.assertRaisesRegex(ValueError,'names board FC for format daq'):
            sd._resolution_manifest(manifest,sd.parse_format_block(format_block('daq',3,'FC','x',1000,DAQ_RECORDS).splitlines()))

    def test_conflicting_mapping_profile_is_refused(self):
        block=sd.parse_format_block(daq_block().splitlines())
        for claimed in ('daq-20261123','fc-v2','fc-20261101'):
            with self.assertRaisesRegex(ValueError,'conflicts with the SD file.s declared format daq v3'):
                sd._resolution_manifest(self.make_manifest(['kg1000_v'],DAQ_KINDS,mapping_profile=claimed,firmware_head=sd.DAQ_SD_HEAD_US),block)
        _,profile,_=sd._resolution_manifest(self.make_manifest(['kg1000_v'],DAQ_KINDS,mapping_profile='daq-v3',firmware_head='abc'),block)
        self.assertEqual(profile['sd_format']['manifest_firmware_head'],'abc')

    def test_undeclared_or_mismatched_labels_refuse_only_their_mapping(self):
        records=[DAQ_RECORDS[0].replace('kg1000_v:a=code:r=V:c=kg@1000000','kg1000_v:a=code:r=V:c=V@1000000'),
                 '# seds-sd-records,input_voltage_v:a=code:r=V:c=V,dac_ch1_*:v=V']
        block=sd.parse_format_block(daq_block(records=records).splitlines())
        sd._resolution_manifest(self.make_manifest(['kg1000_v','kg50_kg'],DAQ_KINDS),block)
        with self.assertRaisesRegex(ValueError,'Mapping input_supply needs SD record input_current_a declared'):
            sd._resolution_manifest(self.make_manifest(['input_supply'],DAQ_KINDS),block)
        with self.assertRaisesRegex(ValueError,r'Mapping kg1000_kg needs kg1000_v calibrated_value=kg; the file declares calibrated_value=V'):
            sd._resolution_manifest(self.make_manifest(['kg1000_kg'],DAQ_KINDS),block)
        slow=sd.parse_format_block(daq_block(records=[DAQ_RECORDS[0].replace('kg1000_v:a=code:r=V:c=kg@1000000','kg1000_v:r=V:c=kg'),DAQ_RECORDS[1]]).splitlines())
        with self.assertRaisesRegex(ValueError,'Mapping kg1000_v needs kg1000_v at 1000000 Hz ticks; the file declares 1000 Hz'):
            sd._resolution_manifest(self.make_manifest(['kg1000_v'],DAQ_KINDS),slow)
        narrow=sd.parse_format_block(fc_block(records=[FC_RECORDS[0].replace('IMU:v=m/s2*3;deg/s*3','IMU:v=g*3;deg/s*3'),FC_RECORDS[1]]).splitlines())
        with self.assertRaisesRegex(ValueError,'Mapping IMU needs IMU value=m/s2;m/s2;m/s2;deg/s;deg/s;deg/s'):
            sd._resolution_manifest(self.make_manifest(['IMU'],FC_KINDS),narrow)

    def test_undeclared_optional_p6_rows_are_ignored(self):
        records=[DAQ_RECORDS[0].replace(',kg50_p6_v:a=code:r=V:c=V@1000000',''),DAQ_RECORDS[1]]
        path=self.write('DAQ_0002.CSV',daq_block(records=records),self.daq_rows())
        manifest=self.make_manifest(['kg50_v'],DAQ_KINDS)
        _,profile,_=sd._resolution_manifest(manifest,sd.read_format_block(path))
        self.assertEqual(profile['sources']['kg50_v'],('kg50_v',))
        sd.import_csv(self.db,path,manifest);sd.resolve_recording(self.db)
        self.assertEqual({p['mapping']['source_sensor'] for _,kind,_,p in self.resolved() if kind=='KG50'},{'kg50_v'})


class DaqV4Tests(FormatBase):
    """DAQ SD format v4: the over-range flag in value (v=ovr) and vdda_v."""
    def adc_v4(self,name,uptime_us,volts,calibrated,seq,over=0):
        row=self.adc(name,uptime_us,volts,calibrated,seq)
        if over:
            row[3]=str(over);row[6]='inf' if over>0 else '-inf'   # firmware: the true code, linear raw_value, +-inf calibrated
        return row
    def supply(self,name,t,raw,calibrated,seq):
        return self.row(sensor=name,timestamp_ms=str(BASE_UTC+t),monotonic_ms=str(t),uptime_ticks=str(t),
            uptime_frequency_hz='1000',value='',raw_value=raw,calibrated_value=calibrated,record_sequence=str(seq))
    def write_v4(self,rows):
        return self.write('DAQ_0004.CSV',daq_block(version=4,records=DAQ_V4_RECORDS),rows)
    def resolve_v4(self,rows,selected,**updates):
        path=self.write_v4(rows)
        manifest=self.make_manifest(selected,DAQ_KINDS,**updates)
        result=sd.import_csv(self.db,path,manifest)
        return result,sd.resolve_recording(self.db)
    def replay_rows(self,kind):
        return [(t,v,o,json.loads(p)) for t,v,o,p in self.db.execute(
            'SELECT timestamp_ms,values_json,over_range,provenance_json FROM sd_replay_telemetry WHERE data_type=? ORDER BY id',(kind,))]

    def test_v4_block_is_accepted_and_hash_checked(self):
        block=sd.parse_format_block(daq_block(version=4,records=DAQ_V4_RECORDS).splitlines())
        self.assertEqual(block['schema'],DAQ_V4_SCHEMA,'same hash as the DAQ firmware header')
        self.assertEqual((block['format'],block['version']),('daq',4))
        self.assertEqual(block['records']['kg1000_v']['columns'],
                         {'value':['ovr'],'raw_adc_code':['code'],'raw_value':['V'],'calibrated_value':['kg']})
        self.assertEqual(block['records']['vdda_v']['columns'],{'raw_adc_code':['code'],'raw_value':['V'],'calibrated_value':['V']})
        self.assertEqual(block['records']['vdda_v']['tick_hz'],1000)
        for label in sd.DAQ_OVER_RANGE_LABELS:
            self.assertEqual(block['records'][label]['columns']['value'],['ovr'],label)
        _,profile,_=sd._resolution_manifest(self.make_manifest(['kg1000_v','kg50_v','kg1000_kg','kg50_kg','input_supply'],DAQ_KINDS),block)
        self.assertEqual(profile['sd_format']['profile'],'daq-v4')
        # A v4 file whose load-cell records lack value=ovr is not v4.
        stripped=[line.replace(':v=ovr','') for line in DAQ_V4_RECORDS]
        with self.assertRaisesRegex(ValueError,'Mapping kg1000_v needs kg1000_v value=ovr'):
            sd._resolution_manifest(self.make_manifest(['kg1000_v'],DAQ_KINDS),sd.parse_format_block(daq_block(version=4,records=stripped).splitlines()))
        damaged=daq_block(version=4,records=DAQ_V4_RECORDS,schema=DAQ_V4_SCHEMA).replace('vdda_v:a=code:r=V:c=V','vdda_v:a=code:r=V:c=mV')
        with self.assertRaisesRegex(ValueError,'schema hash mismatch'):sd.parse_format_block(damaged.splitlines())

    def test_v4_imports_vdda_and_reads_in_range_rows_like_v3(self):
        rows=[];seq=0
        for i in range(100):
            seq+=1;rows.append(self.adc_v4('kg1000_v',1000000+i*1000+288,0.001*i,0.5*i,seq))
        for t in (1050,1150):
            for name,raw,cal in (('input_voltage_v','1.29','14.2'),('input_current_a','0.24','0.5'),('vdda_v','3.301','3.301')):
                seq+=1;rows.append(self.supply(name,t,raw,cal,seq))
                if name=='vdda_v':rows[-1][4]='1497'
        result,summary=self.resolve_v4(rows,['kg1000_v','input_supply'])
        self.assertEqual((result['sd_format']['version'],result['sd_format']['schema']),(4,DAQ_V4_SCHEMA))
        self.assertEqual(summary['sd_formats'][0]['profile'],'daq-v4')
        self.assertEqual(summary['unresolved_samples'],0,'vdda_v is imported but feeds no mapping')
        vdda=[json.loads(f) for (f,) in self.db.execute("SELECT fields_json FROM sd_samples WHERE sensor='vdda_v' ORDER BY id")]
        self.assertEqual([(f['raw_adc_code'],f['raw_value'],f['calibrated_value']) for f in vdda],[('1497','3.301','3.301')]*2)
        self.assertEqual([json.loads(v) for _,v,_,_ in self.replay_rows('DAQ_POWER_MONITOR')],[[1.29,0.24,14.2,0.5]]*2)
        kg=self.replay_rows('KG1000')
        self.assertEqual(len(kg),4)
        self.assertTrue(all(o is None for _,_,o,_ in kg))
        self.assertTrue(all(json.loads(v)[0] is not None for _,v,_,_ in kg))

    def test_resolver_skips_over_range_rows_in_bins(self):
        # 20 ms KG1000 bins of 1 kHz rows: bin 0 in range, bin 1 mixed, bin 2
        # all above, bin 3 all below, bin 4 mostly high but last low, bin 5
        # mostly low but last high; bin 6 closes the others.
        def flag(i):
            b,k=divmod(i,20)
            if b==1:return 1 if k%2 else 0
            if b==2:return 1
            if b==3:return -1
            if b==4:return (-1 if k==19 else 1)
            if b==5:return (1 if k==19 else -1)
            return 0
        rows=[];seq=0;expected_mixed=[]
        for i in range(140):
            over=flag(i);volts=0.01*(i%20)
            seq+=1;rows.append(self.adc_v4('kg1000_v',1000000+i*1000+288,99.0 if over else volts,1e9 if over else volts*500,seq,over))
            if i//20==1 and not over:expected_mixed.append(volts)
        _,summary=self.resolve_v4(rows,['kg1000_v'])
        self.assertEqual(summary['unresolved_samples'],0)
        kg=self.replay_rows('KG1000')
        self.assertEqual(len(kg),6)
        for _,v,_,_ in kg:self.assertNotIn('inf',v.lower(),'never Infinity in values_json')
        values=[json.loads(v)[0] for _,v,_,_ in kg];over=[o for _,_,o,_ in kg]
        self.assertIsNone(over[0]);self.assertIsNone(over[1])
        # Mixed bin: the running median and mean see only the in-range rows.
        window=[sd._f32(0.01*k) for k in range(17,20)];means=[]
        for volts in expected_mixed:
            window=(window+[sd._f32(volts)])[-3:];means.append(sorted(window)[1])
        self.assertEqual(values[1],sd._f32(sum(means)/len(means)))
        self.assertEqual(kg[1][3]['mapping']['records'],len(expected_mixed))
        self.assertEqual(kg[1][3]['mapping']['over_range_records'],20-len(expected_mixed))
        # All-over-range bins: null plus the packed flag (mask | low << 32).
        self.assertEqual(values[2:],[None]*4)
        self.assertEqual(over[2:],[1,1|1<<32,1|1<<32,1],'direction of the last flagged row, as the DAQ')
        self.assertEqual(kg[2][3]['mapping']['records'],0)

    def test_flagged_rows_passed_through_are_markers(self):
        rows=[self.adc_v4('kg1000_v',1000288,0.1,50.0,1),self.adc_v4('kg1000_v',1001288,9.9,1e9,2,over=1),
              self.adc_v4('kg1000_v',1002288,-9.9,-1e9,3,over=-1)]
        rows[1][6]='inf';rows[2][6]='-inf'
        self.resolve_v4(rows,['kg1000_kg'])
        kg=self.replay_rows('LOADCELL_WEIGHT_KG')
        self.assertEqual([(json.loads(v),o) for _,v,o,_ in kg],[([50.0],None),([None],1),([None],1|1<<32)])
        self.assertEqual(kg[1][3]['mapping']['over_range'],1)

    def test_mcp_vref_v_is_recorded_with_a_2_4_default(self):
        meta=lambda *lines:[{'record_index':i,'fields':line.split(',')} for i,line in enumerate(lines,1)]
        self.assertEqual(sd._mcp_vref_v(meta('# calibration,kg1000_slope=2,mcp_vref_v=2.20000005e+00,loadcell_input=adc_volts')),
                         ([2.20000005],False))
        self.assertEqual(sd._mcp_vref_v(meta('# calibration,mcp_vref_v=2.2,loadcell_input=adc_volts','# calibration,mcp_vref_v=2.2',
                                             '# calibration,mcp_vref_v=2.21')),([2.2,2.21],False),'each change, in file order')
        self.assertEqual(sd._mcp_vref_v(meta('# calibration,kg1000_slope=2,loadcell_input=adc_volts')),([2.4],True))
        with self.assertRaisesRegex(ValueError,'Invalid mcp_vref_v'):sd._mcp_vref_v(meta('# calibration,mcp_vref_v=nan'))
        path=self.write('DAQ_0004.CSV',daq_block(version=4,records=DAQ_V4_RECORDS),[self.adc_v4('kg1000_v',1000288,0.1,50.0,1)],
                        extra='# calibration,kg1000_slope=2.00000000e+00,mcp_vref_v=2.20000005e+00,loadcell_input=adc_volts\r\n')
        sd.import_csv(self.db,path,self.make_manifest(['kg1000_kg'],DAQ_KINDS))
        summary=sd.resolve_recording(self.db)
        self.assertEqual((summary['sd_formats'][0]['mcp_vref_v'],summary['sd_formats'][0]['mcp_vref_v_default']),([2.20000005],False))
        self.assertEqual(self.replay_rows('LOADCELL_WEIGHT_KG')[0][3]['mapping']['sd_format']['mcp_vref_v'],[2.20000005])

    def test_files_without_mcp_vref_v_record_the_default(self):
        _,summary=self.resolve_v4([self.adc_v4('kg1000_v',1000288,0.1,50.0,1)],['kg1000_kg'])
        self.assertEqual((summary['sd_formats'][0]['mcp_vref_v'],summary['sd_formats'][0]['mcp_vref_v_default']),([2.4],True))

    def test_bad_flag_is_unresolved_not_a_number(self):
        rows=[self.adc_v4('kg1000_v',1000288,0.1,50.0,1),self.adc_v4('kg1000_v',1001288,0.1,50.0,2)]
        rows[1][3]='2'
        _,summary=self.resolve_v4(rows,['kg1000_kg'])
        self.assertEqual(summary['unresolved_samples'],1)
        self.assertEqual(len(self.replay_rows('LOADCELL_WEIGHT_KG')),1)

    def test_v3_value_column_is_not_a_flag(self):
        # v3 declares no value column for kg1000_v: whatever it holds is ignored.
        rows=[self.adc('kg1000_v',1000288,0.1,50.0,1)];rows[0][3]='1'
        path=self.write('DAQ_0003.CSV',daq_block(),rows)
        sd.import_csv(self.db,path,self.make_manifest(['kg1000_kg'],DAQ_KINDS));sd.resolve_recording(self.db)
        self.assertEqual([(json.loads(v),o) for _,v,o,_ in self.replay_rows('LOADCELL_WEIGHT_KG')],[([50.0],None)])

    def test_network_rows_keep_their_over_range_column(self):
        self.db.execute('ALTER TABLE telemetry ADD COLUMN over_range INTEGER')
        self.db.execute("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,over_range) "
                        "VALUES(?,?,'KG1000','DAQ','[null]',?)",(BASE_UTC-5000,BASE_UTC-5000,1|1<<32))
        self.db.commit()
        self.resolve_v4([self.adc_v4('kg1000_v',1000288,0.1,50.0,1)],['kg1000_kg'])
        net=[(json.loads(v),o,p['origin']) for _,v,o,p in self.replay_rows('KG1000')]
        self.assertEqual(net,[([None],1|1<<32,'network')])


class LegacySelectionTests(FormatBase):
    def legacy_manifest(self,**updates):
        return self.make_manifest(['kg1000_v'],DAQ_KINDS,**{'mapping_profile':'daq-20261123','firmware_head':sd.DAQ_SD_HEAD_US,**updates})

    def test_headerless_files_still_need_the_pins(self):
        path=self.write('DAQ_OLD.CSV','',self.daq_rows())
        self.assertIsNone(sd.read_format_block(path))
        _,profile,_=sd._resolution_manifest(self.legacy_manifest(),None)
        self.assertEqual(profile['sd_format'],{'format':'daq','version':2,'firmware':sd.DAQ_SD_HEAD_US,'schema':None,
                                               'profile':'daq-20261123','legacy':True})
        for updates in ({'mapping_profile':None},{'firmware_head':None},{'firmware_head':'0'*40},{'mapping_profile':'daq-v3'}):
            with self.assertRaisesRegex(ValueError,'Headerless .legacy. SD file'):
                sd._resolution_manifest(self.legacy_manifest(**updates),None)
        result=sd.import_csv(self.db,path,self.legacy_manifest())
        self.assertIs(result['sd_format']['legacy'],True)
        summary=sd.resolve_recording(self.db)
        self.assertEqual(summary['sd_formats'][0]['version'],2)
        p=self.resolved()[0][3]
        self.assertEqual((p['mapping']['profile'],p['mapping']['source_evidence']),('daq-20261123',sd.DAQ_SD_HEAD_US))

    def test_legacy_labels(self):
        self.assertEqual(sd._supported_formats_text(),'daq v3, daq v4, fc v2 (and legacy headerless daq v1/v2, fc v1)')
        for profile,(fmt,version) in sd.LEGACY_SD_FORMATS.items():
            self.assertIn(profile,sd.PROFILES)
            self.assertEqual(sd.sd_format_summary(None,{'mapping_profile':profile,'firmware_head':'h'})['version'],version)


class WorkerTests(FormatBase):
    """The composed script recording_sd.rs runs (import + resolve + worker())."""
    def run_worker(self,csv_path,manifest):
        source=self.root/'source.db'
        with closing(sqlite3.connect(source)) as db:
            db.execute('CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)')
        script=((ROOT/'backend/src/recording_sd_import.py').read_text().replace("if __name__=='__main__':\n    main()",'')
                +'\n'+(ROOT/'backend/src/recording_sd_resolve.py').read_text()+'\nworker()')
        request={'source':str(source),'output':str(self.root/'out.db'),'csv_path':str(csv_path),'manifest':manifest,
                 'max_bytes':1<<30,'max_database_bytes':1<<30,'minimum_free_bytes':0}
        return subprocess.run([sys.executable,'-c',script],input=json.dumps(request).encode(),capture_output=True)

    def test_worker_reports_the_detected_format(self):
        path=self.write('DAQ_0001.CSV',daq_block(firmware='unknown'),self.daq_rows())
        out=self.run_worker(path,self.make_manifest(['kg1000_v'],DAQ_KINDS))
        self.assertEqual(out.returncode,0,out.stderr.decode())
        result=json.loads(out.stdout)
        self.assertEqual((result['sd_format']['format'],result['sd_format']['version'],result['sd_format']['firmware']),('daq',3,'unknown'))
        self.assertEqual(result['import']['sd_format']['version'],3)
        self.assertEqual(result['sd_formats'][0]['profile'],'daq-v3')

    def test_worker_refuses_unknown_versions_before_copying(self):
        path=self.write('DAQ_0001.CSV',daq_block(version=9),self.daq_rows())
        out=self.run_worker(path,self.make_manifest(['kg1000_v'],DAQ_KINDS))
        self.assertNotEqual(out.returncode,0)
        self.assertIn('SD file declares format daq v9',out.stderr.decode())
        self.assertFalse((self.root/'out.db').exists())


if __name__=='__main__':unittest.main()
