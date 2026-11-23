"""Explicit semantic coverage resolver for imported derived recordings.

No receipt timestamps participate in matching. A file with a SEDS-SD format
block (`# seds-sd,format=..,version=..`, see parse_format_block) is read by the
parser registered for its (format, version) in SD_FORMATS, keyed off the record
entries it declares; its firmware revision is informational. Headerless files
are legacy: their profile is pinned to inspected firmware source identities.
Callers provide the recorded schema and boot/session assignment. Unsupported
formats remain in raw SD storage for explicit selection.
"""
import math
import struct

FC_HEAD='ec188c6daac84e3c957f57ca95d872953cbda588'
DAQ_HEAD='ef3702d87071f204e5ccb9a124b739af82538ac2'
DAQ_SD_HEAD='44a9195cb6a793ab89d0848683a140509f72cbbc'
# Earlier DAQ firmware with the same raw-record format that also wrote the
# scalar row kg1000_window_input_v (always equal to kg1000_window_v; removed).
# Its files still import under the same profile; no mapping reads that row.
DAQ_SD_HEAD_WITH_WINDOW_INPUT='55fe7a811b74001ea2e9db07de24f57b97feb176'
# DAQ firmware with the 1 kHz frame schedule: MCP3564R raw rows carry
# microsecond uptime_ticks (uptime_frequency_hz=1000000), and the network
# KG1000/KG50 values are fixed-bin means of running medians (daq_median.h).
DAQ_SD_HEAD_US='e5fad32e33f065771887770cc8a650bf9692aaf0'
# The same raw-record format with the DAC waveform generator added (DAQ
# 99db2e6 on top of e5fad32). It writes only extra dac_chN_* scalar rows, which
# no mapping reads.
DAQ_SD_HEAD_US_DAC='99db2e65c6310d6ed088831cde62ba683175eae6'
# field, element reordering, target type, units. BARO existing summary is alt/prs/tmp;
# raw barometer_sample is pressure/temperature/altitude. These must not be swapped.
PROFILES={
 'fc-20261101': {'head':FC_HEAD,'mappings':{
  'barometer_sample':('calibrated_value',[2,0,1],'BAROMETER_DATA',['m','Pa','degC']),
  'BARO':('value',[0,1,2],'BAROMETER_DATA',['m','Pa','degC']),
  'IMU':('value',[0,1,2,3,4,5],'IMU_DATA',['g']*3+['deg/s']*3),
  'gyroscope_sample':('calibrated_value',[0,1,2],'GYRO_DATA',['deg/s']*3),
  'accelerometer_sample':('calibrated_value',[0,1,2],'ACCEL_DATA',['g']*3),
  'EKF':('value',[0,1,2,3,4,5],'ASCENT_STATE',['1']*4+['m','m/s']),
  'DKF':('value',[0,1,2,3],'DESCENT_STATE',['deg','deg','m','m/s']),
 }},
 'daq-20261101': {'head':DAQ_HEAD,'mappings':{
  # KG50/KG1000 network publications contain raw sensor values, not SD calibrated kg.
  # This firmware wrote them on the legacy raw scale; the resolved view converts
  # them to ADC-pin volts (see LEGACY_RAW_TO_VOLTS).
  'kg50_raw':('calibrated_value',[0],'LOADCELL_50_WEIGHT_KG',['kg']),
  'mcp3564r_raw':('calibrated_value',[0],'LOADCELL_WEIGHT_KG',['kg']),
  'kg50_network':('value',[0],'KG50',['sensor_raw']),
  'kg1000_network':('value',[0],'KG1000',['sensor_raw']),
 }},
 # DAQ firmware that keeps each load-cell conversion once, as a raw record
 # named <sensor>_<unit> (DAQ-Board docs/sd-record-format.md). SEDSnet
 # KG1000/KG50 are rebuilt from those records with the DAQ's own boxcar, so
 # SD backfill equals what the station would have received. Selections are
 # mapping names; 'sensors' lists the SD rows each one reads.
 'daq-20261123': {'head':DAQ_SD_HEAD_US,'heads':(DAQ_SD_HEAD_US,DAQ_SD_HEAD_US_DAC),'mappings':{
  # KG1000 network value: mean of each 20000 us bin of a 3-sample running median.
  'kg1000_v':('raw_value',[0],'KG1000',['V'],{'median_bin':(3,20000),'uptime_hz':1000000,'sensors':('kg1000_v',)}),
  # Selected KG50 (AMP2 or P6): 5-sample median, 50000 us bins; both restart
  # when the selection changes, as on the DAQ.
  'kg50_v':('raw_value',[0],'KG50',['V'],{'median_bin':(5,50000),'uptime_hz':1000000,'sensors':('kg50_v','kg50_p6_v')}),
  'kg1000_kg':('calibrated_value',[0],'LOADCELL_WEIGHT_KG',['kg'],{'uptime_hz':1000000,'sensors':('kg1000_v',)}),
  'kg50_kg':('calibrated_value',[0],'LOADCELL_50_WEIGHT_KG',['kg'],{'uptime_hz':1000000,'sensors':('kg50_v',)}),
  'input_supply':(None,[0,1,2,3],'DAQ_POWER_MONITOR',['V','V','V','A'],{'pair':('input_voltage_v','input_current_a'),
      'sensors':('input_voltage_v','input_current_a')}),
 }},
 'daq-20261117': {'head':DAQ_SD_HEAD,'heads':(DAQ_SD_HEAD,DAQ_SD_HEAD_WITH_WINDOW_INPUT),'mappings':{
  'kg1000_v':('raw_value',[0],'KG1000',['V'],{'boxcar':True,'sensors':('kg1000_v',)}),
  # The selected KG50 input: AMP2 (CH1) or P6. The DAQ restarts the boxcar
  # when the selection changes, and so does this reader.
  'kg50_v':('raw_value',[0],'KG50',['V'],{'boxcar':True,'sensors':('kg50_v','kg50_p6_v')}),
  'kg1000_kg':('calibrated_value',[0],'LOADCELL_WEIGHT_KG',['kg'],{'sensors':('kg1000_v',)}),
  'kg50_kg':('calibrated_value',[0],'LOADCELL_50_WEIGHT_KG',['kg'],{'sensors':('kg50_v',)}),
  # Board supply rows pair into the DAQ_POWER_MONITOR layout
  # [VMON pin V, IMON pin V, input V, input A].
  'input_supply':(None,[0,1,2,3],'DAQ_POWER_MONITOR',['V','V','V','A'],{'pair':('input_voltage_v','input_current_a'),
      'sensors':('input_voltage_v','input_current_a')}),
 }},
}


def _declare(field,units,labels):
    return {label:{field:list(units)} for label in labels}


# Formats declared by a SEDS-SD header, by (format, version). Mappings use the
# legacy tuple layout; their options add 'declare' (labels the mapping needs,
# each with the column units it reads, width included) and 'optional' (more
# source labels, read only when declared, and then checked the same way).
# Median bins also need the declared tick rate (uptime_hz). Other declared
# labels and undeclared data labels are ignored. A firmware SD layout change
# bumps the version; register the new version here (docs/data-pipeline.md 3.5).
SD_FORMATS={
 ('daq',3):{'name':'daq-v3','board':'DAQ','mappings':{
  'kg1000_v':('raw_value',[0],'KG1000',['V'],{'median_bin':(3,20000),'uptime_hz':1000000,'sensors':('kg1000_v',),
      'declare':_declare('raw_value',['V'],('kg1000_v',))}),
  'kg50_v':('raw_value',[0],'KG50',['V'],{'median_bin':(5,50000),'uptime_hz':1000000,'sensors':('kg50_v','kg50_p6_v'),
      'declare':_declare('raw_value',['V'],('kg50_v',)),'optional':_declare('raw_value',['V'],('kg50_p6_v',))}),
  'kg1000_kg':('calibrated_value',[0],'LOADCELL_WEIGHT_KG',['kg'],{'sensors':('kg1000_v',),
      'declare':_declare('calibrated_value',['kg'],('kg1000_v',))}),
  'kg50_kg':('calibrated_value',[0],'LOADCELL_50_WEIGHT_KG',['kg'],{'sensors':('kg50_v',),
      'declare':_declare('calibrated_value',['kg'],('kg50_v',))}),
  'input_supply':(None,[0,1,2,3],'DAQ_POWER_MONITOR',['V','V','V','A'],{'pair':('input_voltage_v','input_current_a'),
      'sensors':('input_voltage_v','input_current_a'),
      'declare':{'input_voltage_v':{'raw_value':['V'],'calibrated_value':['V']},
                 'input_current_a':{'raw_value':['V'],'calibrated_value':['A']}}}),
 }},
 # FC rows hold the converted vector in both value and calibrated_value; the
 # header declares value. Accelerations are m/s^2 (FC docs gap 8).
 ('fc',2):{'name':'fc-v2','board':'FC','mappings':{
  'barometer_sample':('value',[2,0,1],'BAROMETER_DATA',['m','Pa','degC'],{'declare':_declare('value',['Pa','degC','m'],('barometer_sample',))}),
  'BARO':('value',[0,1,2],'BAROMETER_DATA',['m','Pa','degC'],{'declare':_declare('value',['m','Pa','degC'],('BARO',))}),
  'IMU':('value',[0,1,2,3,4,5],'IMU_DATA',['m/s2']*3+['deg/s']*3,{'declare':_declare('value',['m/s2']*3+['deg/s']*3,('IMU',))}),
  'gyroscope_sample':('value',[0,1,2],'GYRO_DATA',['deg/s']*3,{'declare':_declare('value',['deg/s']*3,('gyroscope_sample',))}),
  'accelerometer_sample':('value',[0,1,2],'ACCEL_DATA',['m/s2']*3,{'declare':_declare('value',['m/s2']*3,('accelerometer_sample',))}),
  'EKF':('value',[0,1,2,3,4,5],'ASCENT_STATE',['1']*4+['m','m/s'],{'declare':_declare('value',['1']*4+['m','m/s'],('EKF',))}),
  'DKF':('value',[0,1,2,3],'DESCENT_STATE',['deg','deg','m','m/s'],{'declare':_declare('value',['deg','deg','m','m/s'],('DKF',))}),
 }},
}


def _supported_formats_text():
    current=', '.join(f'{name} v{version}' for name,version in sorted(SD_FORMATS))
    legacy={}
    for name,version in sorted(set(LEGACY_SD_FORMATS.values())):legacy.setdefault(name,[]).append(f'v{version}')
    return current+' (and legacy headerless '+', '.join(f'{n} {"/".join(v)}' for n,v in legacy.items())+')'


def select_sd_format(block):
    """The registered format for a parsed block, or ValueError naming what is supported."""
    entry=SD_FORMATS.get((block['format'],block['version']))
    if entry is None:
        raise ValueError(f"SD file declares format {block['format']} v{block['version']}; this ground station reads "
                         f'{_supported_formats_text()}. Update the ground station.')
    if block['board']!=entry['board']:
        raise ValueError(f"SD format header names board {block['board']} for format {block['format']}; expected {entry['board']}")
    if block['columns']!=SD_COLUMN_TYPES:
        raise ValueError(f"SD file declares {block['format']} v{block['version']} but its # seds-sd-columns line is not that version's row layout")
    return entry


def _declared_sources(block,name,mapping):
    """Source labels a header-file mapping reads; ValueError if a needed label is
    not declared or declared with other columns, units, width or tick rate."""
    options=_options(mapping);sources=[];hz={}
    def check(label,columns,required):
        record=declared_record(block,label)
        if record is None:
            if required:
                raise ValueError(f'Mapping {name} needs SD record {label} declared with '
                    +', '.join(f'{field}={";".join(units)}' for field,units in columns.items())+'; the file does not declare it')
            return
        for field,units in columns.items():
            got=record['columns'].get(field)
            if got!=units:
                raise ValueError(f"Mapping {name} needs {label} {field}={';'.join(units)}; the file declares "
                                 +(f"{field}={';'.join(got)}" if got else f'no {field}')+f" ({record['entry']})")
        if options.get('uptime_hz') and record['tick_hz']!=options['uptime_hz']:
            raise ValueError(f"Mapping {name} needs {label} at {options['uptime_hz']} Hz ticks; the file declares {record['tick_hz']} Hz")
        sources.append(label);hz[label]=record['tick_hz']
    for label,columns in options.get('declare',{}).items():check(label,columns,True)
    for label,columns in options.get('optional',{}).items():check(label,columns,False)
    return tuple(label for label in options.get('sensors',(name,)) if label in sources),hz


# Width of the DAQ KG1000/KG50 network boxcar bins (DAQ_BROADCAST_PERIOD_MS);
# files that state loadcell_boxcar_ms on their # calibration line use theirs.
DAQ_BOXCAR_MS=4
UTC_MIN,UTC_MAX=946684800000,4102444800000
# DAQ KG1000/AMP2 KG50 legacy raw (code*2.2104/2^27) to ADC-pin volts
# (code*2.4/2^23): exactly 16000/921, as loadcell::LEGACY_RAW_TO_VOLTS.
LEGACY_RAW_TO_VOLTS=16000/921
LEGACY_LOADCELL_KINDS=('KG1000','KG50','DAQ_LOADCELL_CHAMBER')


def _f32(value):
    return struct.unpack('<f',struct.pack('<f',value))[0]


def _options(mapping):
    return mapping[4] if len(mapping)>4 else {}


class Boxcar:
    """daq_bin_average_add on SD raw records: the mean of each fixed bin of
    uptime ms (bin = uptime // period), summed in double precision in record
    order and rounded once to binary32. A bin is complete when a record of a
    later bin arrives. Non-finite values are skipped, as on the DAQ."""
    def __init__(self,period_ms):
        self.period=period_ms;self.reset()
    def reset(self):
        self.bin=None;self.sum=0.0;self.count=0;self.last=None;self.first=None
    def add(self,value,uptime_ms,sample):
        """Returns (mean, count, first sample, last sample) of a completed bin, or None."""
        value=_f32(value)
        if not math.isfinite(value):return None
        bin_=uptime_ms//self.period;done=None
        if self.count and bin_!=self.bin:
            done=(_f32(self.sum/self.count),self.count,self.first,self.last)
            self.sum=0.0;self.count=0;self.first=None
        self.bin=bin_;self.sum+=value;self.count+=1;self.last=sample
        if self.first is None:self.first=sample
        return done


class MedianBin:
    """daq_median_add then daq_bin_us_add on SD raw records: a running median
    of the last `size` finite binary32 values (fewer at the start; lower middle
    for an even count), then the mean of each fixed bin of uptime microseconds,
    summed in double precision in record order and rounded once to binary32.
    A bin is complete when a record of a later bin arrives; it is dated at the
    bin centre."""
    def __init__(self,size,period_us):
        self.size=size;self.period=period_us;self.reset()
    def reset(self):
        self.window=[];self.bin=None;self.sum=0.0;self.count=0;self.last=None;self.first=None
    def add(self,value,uptime_us,sample):
        """Returns (mean, count, first sample, last sample, centre_us) of a completed bin, or None."""
        value=_f32(value)
        if not math.isfinite(value):return None
        self.window=(self.window+[value])[-self.size:]
        median=sorted(self.window)[(len(self.window)-1)//2]
        bin_=uptime_us//self.period;done=None
        if self.count and bin_!=self.bin:
            done=(_f32(self.sum/self.count),self.count,self.first,self.last,self.bin*self.period+self.period//2)
            self.sum=0.0;self.count=0;self.first=None
        self.bin=bin_;self.sum+=median;self.count+=1;self.last=sample
        if self.first is None:self.first=sample
        return done


def _boxcar_ms(metadata):
    """loadcell_boxcar_ms from the file's # calibration lines (one value)."""
    found=set()
    for item in metadata or []:
        fields=item.get('fields') if isinstance(item,dict) else None
        if not fields or fields[0]!='# calibration':continue
        for field in fields[1:]:
            if field.startswith('loadcell_boxcar_ms='):
                text=field.split('=',1)[1]
                if not text.isdigit() or not 1<=int(text)<=1000:raise ValueError('Invalid loadcell_boxcar_ms')
                found.add(int(text))
    if len(found)>1:raise ValueError('One SD file states two load-cell boxcar widths')
    return found.pop() if found else DAQ_BOXCAR_MS


def legacy_raw_to_volts(raw):
    """Same binary32 result as loadcell::legacy_raw_to_volts in the backend."""
    return _f32(_f32(raw)*LEGACY_RAW_TO_VOLTS)


def _p6_payload(payload):
    # A P6 KG50 selection is tagged [source, volts] (8 bytes) and was always volts.
    try:
        return isinstance(payload,str) and len(json.loads(payload))==8
    except (ValueError,TypeError):
        return False


def _network_values_to_volts(kind,values,payload):
    """values_json of a network row recorded on the legacy raw scale, in volts."""
    if kind not in LEGACY_LOADCELL_KINDS or (kind=='KG50' and _p6_payload(payload)):
        return values
    try:
        vector=json.loads(values or '[]')
    except ValueError:
        return values
    if not isinstance(vector,list) or not vector or not isinstance(vector[0],(int,float)) or isinstance(vector[0],bool):
        return values
    vector[0]=legacy_raw_to_volts(vector[0])
    return canonical(vector)


def canonical_utc(value):
    return type(value) is int and UTC_MIN<=value<UTC_MAX


def _resolution_manifest(manifest,block=None):
    """Validates the manifest against the file's format block (None: headerless).
    Returns (assignment, profile, selected); profile carries the mappings, its
    name, the source evidence, the detected sd_format and, for header files,
    the declared source labels and tick rates of each mapping."""
    assignment=manifest.get('assignment',{})
    claimed=manifest.get('mapping_profile')
    if block is not None:
        entry=select_sd_format(block)
        if claimed is not None and claimed!=entry['name']:
            raise ValueError(f"Manifest mapping_profile {claimed} conflicts with the SD file's declared format "
                             f"{block['format']} v{block['version']} (profile {entry['name']}); omit mapping_profile or use {entry['name']}")
        sd_format={**sd_format_summary(block,manifest),'profile':entry['name']}
        if manifest.get('firmware_head') is not None:sd_format['manifest_firmware_head']=manifest['firmware_head']
        profile={'name':entry['name'],'mappings':entry['mappings'],'evidence':block['firmware'],'sd_format':sd_format,
                 'sources':{},'hz':{},'block':block}
    else:
        pinned=PROFILES.get(claimed) if isinstance(claimed,str) else None
        if pinned is None or manifest.get('firmware_head') not in pinned.get('heads',(pinned['head'],)):
            raise ValueError('Headerless (legacy) SD file: a supported versioned firmware mapping_profile and its exact '
                             'pinned firmware_head are required')
        profile={'name':claimed,'mappings':pinned['mappings'],'evidence':manifest['firmware_head'],
                 'sd_format':sd_format_summary(None,manifest),'sources':None,'hz':None,'block':None}
    if not isinstance(assignment,dict) or assignment.get('boot_and_session_verified') is not True:
        raise ValueError('Explicit verified board boot/session assignment required')
    if not isinstance(manifest.get('launch_session'),str) or not manifest['launch_session'].strip():
        raise ValueError('Explicit launch/test session identity required')
    sender=assignment.get('sender_id')
    if not isinstance(sender,str) or not sender or len(sender)>128:
        raise ValueError('Assigned network sender required')
    lo,hi=assignment.get('start_utc_ms'),assignment.get('end_utc_ms')
    if not canonical_utc(lo) or not canonical_utc(hi) or not lo<hi or hi-lo>7*86400000:
        raise ValueError('Explicit canonical UTC assignment range (at most seven days) required')
    schema=manifest['config'].get('schema')
    if not isinstance(schema,dict):
        raise ValueError('Frozen recorded config.schema required for replay mapping')
    # The relevant type shape must be in the recorded schema. Never consult a
    # current global schema to infer the historical type or vector width.
    definitions={item.get('name'):item for group in schema.values() if isinstance(group,list)
                 for item in group if isinstance(item,dict)}
    for item in manifest['config'].get('derived_channels',[]):
        if isinstance(item,dict): definitions[item.get('name')]=item
    selected=manifest.get('selected_sensors')
    if not isinstance(selected,list) or not selected or len(selected)>32 or any(not isinstance(sensor,str) for sensor in selected) or len(set(selected))!=len(selected):
        raise ValueError('Choose bounded sensor mappings explicitly')
    if any(not isinstance(sensor,str) for sensor in selected):
        raise ValueError('Sensor selections must be strings')
    mapped_kinds=[]
    for sensor in selected:
        if sensor not in profile['mappings']:
            raise ValueError('Unsupported sensor mapping: '+str(sensor))
        _,indices,kind,_=profile['mappings'][sensor][:4]
        if profile['block'] is not None:
            sources,hz=_declared_sources(profile['block'],sensor,profile['mappings'][sensor])
            profile['sources'][sensor]=sources;profile['hz'].update(hz)
        if kind in mapped_kinds: raise ValueError('Choose one SD source for each semantic channel')
        mapped_kinds.append(kind)
        element=definitions.get(kind,{}).get('element',{})
        if element.get('kind')!='Static' or element.get('data_type')!='Float32' or element.get('count')!=len(indices):
            raise ValueError('Recorded schema is incompatible with mapped '+kind)
    if assignment.get('units_axes_calibration_verified') is not True:
        raise ValueError('Units, axes and calibration equivalence must be verified')
    gap=assignment.get('max_gap_ms')
    if type(gap) is not int or not 1<=gap<=1000:
        raise ValueError('Explicit bounded coverage gap threshold (1..1000 ms) required')
    return assignment,profile,selected


def resolve_recording(db,max_database_bytes=MAX_DATABASE_BYTES):
    """Disk-backed full-resolution UTC view; original network/SD tables retained.

    CSV cells and candidates are never collected into a Python array. SQLite owns
    indexed coverage and external sorting. Only <=32 active channel states per
    source and one CSV/vector/provenance record are held by the resolver.
    """
    if db.in_transaction:raise ValueError('Resolver requires its own transaction')
    db.execute('PRAGMA temp_store=FILE')
    db.executescript("""
      DROP TABLE IF EXISTS sd_resolution_candidates;
      CREATE TABLE sd_resolution_candidates(timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,
        values_json TEXT,payload_json TEXT,provenance_json TEXT,sort_key INTEGER,
        board_id TEXT,boot_id TEXT,launch_session TEXT);
      CREATE UNIQUE INDEX sd_candidate_physical ON sd_resolution_candidates(sort_key,sender_id,data_type,board_id,boot_id,COALESCE(launch_session,'')) WHERE board_id IS NOT NULL;
      DROP TABLE IF EXISTS sd_coverage_segments;
      CREATE TABLE sd_coverage_segments(id INTEGER PRIMARY KEY,import_id INTEGER,sender_id TEXT,data_type TEXT,
        start_ms INTEGER,end_ms INTEGER,board_id TEXT,boot_id TEXT,launch_session TEXT);
      CREATE INDEX sd_coverage_lookup ON sd_coverage_segments(sender_id,data_type,start_ms,end_ms);
      DROP TABLE IF EXISTS sd_sequence_holes;
      CREATE TABLE sd_sequence_holes(import_id INTEGER,start_ms INTEGER,end_ms INTEGER);
      CREATE INDEX sd_hole_lookup ON sd_sequence_holes(import_id,start_ms,end_ms);
    """)
    unresolved=suppressed=processed=0;sd_formats=[]
    # The resolved view serves every load cell in ADC-pin volts. Network rows
    # before the recording's loadcell_units mark (or all of them without it)
    # were stored on the DAQ legacy raw scale.
    marked=db.execute("SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='loadcell_units'").fetchone()
    first_volts_id=(db.execute('SELECT first_volts_id FROM main.loadcell_units WHERE id=1').fetchone() or [math.inf])[0] if marked else math.inf
    db.execute('BEGIN IMMEDIATE')
    try:
        for import_id,encoded in db.execute('SELECT id,manifest_json FROM sd_imports ORDER BY id'):
            manifest=json.loads(encoded)
            metadata_json=db.execute('SELECT metadata_json FROM sd_imports WHERE id=?',(import_id,)).fetchone()[0]
            assignment,profile,selected=_resolution_manifest(manifest,format_block_from_metadata(json.loads(metadata_json)))
            sd_formats.append({'import_id':import_id,**profile['sd_format']})
            lo,hi=assignment['start_utc_ms'],assignment['end_utc_ms'];previous_record=None
            records=db.execute('SELECT s.record_sequence,s.timestamp_ms,s.time_source FROM sd_sample_origins o '
                'JOIN sd_samples s ON s.id=o.sample_id WHERE o.import_id=? ORDER BY o.record_index',(import_id,))
            for sequence,stamp,time_source in records:
                seq=int(sequence)
                if previous_record and seq!=previous_record[0] and seq not in ((previous_record[0]+1)%2**32,(previous_record[0]+1)%2**64):
                    if time_source=='network' and previous_record[2]=='network':
                        db.execute('INSERT INTO sd_sequence_holes VALUES(?,?,?)',(import_id,min(stamp,previous_record[1]),max(stamp,previous_record[1])))
                previous_record=(seq,stamp,time_source)
            previous={}
            by_sensor={}
            for name in selected:
                sources=profile['sources'][name] if profile['sources'] is not None else _options(profile['mappings'][name]).get('sensors',(name,))
                for source_sensor in sources:
                    by_sensor.setdefault(source_sensor,[]).append(name)
            boxcars={};pending_pair={};boxcar_ms=None

            def emit(name,stamp,values,sample,extra=None,ambiguous=False):
                nonlocal unresolved,processed
                field,indices,kind,units=profile['mappings'][name][:4];key=(assignment['sender_id'],kind)
                if stamp is None or not canonical_utc(stamp) or not lo<=stamp<hi or ambiguous:
                    previous.pop(key,None);unresolved+=1;return
                state=previous.get(key)
                continuous=state and state[1]<=stamp and stamp-state[1]<=assignment['max_gap_ms']
                if continuous:
                    continuous=not db.execute('SELECT 1 FROM sd_sequence_holes WHERE import_id=? AND start_ms<? AND end_ms>? LIMIT 1',(import_id,stamp,state[1])).fetchone()
                if continuous:
                    segment_id=state[0];db.execute('UPDATE sd_coverage_segments SET end_ms=? WHERE id=?',(stamp,segment_id))
                else:
                    segment_id=db.execute('INSERT INTO sd_coverage_segments(import_id,sender_id,data_type,start_ms,end_ms,board_id,boot_id,launch_session) VALUES(?,?,?,?,?,?,?,?)',
                        (import_id,key[0],kind,stamp,stamp,manifest['board_id'],manifest['boot_id'],manifest.get('launch_session'))).lastrowid
                previous[key]=(segment_id,stamp)
                provenance={k:v for k,v in sample.items() if k not in ('config','calibration_metadata')}
                provenance.update(origin='sd',sd_sample_id=sample['sample_id'],received_timestamp_ms=None,
                    source_timestamp_ms=int(sample['fields']['timestamp_ms']),coverage_segment_id=segment_id,
                    mapping={'profile':profile['name'],'sensor':name,'field':field,'indices':indices,
                             'data_type':kind,'units':['V' if u=='sensor_raw' else u for u in units],'source_evidence':profile['evidence'],
                             'sd_format':profile['sd_format']})
                if 'sensor_raw' in units:provenance['mapping']['converted_from']={'units':'legacy_raw','volts_per_raw':'16000/921'}
                if extra:provenance['mapping'].update(extra)
                processed+=1
                if processed%4096==0:check_disk_limit(db,max_database_bytes)
                db.execute('INSERT OR IGNORE INTO sd_resolution_candidates VALUES(?,?,?,?,?,?,?,?,?,?)',
                    (stamp,key[0],kind,canonical(values),None,canonical(provenance),sample['sample_id'],
                     manifest['board_id'],manifest['boot_id'],manifest.get('launch_session')))

            def number(text):
                value=float(text)
                return value if math.isfinite(value) else None

            for sample in iter_sd_rows(db,[import_id]):
                fields=sample['fields'];sensor=sample['sensor']
                for name in by_sensor.get(sensor,()):
                    mapping=profile['mappings'][name];field,indices,kind,units=mapping[:4];options=_options(mapping)
                    stamp=sample['utc_timestamp_ms']
                    try:
                        expected_hz=profile['hz'][sensor] if profile['hz'] is not None else options.get('uptime_hz',1000)
                        if int(fields['uptime_frequency_hz'])!=expected_hz:raise ValueError('Uptime scale')
                        if options.get('median_bin'):
                            size,period=options['median_bin']
                            box=boxcars.get(name)
                            if box is None:box=boxcars[name]=[MedianBin(size,period),sensor]
                            if box[1]!=sensor:box[0].reset();box[1]=sensor
                            done=box[0].add(float(fields[field]),int(fields['uptime_ticks']),sample)
                            if done:
                                mean,count,first,last,centre_us=done
                                # Dated at the bin centre, like the DAQ's packet.
                                age_ms=int(last['fields']['uptime_ticks'])//1000-centre_us//1000
                                stamp=last['utc_timestamp_ms']
                                emit(name,None if stamp is None else stamp-age_ms,[mean if math.isfinite(mean) else None],last,
                                     {'derived':'median_bin','median':size,'bin_us':period,'records':count,'source_sensor':last['sensor'],
                                      'first_sd_sample_id':first['sample_id'],'last_sd_sample_id':last['sample_id']},last['ambiguous'])
                            continue
                        if options.get('boxcar'):
                            # Accumulate every record whatever its clock; a bin is
                            # dated by its last record when it completes.
                            if boxcar_ms is None:boxcar_ms=_boxcar_ms(sample['calibration_metadata'])
                            box=boxcars.get(name)
                            if box is None:box=boxcars[name]=[Boxcar(boxcar_ms),sensor]
                            if box[1]!=sensor:box[0].reset();box[1]=sensor
                            done=box[0].add(float(fields[field]),int(fields['uptime_ticks']),sample)
                            if done:
                                mean,count,first,last=done
                                emit(name,last['utc_timestamp_ms'],[mean if math.isfinite(mean) else None],last,
                                     {'derived':'boxcar','boxcar_ms':boxcar_ms,'records':count,'source_sensor':last['sensor'],
                                      'first_sd_sample_id':first['sample_id'],'last_sd_sample_id':last['sample_id']},last['ambiguous'])
                            continue
                        if options.get('pair'):
                            voltage,current=options['pair']
                            if sensor==voltage:
                                pending_pair[name]=sample;continue
                            first=pending_pair.pop(name,None)
                            if first is None or first['fields']['uptime_ticks']!=fields['uptime_ticks']:
                                unresolved+=1;continue
                            values=[number(first['fields']['raw_value']),number(fields['raw_value']),
                                    number(first['fields']['calibrated_value']),number(fields['calibrated_value'])]
                            emit(name,first['utc_timestamp_ms'],values,first,
                                 {'derived':'pair','paired_sd_sample_id':sample['sample_id']},first['ambiguous'] or sample['ambiguous'])
                            continue
                        vector=[float(x) for x in fields[field].split()]
                        values=[vector[i] if math.isfinite(vector[i]) else None for i in indices]
                        if len(vector)!=len(indices):raise ValueError('Vector width mismatch')
                        if 'sensor_raw' in units:
                            values=[None if v is None else legacy_raw_to_volts(v) for v in values]
                    except (ValueError,IndexError,KeyError):
                        previous.pop((assignment['sender_id'],kind),None)
                        if name in boxcars:boxcars[name][0].reset()
                        unresolved+=1;continue
                    emit(name,stamp,values,sample,ambiguous=sample['ambiguous'])
        for row_id,received,source,sender,kind,values,payload in db.execute('SELECT id,timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json FROM main.telemetry ORDER BY id'):
            if not canonical_utc(source):unresolved+=1;continue
            identities=db.execute("SELECT DISTINCT board_id,boot_id,launch_session FROM sd_coverage_segments s WHERE sender_id=? AND data_type=? AND start_ms<=? AND end_ms>=? AND NOT EXISTS(SELECT 1 FROM sd_sequence_holes h WHERE h.import_id=s.import_id AND h.start_ms<=? AND h.end_ms>=?) LIMIT 2",(sender,kind,source,source,source,source)).fetchall()
            if len(identities)==1:suppressed+=1;continue
            provenance={'origin':'network','original_network_id':row_id,'received_timestamp_ms':received,
                        'source_timestamp_ms':source,'utc_timestamp_ms':source,'ambiguous':len(identities)>1}
            if row_id<first_volts_id:
                converted=_network_values_to_volts(kind,values,payload)
                if converted!=values:
                    provenance['converted_from']={'units':'legacy_raw','volts_per_raw':'16000/921','values_json':values}
                    values=converted
            db.execute('INSERT INTO sd_resolution_candidates VALUES(?,?,?,?,?,?,?,?,?,?)',
                (source,sender,kind,values or '[]',payload,canonical(provenance),row_id,None,None,None))
        # Overlapping unproven boot assignments remain separately labelled SD
        # series while the canonical network channel remains available.
        db.execute("""UPDATE sd_resolution_candidates AS c SET
            sender_id=sender_id||' [SD '||boot_id||']',
            provenance_json=json_set(provenance_json,'$.ambiguous',json('true'))
            WHERE board_id IS NOT NULL AND EXISTS(SELECT 1 FROM sd_coverage_segments s
              WHERE s.sender_id=c.sender_id AND s.data_type=c.data_type AND s.start_ms<=c.timestamp_ms AND s.end_ms>=c.timestamp_ms
              AND (s.board_id!=c.board_id OR s.boot_id!=c.boot_id OR COALESCE(s.launch_session,'')!=COALESCE(c.launch_session,'')))""")
        db.execute('DROP TABLE IF EXISTS sd_replay_telemetry')
        db.execute('CREATE TABLE sd_replay_telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER NOT NULL,source_timestamp_ms INTEGER,data_type TEXT NOT NULL,sender_id TEXT,values_json TEXT,payload_json TEXT,provenance_json TEXT NOT NULL)')
        db.execute("""INSERT INTO sd_replay_telemetry SELECT ROW_NUMBER() OVER(ORDER BY timestamp_ms,sender_id,data_type,sort_key),timestamp_ms,
            json_extract(provenance_json,'$.source_timestamp_ms'),data_type,sender_id,values_json,payload_json,provenance_json FROM sd_resolution_candidates""")
        db.execute("CREATE INDEX sd_replay_sample_origin ON sd_replay_telemetry(json_extract(provenance_json,'$.sd_sample_id'))")
        db.execute('CREATE INDEX sd_replay_time ON sd_replay_telemetry(timestamp_ms)')
        db.execute('CREATE INDEX sd_replay_series ON sd_replay_telemetry(data_type,sender_id,timestamp_ms)')
        check_disk_limit(db,max_database_bytes)
        count=db.execute('SELECT COUNT(*) FROM sd_replay_telemetry').fetchone()[0]
        segments=db.execute('SELECT COUNT(*) FROM sd_coverage_segments').fetchone()[0]
        db.execute('DROP TABLE sd_resolution_candidates')
        # Marks the resolved view as ADC-pin volts throughout (telemetry_db::loadcell_volts_from_id).
        db.execute('DROP TABLE IF EXISTS sd_replay_loadcell_units')
        db.execute("CREATE TABLE sd_replay_loadcell_units(id INTEGER PRIMARY KEY CHECK(id=1),unit TEXT NOT NULL)")
        db.execute("INSERT INTO sd_replay_loadcell_units VALUES(1,'adc_volts')")
        db.execute('CREATE TABLE IF NOT EXISTS sd_resolution_manifest(id INTEGER PRIMARY KEY CHECK(id=1),manifest_json TEXT NOT NULL)')
        summary={'format':'seds-sd-resolution','version':1,'axis':'canonical_utc','coverage_segments':segments,
                 'coverage_table':'sd_coverage_segments','network_suppressed_in_resolved_view':suppressed,
                 'unresolved_samples':unresolved,'rows':count,'original_network_preserved':True,'sd_formats':sd_formats}
        db.execute('INSERT OR REPLACE INTO sd_resolution_manifest VALUES(1,?)',(canonical(summary),))
        db.commit();return summary
    except Exception:
        db.rollback();raise


def worker():
    import sys
    import tempfile
    request=json.load(sys.stdin)
    manifest=request['manifest']
    source=Path(request['csv_path'])
    # Select by the file's declared format (or the legacy pins) before copying.
    _,profile,_=_resolution_manifest(manifest,read_format_block(source))
    global MINIMUM_FREE_BYTES
    MINIMUM_FREE_BYTES=request.get('minimum_free_bytes',0)
    create_derived_recording(request['source'],request['output'])
    with closing(sqlite3.connect(request['output'])) as db:
        imported=import_csv(db,source,manifest,max_bytes=request['max_bytes'],max_database_bytes=request['max_database_bytes'])
        result=resolve_recording(db,request['max_database_bytes']);result['import']=imported
        result['sd_format']=profile['sd_format']
    print(canonical(result))
