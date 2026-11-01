"""Explicit semantic coverage resolver for imported derived recordings.

No receipt timestamps participate in matching. Profiles are pinned to inspected
firmware source identities; callers provide the recorded schema and boot/session
assignment. Unsupported formats remain in raw SD storage for explicit selection.
"""
import math

FC_HEAD='ec188c6daac84e3c957f57ca95d872953cbda588'
DAQ_HEAD='ef3702d87071f204e5ccb9a124b739af82538ac2'
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
  'kg50_raw':('calibrated_value',[0],'LOADCELL_50_WEIGHT_KG',['kg']),
  'mcp3564r_raw':('calibrated_value',[0],'LOADCELL_WEIGHT_KG',['kg']),
  'kg50_network':('value',[0],'KG50',['sensor_raw']),
  'kg1000_network':('value',[0],'KG1000',['sensor_raw']),
 }}
}
UTC_MIN,UTC_MAX=946684800000,4102444800000


def canonical_utc(value):
    return type(value) is int and UTC_MIN<=value<UTC_MAX


def _resolution_manifest(manifest):
    assignment=manifest.get('assignment',{})
    profile=PROFILES.get(manifest.get('mapping_profile'))
    if profile is None or manifest.get('firmware_head')!=profile['head']:
        raise ValueError('A supported versioned firmware mapping and exact source identity are required')
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
        _,indices,kind,_=profile['mappings'][sensor]
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
    unresolved=suppressed=processed=0
    db.execute('BEGIN IMMEDIATE')
    try:
        for import_id,encoded in db.execute('SELECT id,manifest_json FROM sd_imports ORDER BY id'):
            manifest=json.loads(encoded);assignment,profile,selected=_resolution_manifest(manifest)
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
            for sample in iter_sd_rows(db,[import_id]):
                fields=sample['fields'];sensor=sample['sensor']
                if sensor not in selected:continue
                field,indices,kind,units=profile['mappings'][sensor];key=(assignment['sender_id'],kind)
                stamp=sample['utc_timestamp_ms']
                if stamp is None or not canonical_utc(stamp) or not lo<=stamp<hi or sample['ambiguous'] or int(fields['uptime_frequency_hz'])!=1000:
                    previous.pop(key,None);unresolved+=1;continue
                try:
                    vector=[float(x) for x in fields[field].split()]
                    values=[vector[i] if math.isfinite(vector[i]) else None for i in indices]
                    if len(vector)!=len(indices):raise ValueError('Vector width mismatch')
                except (ValueError,IndexError):
                    previous.pop(key,None);unresolved+=1;continue
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
                    source_timestamp_ms=int(fields['timestamp_ms']),coverage_segment_id=segment_id,
                    mapping={'profile':manifest['mapping_profile'],'sensor':sensor,'field':field,'indices':indices,
                             'data_type':kind,'units':units,'source_evidence':profile['head']})
                processed+=1
                if processed%4096==0:check_disk_limit(db,max_database_bytes)
                db.execute('INSERT OR IGNORE INTO sd_resolution_candidates VALUES(?,?,?,?,?,?,?,?,?,?)',
                    (stamp,key[0],kind,canonical(values),None,canonical(provenance),sample['sample_id'],
                     manifest['board_id'],manifest['boot_id'],manifest.get('launch_session')))
        for row_id,received,source,sender,kind,values,payload in db.execute('SELECT id,timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json FROM main.telemetry ORDER BY id'):
            if not canonical_utc(source):unresolved+=1;continue
            identities=db.execute("SELECT DISTINCT board_id,boot_id,launch_session FROM sd_coverage_segments s WHERE sender_id=? AND data_type=? AND start_ms<=? AND end_ms>=? AND NOT EXISTS(SELECT 1 FROM sd_sequence_holes h WHERE h.import_id=s.import_id AND h.start_ms<=? AND h.end_ms>=?) LIMIT 2",(sender,kind,source,source,source,source)).fetchall()
            if len(identities)==1:suppressed+=1;continue
            provenance={'origin':'network','original_network_id':row_id,'received_timestamp_ms':received,
                        'source_timestamp_ms':source,'utc_timestamp_ms':source,'ambiguous':len(identities)>1}
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
        db.execute('CREATE TABLE IF NOT EXISTS sd_resolution_manifest(id INTEGER PRIMARY KEY CHECK(id=1),manifest_json TEXT NOT NULL)')
        summary={'format':'seds-sd-resolution','version':1,'axis':'canonical_utc','coverage_segments':segments,
                 'coverage_table':'sd_coverage_segments','network_suppressed_in_resolved_view':suppressed,
                 'unresolved_samples':unresolved,'rows':count,'original_network_preserved':True}
        db.execute('INSERT OR REPLACE INTO sd_resolution_manifest VALUES(1,?)',(canonical(summary),))
        db.commit();return summary
    except Exception:
        db.rollback();raise


def worker():
    import sys
    import tempfile
    request=json.load(sys.stdin)
    manifest=request['manifest']
    _resolution_manifest(manifest)
    global MINIMUM_FREE_BYTES
    MINIMUM_FREE_BYTES=request.get('minimum_free_bytes',0)
    source=Path(request['csv_path'])
    create_derived_recording(request['source'],request['output'])
    with closing(sqlite3.connect(request['output'])) as db:
        imported=import_csv(db,source,manifest,max_bytes=request['max_bytes'],max_database_bytes=request['max_database_bytes'])
        result=resolve_recording(db,request['max_database_bytes']);result['import']=imported
    print(canonical(result))
