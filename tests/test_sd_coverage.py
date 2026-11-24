import test_recording_sd_import as fixture
ROOT,sd=fixture.ROOT,fixture.sd
import csv
import json
import math
from pathlib import Path
import sqlite3
import struct
import subprocess
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
        # Recorded by a volts-scale station: network rows are served as stored.
        self.db.execute('CREATE TABLE loadcell_units(id INTEGER PRIMARY KEY CHECK(id=1),first_volts_id INTEGER NOT NULL,unit TEXT NOT NULL)')
        self.db.execute("INSERT INTO loadcell_units VALUES(1,1,'adc_volts')")
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
    def test_legacy_raw_load_cells_are_resolved_in_volts(self):
        base=1700000000000;k=sd.LEGACY_RAW_TO_VOLTS
        self.db.execute('DELETE FROM telemetry')
        # Legacy station rows: AMP2 KG50 and KG1000 on the raw scale, a P6 KG50
        # tagged [source, volts] (already volts), then rows after the volts mark.
        self.db.executemany('INSERT INTO telemetry VALUES(?,?,?,?,?,?,?)',[
            (1,base+50000,base+5000,'DAQ','KG50','[0.0122373]','[1,2,3,4]'),
            (2,base+50000,base+5001,'DAQ','KG1000','[0.0122373]','[]'),
            (3,base+50000,base+5002,'DAQ','KG50','[0.0042532505]','[0,0,128,63,218,94,139,59]'),
            (4,base+50000,base+5003,'DAQ','ROLL','[0.5]','[]'),
            (5,base+50000,base+5004,'DAQ','KG1000','[0.25]','[]')])
        self.db.execute('CREATE TABLE loadcell_units(id INTEGER PRIMARY KEY CHECK(id=1),first_volts_id INTEGER NOT NULL,unit TEXT NOT NULL)')
        self.db.execute("INSERT INTO loadcell_units VALUES(1,5,'adc_volts')")
        self.db.commit()
        # SD rows from the legacy firmware profile are raw-scale too.
        rows=[self.row(sensor='kg50_network',value='0.0122373',timestamp_ms=str(base),uptime_ticks='100',monotonic_ms='100',record_sequence='1')]
        sd.import_csv(self.db,self.file('legacy.csv',rows),self.manifest())
        sd.resolve_recording(self.db)
        resolved={(json.loads(p)['origin'],kind,t):(json.loads(v),json.loads(p)) for t,kind,v,p in
                  self.db.execute('SELECT timestamp_ms,data_type,values_json,provenance_json FROM sd_replay_telemetry')}
        volts=sd.legacy_raw_to_volts(0.0122373)
        self.assertAlmostEqual(volts,0.0122373*k,places=6)
        self.assertEqual(resolved[('network','KG50',base+5000)][0],[volts])
        self.assertEqual(resolved[('network','KG50',base+5000)][1]['converted_from']['values_json'],'[0.0122373]')
        self.assertEqual(resolved[('network','KG1000',base+5001)][0],[volts])
        self.assertEqual(resolved[('network','KG50',base+5002)][0],[0.0042532505],'P6 volts are not rescaled')
        self.assertEqual(resolved[('network','ROLL',base+5003)][0],[0.5])
        self.assertEqual(resolved[('network','KG1000',base+5004)][0],[0.25],'after the volts mark')
        sd_value,provenance=resolved[('sd','KG50',base)]
        self.assertEqual(sd_value,[volts])
        self.assertEqual(provenance['mapping']['units'],['V'])
        self.assertEqual(provenance['mapping']['converted_from']['volts_per_raw'],'16000/921')
        self.assertEqual(self.db.execute('SELECT unit FROM sd_replay_loadcell_units').fetchone()[0],'adc_volts')
        # The original network table is untouched.
        self.assertEqual(self.db.execute('SELECT values_json FROM main.telemetry WHERE id=2').fetchone()[0],'[0.0122373]')
    def test_legacy_conversion_matches_the_backend_binary32_result(self):
        import struct
        f32=lambda x:struct.unpack('<f',struct.pack('<f',x))[0]
        for raw in (0.0122373,0.19427338,-0.0015,1e-7):
            self.assertEqual(sd.legacy_raw_to_volts(raw),f32(f32(raw)*16000/921))


def _daq_checkout():
    """A DAQ-Board checkout whose network boxcar is daq_bin_average_add."""
    for parent in Path(__file__).resolve().parents:
        header=parent/'DAQ-Board/Core/Inc/daq_downsample.h'
        if header.is_file() and 'daq_bin_average_add' in header.read_text():
            return parent/'DAQ-Board'
    return None


HEADER=','.join(sd.FIELDS)+'\r\n'
CALIBRATION='# calibration,kg1000_slope=2.00000000e+00,loadcell_input=adc_volts,loadcell_boxcar_ms=4\r\n'
BASE_UTC=1800000000000


def f32(value):
    return struct.unpack('<f',struct.pack('<f',value))[0]


class LoadcellBoxcarTests(unittest.TestCase):
    """SD backfill of KG1000/KG50 from the DAQ's raw records (profile daq-20261117)."""
    row=fixture.RecoveryTests.row
    tearDown=fixture.RecoveryTests.tearDown
    def setUp(self):
        fixture.RecoveryTests.setUp(self)
        self.db.execute('DELETE FROM telemetry');self.db.commit()
    def profile_manifest(self,selected=('kg1000_v','kg50_v')):
        schema={'types':[{'name':kind,'element':{'kind':'Static','data_type':'Float32','count':count}}
                         for kind,count in [('KG1000',1),('KG50',1),('DAQ_POWER_MONITOR',4),('LOADCELL_WEIGHT_KG',1)]]}
        return {'board_id':'DAQ-serial-1','boot_id':'boot1','stream_id':'raw','launch_session':'fire-1',
            'mapping_profile':'daq-20261117','firmware_head':sd.DAQ_SD_HEAD,'selected_sensors':list(selected),
            'config':{'schema':schema},'sensor_types':{},'assignment':{'sender_id':'DAQ','boot_and_session_verified':True,
                'units_axes_calibration_verified':True,'start_utc_ms':BASE_UTC-10000,'end_utc_ms':BASE_UTC+600000,'max_gap_ms':20}}
    def write(self,name,lines):
        path=Path(self.temp.name)/name
        path.write_bytes((HEADER+CALIBRATION+''.join(lines)).encode())
        return path
    def resolved(self):
        return [(t,kind,json.loads(v),json.loads(p)) for t,kind,v,p in self.db.execute(
            'SELECT timestamp_ms,data_type,values_json,provenance_json FROM sd_replay_telemetry ORDER BY id')]

    def test_resolver_matches_the_daq_boxcar_bit_for_bit(self):
        daq=_daq_checkout()
        if daq is None:
            self.skipTest('No DAQ-Board checkout with daq_bin_average_add next to this GroundStation checkout')
        # Production DAQ code: daq_bin_average_add (drain's KG1000/KG50 boxcar,
        # restarting KG50 when its input changes) and the SD raw formatter.
        program=r"""
#include "daq_downsample.h"
#include "sd_raw_record_format.h"
#include <stdio.h>
#include <string.h>
int main(void){
 daq_bin_average_t kg1000={0},kg50={0};unsigned kg50_channel=1;
 sd_raw_format_cache_t cache={0};char line[256];uint32_t state=12345,seq=0;uint64_t t_us=0;
 for(unsigned i=0;i<6000;i++){
  state^=state<<13;state^=state>>17;state^=state<<5;
  t_us+=150+(state%80);                       /* irregular ~5.2 kHz conversions */
  const uint64_t ms=1000000+t_us/1000;
  const unsigned channel=(i&1)?(i>=3000&&i<4500?9:1):0;  /* P6 selected mid-run */
  const int32_t code=(int32_t)(state>>8)-8388608;
  const float volts=(float)code*2.4f/8388608.0f/(channel==9?16.0f:1.0f);
  const char*name=channel==0?"kg1000_v":channel==1?"kg50_v":"kg50_p6_v";
  sd_format_csv_raw_record(line,sizeof line,1800000000000ULL+(ms-1000000),ms,name,code,volts,volts*2.0f,25.0f,7,++seq,&cache);
  fputs(line,stdout);
  float out;uint64_t at;uint32_t bits;
  if(channel==0){
   if(daq_bin_average_add(&kg1000,volts,ms,4,&out,&at)){memcpy(&bits,&out,4);printf("#KG1000,%llu,%u\n",(unsigned long long)at,bits);}
  }else{
   if(channel!=kg50_channel){memset(&kg50,0,sizeof kg50);kg50_channel=channel;}
   if(daq_bin_average_add(&kg50,volts,ms,4,&out,&at)){memcpy(&bits,&out,4);printf("#KG50,%llu,%u\n",(unsigned long long)at,bits);}
  }
 }
}
"""
        binary=Path(self.temp.name)/'daq-boxcar'
        subprocess.run(['cc','-std=c11','-O2','-I',str(daq/'Core/Inc'),'-x','c','-','-o',str(binary)],input=program.encode(),check=True)
        output=[line.rstrip('\r') for line in subprocess.check_output([str(binary)]).decode().split('\n') if line]
        daq_bins={}
        for mark in (line for line in output if line.startswith('#K')):
            kind,uptime,bits=mark[1:].split(',')
            daq_bins.setdefault(kind,[]).append((BASE_UTC+int(uptime)-1000000,int(bits)))
        csv_rows=''.join(line+'\r\n' for line in output if not line.startswith('#'))
        sd.import_csv(self.db,self.write('fire.csv',[csv_rows]),self.profile_manifest())
        sd.resolve_recording(self.db)
        resolved={}
        for t,kind,values,provenance in self.resolved():
            self.assertEqual(provenance['origin'],'sd')
            self.assertEqual(provenance['mapping']['derived'],'boxcar')
            resolved.setdefault(kind,[]).append((t,struct.unpack('<I',struct.pack('<f',values[0]))[0]))
        self.assertGreater(len(daq_bins['KG1000']),250)
        self.assertGreater(len(daq_bins['KG50']),250)
        # Every bin the DAQ closed, value bits and the time of its last record.
        self.assertEqual(resolved['KG1000'],daq_bins['KG1000'])
        self.assertEqual(resolved['KG50'],daq_bins['KG50'])

    def test_boxcar_bins_reset_on_input_switch_and_pair_the_supply(self):
        rows=[]
        def raw(name,uptime,volts,calibrated=None,seq=[0]):
            seq[0]+=1
            return self.row(sensor=name,timestamp_ms=str(BASE_UTC+uptime),monotonic_ms=str(uptime),uptime_ticks=str(uptime),
                value='',raw_value=repr(volts),calibrated_value=repr(volts if calibrated is None else calibrated),record_sequence=str(seq[0]))
        for uptime,volts in [(100,0.25),(101,0.5),(103,0.75),(104,1.0),(108,2.0)]:
            rows.append(raw('kg1000_v',uptime,volts))
        rows.append(raw('kg50_v',100,0.1));rows.append(raw('kg50_v',101,0.3))
        rows.append(raw('kg50_p6_v',102,-0.002));rows.append(raw('kg50_p6_v',104,-0.004))  # switch drops the AMP2 partial bin
        rows.append(raw('kg50_p6_v',108,-0.006))
        rows.append(raw('input_voltage_v',105,1.29,14.2));rows.append(raw('input_current_a',105,0.24,0.5))
        rows.append(raw('input_voltage_v',109,1.01,11.1));rows.append(raw('input_current_a',109,0.24,0.5))
        path=Path(self.temp.name)/'mix.csv'
        with path.open('w',newline='') as out:
            out.write(HEADER+CALIBRATION);csv.writer(out,lineterminator='\r\n').writerows(rows)
        sd.import_csv(self.db,path,self.profile_manifest(('kg1000_v','kg50_v','input_supply')))
        summary=sd.resolve_recording(self.db)
        got={(kind,t):(values,p) for t,kind,values,p in self.resolved()}
        mean=lambda *v:f32(sum(f32(x) for x in v)/len(v))
        self.assertEqual(got[('KG1000',BASE_UTC+103)][0],[mean(0.25,0.5,0.75)])
        self.assertEqual(got[('KG1000',BASE_UTC+103)][1]['mapping']['records'],3)
        self.assertEqual(got[('KG1000',BASE_UTC+104)][0],[1.0])
        self.assertNotIn(('KG1000',BASE_UTC+108),got,'the last bin is open at the end of the file')
        self.assertNotIn(('KG50',BASE_UTC+101),got,'AMP2 partial bin dropped at the input switch')
        self.assertEqual(got[('KG50',BASE_UTC+102)][0],[f32(-0.002)])
        self.assertEqual(got[('KG50',BASE_UTC+104)][0],[f32(-0.004)])
        self.assertEqual(got[('KG50',BASE_UTC+104)][1]['mapping']['source_sensor'],'kg50_p6_v')
        self.assertEqual(got[('DAQ_POWER_MONITOR',BASE_UTC+105)][0],[1.29,0.24,14.2,0.5])
        self.assertEqual(got[('DAQ_POWER_MONITOR',BASE_UTC+109)][0],[1.01,0.24,11.1,0.5])
        self.assertEqual(got[('DAQ_POWER_MONITOR',BASE_UTC+105)][1]['mapping']['derived'],'pair')
        self.assertEqual(summary['unresolved_samples'],0)

    def test_files_with_and_without_the_removed_kg1000_window_input_row_resolve(self):
        # DAQ firmware before DAQ_SD_HEAD also wrote kg1000_window_input_v, a
        # scalar copy of kg1000_window_v; current files no longer contain it.
        def rows(with_input):
            out=[]
            for i,(uptime,volts) in enumerate([(100,0.25),(101,0.5),(104,1.0),(108,2.0)]):
                out.append(self.row(sensor='kg1000_v',timestamp_ms=str(BASE_UTC+uptime),monotonic_ms=str(uptime),
                    uptime_ticks=str(uptime),value='',raw_value=repr(volts),calibrated_value=repr(volts),record_sequence=str(2*i+1)))
                for name in ('kg1000_window_v',)+(('kg1000_window_input_v',) if with_input else ()):
                    out.append(self.row(sensor=name,timestamp_ms=str(BASE_UTC+uptime),monotonic_ms=str(uptime),
                        uptime_ticks=str(uptime),value=repr(volts),raw_value='',calibrated_value='',record_sequence=str(2*i+2)))
            return out
        results={}
        for head,with_input in ((sd.DAQ_SD_HEAD_WITH_WINDOW_INPUT,True),(sd.DAQ_SD_HEAD,False)):
            path=Path(self.temp.name)/f'{head[:7]}.csv'
            with path.open('w',newline='') as out:
                out.write(HEADER+CALIBRATION);csv.writer(out,lineterminator='\r\n').writerows(rows(with_input))
            sd.import_csv(self.db,path,{**self.profile_manifest(('kg1000_v',)),'firmware_head':head,'boot_id':head})
            sd.resolve_recording(self.db)
            got=[(t,v,p['mapping']['source_evidence']) for t,kind,v,p in self.resolved() if kind=='KG1000' and p['mapping']['source_evidence']==head]
            self.assertTrue(got,head)
            results[head]=[(t,v) for t,v,_ in got]
        self.assertEqual(results[sd.DAQ_SD_HEAD_WITH_WINDOW_INPUT],results[sd.DAQ_SD_HEAD])
        self.assertNotEqual(sd.DAQ_SD_HEAD,sd.DAQ_SD_HEAD_WITH_WINDOW_INPUT)
        with self.assertRaises(ValueError):sd._resolution_manifest({**self.profile_manifest(),'firmware_head':'0'*40})

    def test_new_names_do_not_resolve_under_the_legacy_profile_and_vice_versa(self):
        legacy={**self.profile_manifest(),'mapping_profile':'daq-20261101','firmware_head':sd.DAQ_HEAD,'selected_sensors':['kg1000_v']}
        with self.assertRaises(ValueError):sd._resolution_manifest(legacy)
        with self.assertRaises(ValueError):sd._resolution_manifest(self.profile_manifest(('kg1000_network',)))
        with self.assertRaises(ValueError):sd._resolution_manifest({**self.profile_manifest(),'firmware_head':sd.DAQ_HEAD})

    def test_file_boxcar_width_is_read_from_the_calibration_line(self):
        self.assertEqual(sd._boxcar_ms([{'fields':['# calibration','kg1000_slope=1','loadcell_boxcar_ms=10']}]),10)
        self.assertEqual(sd._boxcar_ms([{'fields':['# calibration','kg1000_slope=1']}]),sd.DAQ_BOXCAR_MS)
        with self.assertRaises(ValueError):
            sd._boxcar_ms([{'fields':['# calibration','loadcell_boxcar_ms=4']},{'fields':['# calibration','loadcell_boxcar_ms=5']}])



def _daq_median_checkout():
    """A DAQ-Board checkout whose network values are median bins (daq_median.h)."""
    for parent in Path(__file__).resolve().parents:
        header=parent/'DAQ-Board/Core/Inc/daq_median.h'
        if header.is_file() and 'daq_bin_us_add' in header.read_text():
            return parent/'DAQ-Board'
    return None


class LoadcellMedianBinTests(unittest.TestCase):
    """SD backfill of KG1000/KG50 from microsecond raw records (profile daq-20261123)."""
    tearDown=fixture.RecoveryTests.tearDown
    def setUp(self):
        fixture.RecoveryTests.setUp(self)
        self.db.execute('DELETE FROM telemetry');self.db.commit()
    def median_manifest(self):
        schema={'types':[{'name':kind,'element':{'kind':'Static','data_type':'Float32','count':1}}
                         for kind in ('KG1000','KG50')]}
        return {'board_id':'DAQ-serial-1','boot_id':'boot1','stream_id':'raw','launch_session':'fire-1',
            'mapping_profile':'daq-20261123','firmware_head':sd.DAQ_SD_HEAD_US,'selected_sensors':['kg1000_v','kg50_v'],
            'config':{'schema':schema},'sensor_types':{},'assignment':{'sender_id':'DAQ','boot_and_session_verified':True,
                'units_axes_calibration_verified':True,'start_utc_ms':BASE_UTC-10000,'end_utc_ms':BASE_UTC+600000,'max_gap_ms':60}}

    def test_profile_accepts_the_dac_generator_head_and_no_other(self):
        # The DAC generator adds only dac_chN_* scalar rows on top of the 1 kHz
        # raw-record format, so its DAQ head resolves under the same profile.
        for head in (sd.DAQ_SD_HEAD_US, sd.DAQ_SD_HEAD_US_DAC):
            manifest=self.median_manifest(); manifest['firmware_head']=head
            sd._resolution_manifest(manifest)
        manifest=self.median_manifest(); manifest['firmware_head']='0'*40
        with self.assertRaises(ValueError):
            sd._resolution_manifest(manifest)

    def test_resolver_matches_the_daq_median_bins_bit_for_bit(self):
        daq=_daq_median_checkout()
        if daq is None:
            self.skipTest('No DAQ-Board checkout with daq_median.h next to this GroundStation checkout')
        # Production DAQ code: daq_median_add + daq_bin_us_add as the drain uses
        # them (restarting KG50 when its input changes) and the SD raw formatter.
        # With SD format v4 (an over_range argument) the file carries the
        # firmware's own format block, some conversions are over range, and
        # those join their bin through daq_bin_us_add_over instead.
        v4='int8_t over_range' in (daq/'Core/Inc/sd_raw_record_format.h').read_text()
        program=(r"""
#define SD_V4 1
#include "daq_over_range.h"
#include "sd_format.h"
""" if v4 else '#define SD_V4 0\n')+r"""
#include "daq_median.h"
#include "sd_raw_record_format.h"
#include <stdio.h>
#include <string.h>
#if SD_V4
#define FORMAT(...) sd_format_csv_raw_record(line,sizeof line,__VA_ARGS__)
#define OVER(code) daq_adc_over_range(code)
#else
#define OVER(code) 0
#endif
static int add(daq_bin_us_t*b,daq_median_t*m,int over,float volts,uint64_t t_us,uint32_t period,daq_bin_out_t*out){
#if SD_V4
 if(over)return daq_bin_us_add_over(b,over,t_us,period,out);
#endif
 (void)over;return daq_bin_us_add(b,daq_median_add(m,volts),t_us,period,out);
}
int main(void){
#if SD_V4
 fputs(SD_FORMAT_BLOCK,stdout);
#endif
 daq_median_t m1000,m50; daq_median_init(&m1000,3); daq_median_init(&m50,5);
 daq_bin_us_t b1000={0},b50={0}; unsigned kg50_channel=1; daq_bin_out_t out;
 sd_raw_format_cache_t cache={0};char line[256];uint32_t state=12345,seq=0;
 for(unsigned frame=0;frame<6000;frame++){
  const uint64_t frame_us=1000000000ULL+frame*1000ULL;
  for(unsigned k=0;k<2;k++){
   state^=state<<13;state^=state>>17;state^=state<<5;
   const unsigned channel=k==0?0:(frame>=2000&&frame<4000?9:1);   /* P6 selected mid-run */
   if(k==1&&frame%10>=8)continue;                                   /* temperature and aux slots */
   const uint64_t t_us=frame_us+(k==0?288:620);
   int32_t code=(int32_t)(state>>8)-8388608;
   /* Over-range stretches (v4): some isolated, some filling whole bins. */
   if(SD_V4&&(frame%97<3||(frame>=500&&frame<560)||(frame>=3000&&frame<3120)))code=code<0?code-8388608:code+8388608;
   const int over=OVER(code);
   const float volts=(float)code*2.4f/8388608.0f/(channel==9?16.0f:1.0f);
   const char*name=channel==0?"kg1000_v":channel==1?"kg50_v":"kg50_p6_v";
   const uint64_t ms=t_us/1000;
#if SD_V4
   FORMAT(1800000000000ULL+(ms-1000000),ms,t_us,1000000,name,(int8_t)over,code,volts,over>0?INFINITY:over<0?-INFINITY:volts*2.0f,25.0f,7,++seq,&cache);
#else
   sd_format_csv_raw_record(line,sizeof line,1800000000000ULL+(ms-1000000),ms,t_us,1000000,name,code,volts,volts*2.0f,25.0f,7,++seq,&cache);
#endif
   fputs(line,stdout);
   uint32_t bits;
   if(channel==0){
    if(add(&b1000,&m1000,over,volts,t_us,20000,&out)){memcpy(&bits,&out.mean,4);printf("#KG1000,%llu,%u\n",(unsigned long long)(out.centre_us/1000),bits);}
   }else{
    if(channel!=kg50_channel){b50=(daq_bin_us_t){0};daq_median_init(&m50,5);kg50_channel=channel;}
    if(add(&b50,&m50,over,volts,t_us,50000,&out)){memcpy(&bits,&out.mean,4);printf("#KG50,%llu,%u\n",(unsigned long long)(out.centre_us/1000),bits);}
   }
  }
 }
}
"""
        binary=Path(self.temp.name)/'daq-median'
        subprocess.run(['cc','-std=c11','-O2','-I',str(daq/'Core/Inc'),'-x','c','-','-lm','-o',str(binary)],input=program.encode(),check=True)
        output=[line.rstrip('\r') for line in subprocess.check_output([str(binary)]).decode().split('\n') if line]
        daq_bins={}
        for mark in (line for line in output if line.startswith('#K')):
            kind,uptime_ms,bits=mark[1:].split(',')
            daq_bins.setdefault(kind,[]).append((BASE_UTC+int(uptime_ms)-1000000,int(bits)))
        block=''.join(line+'\r\n' for line in output if line.startswith('# seds-sd'))
        csv_rows=''.join(line+'\r\n' for line in output if not line.startswith('#'))
        path=Path(self.temp.name)/'fire.csv'
        path.write_bytes((HEADER+block+'# calibration,kg1000_slope=2.00000000e+00,loadcell_input=adc_volts,loadcell_report=median_bin\r\n'+csv_rows).encode())
        manifest=self.median_manifest()
        if v4:
            del manifest['mapping_profile'],manifest['firmware_head']
            self.assertEqual(sd.read_format_block(path)['version'],4)
        sd.import_csv(self.db,path,manifest)
        sd.resolve_recording(self.db)
        resolved={};markers=0
        for t,kind,values,over_range,provenance in [(t,k,json.loads(v),o,json.loads(p)) for t,k,v,o,p in self.db.execute(
                'SELECT timestamp_ms,data_type,values_json,over_range,provenance_json FROM sd_replay_telemetry ORDER BY id')]:
            self.assertEqual(provenance['mapping']['derived'],'median_bin')
            value=values[0]
            if over_range:
                # Stored as null plus the packed flag; the DAQ published +-inf.
                self.assertIsNone(value);markers+=1
                value=-math.inf if over_range>>32 else math.inf
            resolved.setdefault(kind,[]).append((t,struct.unpack('<I',struct.pack('<f',value))[0]))
        if v4:self.assertGreater(markers,4,'whole over-range bins were exercised')
        self.assertGreater(len(daq_bins['KG1000']),290)
        self.assertGreater(len(daq_bins['KG50']),100)
        # Every bin the DAQ closed: value bits, dated at the bin centre.
        self.assertEqual(resolved['KG1000'],daq_bins['KG1000'])
        self.assertEqual(resolved['KG50'],daq_bins['KG50'])


if __name__=='__main__':unittest.main()
