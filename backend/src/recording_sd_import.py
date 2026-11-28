"""Private offline SD import candidate. Does not open hardware or alter telemetry.

Call create_derived_recording(source, NEW_destination), then import_csv(db, file,
manifest). Manifest identities are supplied from independent recording evidence;
filenames are not evidence of boot or board identity. HTTP/UI wiring is pending.
"""
import csv
from contextlib import closing
import hashlib
import io
import json
import os
from pathlib import Path
import sqlite3

FIELDS = ('timestamp_ms monotonic_ms sensor value raw_adc_code raw_value '
          'calibrated_value time_source adc_temperature_c adc_temperature_code '
          'uptime_ticks uptime_frequency_hz record_sequence').split()
MAX_FILE_BYTES = 3 * 1024 * 1024 * 1024
MAX_FIELD_BYTES = 8192
MAX_PHYSICAL_LINE_CHARS = len(FIELDS)*MAX_FIELD_BYTES*2+4096

def bounded_csv_lines(text):
    # csv.reader otherwise asks TextIO for an unbounded physical line. A hostile
    # comma-only or unterminated record must fail before allocating GB of cells.
    while True:
        line=text.readline(MAX_PHYSICAL_LINE_CHARS+1)
        if not line:return
        if len(line)>MAX_PHYSICAL_LINE_CHARS:raise ValueError("CSV physical line exceeds versioned format bounds")
        yield line

# SEDS-SD format block (lines 2.. of a board SD CSV, before any other '#' line):
#   # seds-sd,format=<id>,version=<n>,board=<BOARD>,firmware=<rev>,tick_hz=<hz>,schema=<8hex>
#   # seds-sd-columns,<type[:unit]> x13
#   # seds-sd-records,<entry>,...        (one or more lines)
# schema = first 8 hex of SHA-256 over the columns line and every records line,
# each as its text plus CRLF, in file order. Files without the block are legacy
# (mapping_profile + pinned firmware_head).
SD_FORMAT_LINE = '# seds-sd'
SD_COLUMNS_LINE = '# seds-sd-columns'
SD_RECORDS_LINE = '# seds-sd-records'
SD_COLUMN_ALIASES = {'v': 'value', 'a': 'raw_adc_code', 'r': 'raw_value', 'c': 'calibrated_value'}
SD_COLUMN_TYPES = ('u64:ms,u64:ms,label,vec,vec,vec,vec,clock,f32:degC,i32:code,'
                   'u64:tick,u32:Hz,u64:seq').split(',')
# Headerless files: the legacy format each pinned mapping profile reads.
LEGACY_SD_FORMATS = {'daq-20261101': ('daq', 1), 'daq-20261117': ('daq', 2),
                     'daq-20261123': ('daq', 2), 'fc-20261101': ('fc', 1)}


def _format_line_text(line):
    if isinstance(line, (list, tuple)):
        return ','.join(line)
    if isinstance(line, bytes):
        line = line.decode('utf-8')
    return line.rstrip('\r\n')


def _sd_units(text, where):
    units = []
    for item in text.split(';'):
        name, star, count = item.partition('*')
        if not name or (star and (not count.isdigit() or not 1 <= int(count) <= 64)):
            raise ValueError(f'SD format header damaged: bad units {text!r} in {where}')
        units += [name] * (int(count) if star else 1)
    return units


def _sd_record_entry(entry):
    body, at, tick = entry.partition('@')
    if at and (not tick.isdigit() or not 0 < int(tick) < 2**32):
        raise ValueError(f'SD format header damaged: bad tick rate in record entry {entry!r}')
    label, *columns = body.split(':')
    if not label or not columns or label == '*':
        raise ValueError(f'SD format header damaged: bad record entry {entry!r}')
    declared = {}
    for column in columns:
        alias, equals, units = column.partition('=')
        field = SD_COLUMN_ALIASES.get(alias)
        if not equals or field is None or field in declared:
            raise ValueError(f'SD format header damaged: bad column {column!r} in record entry {entry!r}')
        declared[field] = _sd_units(units, entry)
    prefix = label.endswith('*')
    return (label[:-1] if prefix else label), {'columns': declared, 'tick_hz': int(tick) if at else None,
                                               'prefix': prefix, 'entry': entry}


def parse_format_block(lines):
    """Parse and verify the SEDS-SD format block.

    `lines` are the CSV lines after the 13-column header, in file order (text or
    bytes with or without line endings, or csv field lists). Only the leading
    run matters; pass at least the lines up to the first data row. Returns None
    for a headerless (legacy) file. Raises ValueError for a malformed block or a
    schema hash that does not match. The result:
      {format, version, board, firmware, tick_hz, schema, columns: [13 str],
       records: {label: {columns: {field: [unit, ...]}, tick_hz: int (resolved),
                         prefix: bool, entry: str}}, legacy: False}
    """
    texts = []
    for line in lines:
        text = _format_line_text(line)
        if not text.startswith('#'):
            break
        texts.append(text)
    if not texts or texts[0].split(',', 1)[0] != SD_FORMAT_LINE:
        if any(text.startswith(SD_FORMAT_LINE) for text in texts):
            raise ValueError('SD format header damaged: format lines are not right after the CSV header')
        return None
    fields = {}
    for item in texts[0].split(',')[1:]:
        key, equals, value = item.partition('=')
        if not equals or not key or key in fields:
            raise ValueError(f'SD format header damaged: bad field {item!r}')
        fields[key] = value
    missing = [k for k in ('format', 'version', 'board', 'firmware', 'tick_hz', 'schema') if k not in fields]
    if missing:
        raise ValueError('SD format header damaged: missing ' + ', '.join(missing))
    if not fields['version'].isdigit() or not fields['tick_hz'].isdigit() or not 0 < int(fields['tick_hz']) < 2**32:
        raise ValueError('SD format header damaged: version and tick_hz must be decimal integers')
    schema = fields['schema']
    if len(schema) != 8 or any(ch not in '0123456789abcdef' for ch in schema):
        raise ValueError('SD format header damaged: schema must be 8 lowercase hex digits')
    if len(texts) < 2 or texts[1].split(',', 1)[0] != SD_COLUMNS_LINE:
        raise ValueError('SD format header damaged: missing # seds-sd-columns line')
    count = 2
    while count < len(texts) and texts[count].split(',', 1)[0] == SD_RECORDS_LINE:
        count += 1
    if count == 2:
        raise ValueError('SD format header damaged: missing # seds-sd-records line')
    if any(text.startswith(SD_FORMAT_LINE) for text in texts[count:]):
        raise ValueError('SD format header damaged: stray # seds-sd line after the record entries')
    digest = hashlib.sha256(''.join(text + '\r\n' for text in texts[1:count]).encode()).hexdigest()[:8]
    if digest != schema:
        raise ValueError(f'SD format header damaged: schema hash mismatch (declared {schema}, computed {digest})')
    tick_hz = int(fields['tick_hz'])
    records = {}
    for text in texts[2:count]:
        for entry in text.split(',')[1:]:
            label, record = _sd_record_entry(entry)
            key = label + ('*' if record['prefix'] else '')
            if key in records:
                raise ValueError(f'SD format header damaged: record {key} declared twice')
            if record['tick_hz'] is None:
                record['tick_hz'] = tick_hz
            records[key] = record
    return {'format': fields['format'], 'version': int(fields['version']), 'board': fields['board'],
            'firmware': fields['firmware'], 'tick_hz': tick_hz, 'schema': schema,
            'columns': texts[1].split(',')[1:], 'records': records, 'legacy': False}


def declared_record(block, label):
    """The record entry that declares `label` (exact, else longest prefix pattern)."""
    record = block['records'].get(label)
    if record is not None and not record['prefix']:
        return record
    best = None
    for key, record in block['records'].items():
        if record['prefix'] and label.startswith(key[:-1]) and (best is None or len(key) > len(best[0])):
            best = (key, record)
    return best[1] if best else None


def format_block_from_metadata(metadata):
    """parse_format_block over the '#' rows import_csv stored (leading run only)."""
    items = sorted((item for item in metadata or [] if isinstance(item, dict) and 'fields' in item),
                   key=lambda item: item['record_index'])
    leading = []
    for position, item in enumerate(items, 1):
        if item['record_index'] != position:
            if any(','.join(rest['fields']).startswith(SD_FORMAT_LINE) for rest in items[position-1:]):
                raise ValueError('SD format header damaged: format lines are not right after the CSV header')
            break
        leading.append(item['fields'])
    return parse_format_block(leading)


def read_format_block(path, max_rows=1024):
    """Peek a CSV's format block (None if headerless or not a 13-column SD CSV)."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    csv.field_size_limit(MAX_FIELD_BYTES)
    with io.TextIOWrapper(os.fdopen(fd, 'rb'), encoding='utf-8-sig', newline='') as text:
        reader = csv.reader(bounded_csv_lines(text), strict=True)
        if next(reader, None) != FIELDS:
            return None
        rows = []
        for row in reader:
            if not row or not row[0].startswith('#') or len(rows) >= max_rows:
                break
            rows.append(row)
    return parse_format_block(rows)


def sd_format_summary(block, manifest):
    """Detected format for results and provenance."""
    if block is not None:
        return {'format': block['format'], 'version': block['version'], 'board': block['board'],
                'firmware': block['firmware'], 'schema': block['schema'], 'tick_hz': block['tick_hz'],
                'legacy': False}
    profile = manifest.get('mapping_profile') if isinstance(manifest, dict) else None
    legacy = LEGACY_SD_FORMATS.get(profile) if isinstance(profile, str) else None
    return {'format': legacy[0] if legacy else None, 'version': legacy[1] if legacy else None,
            'firmware': manifest.get('firmware_head') if legacy else None, 'schema': None,
            'profile': profile if legacy else None, 'legacy': True}


MAX_ROWS = 50_000_000
MAX_DATABASE_BYTES = 32 * 1024**3
MINIMUM_FREE_BYTES = 0

def check_disk_limit(db, maximum=MAX_DATABASE_BYTES):
    pages=db.execute("PRAGMA page_count").fetchone()[0]
    size=db.execute("PRAGMA page_size").fetchone()[0]
    if MINIMUM_FREE_BYTES:
        import shutil
        path=db.execute("PRAGMA database_list").fetchone()[2]
        if shutil.disk_usage(Path(path).parent).free<MINIMUM_FREE_BYTES:raise ValueError("Insufficient free disk; recovery stopped without changing original recording")
    if pages*size>maximum:raise ValueError("Derived recording exceeds configured disk budget")

SCHEMA = '''
CREATE TABLE IF NOT EXISTS sd_imports(
 id INTEGER PRIMARY KEY, file_sha256 TEXT NOT NULL, filename TEXT NOT NULL,
 board_id TEXT NOT NULL, boot_id TEXT NOT NULL, stream_id TEXT NOT NULL,
 launch_session TEXT, schema_version TEXT NOT NULL, config_json TEXT NOT NULL,
 metadata_json TEXT NOT NULL, manifest_json TEXT NOT NULL,
 UNIQUE(file_sha256,board_id,boot_id,stream_id));
CREATE TABLE IF NOT EXISTS sd_samples(
 id INTEGER PRIMARY KEY, identity_key TEXT NOT NULL, content_sha256 TEXT NOT NULL,
 sensor TEXT NOT NULL, data_type TEXT, timestamp_ms INTEGER NOT NULL,
 monotonic_ms INTEGER NOT NULL, time_source TEXT NOT NULL,
 uptime_ticks TEXT NOT NULL, uptime_frequency_hz INTEGER NOT NULL,
 record_sequence TEXT NOT NULL, fields_json TEXT NOT NULL,
 UNIQUE(identity_key,content_sha256));
CREATE TABLE IF NOT EXISTS sd_sample_origins(
 import_id INTEGER NOT NULL REFERENCES sd_imports(id),
 record_index INTEGER NOT NULL, sample_id INTEGER NOT NULL REFERENCES sd_samples(id),
 PRIMARY KEY(import_id,record_index));
CREATE INDEX IF NOT EXISTS sd_identity_index ON sd_samples(identity_key);
CREATE INDEX IF NOT EXISTS sd_origin_sample_index ON sd_sample_origins(sample_id,import_id,record_index);
CREATE INDEX IF NOT EXISTS sd_origin_import_sample ON sd_sample_origins(import_id,sample_id,record_index);
CREATE TABLE IF NOT EXISTS sd_source_chunks(file_sha256 TEXT NOT NULL,offset INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(file_sha256,offset));
-- Populate ONLY when the original network capture proves this identity. Existing
-- telemetry does not provide it. A row cannot acquire multiple candidate identities.
CREATE TABLE IF NOT EXISTS sd_network_identity(
 telemetry_id INTEGER PRIMARY KEY REFERENCES telemetry(id),
 identity_key TEXT NOT NULL, evidence_json TEXT NOT NULL);
DROP VIEW IF EXISTS sd_exact_matches;
CREATE VIEW sd_exact_matches AS
 SELECT n.telemetry_id, MIN(s.id) AS sample_id, n.identity_key
 FROM sd_network_identity n JOIN sd_samples s USING(identity_key)
 JOIN telemetry t ON t.id=n.telemetry_id
 GROUP BY n.identity_key
 HAVING COUNT(DISTINCT n.telemetry_id)=1 AND COUNT(DISTINCT s.id)=1;
DROP VIEW IF EXISTS sd_resolved_samples;
CREATE VIEW sd_resolved_samples AS
 SELECT 'network:'||t.id AS sample_key,'network' AS origin,t.id AS network_id,
 NULL AS sd_sample_id,t.timestamp_ms AS received_timestamp_ms,
 t.source_timestamp_ms AS source_timestamp_ms,
 NULL AS utc_timestamp_ms,t.sender_id,t.data_type,t.values_json,
 NULL AS fields_json,NULL AS identity_key,0 AS ambiguous
 FROM telemetry t WHERE NOT EXISTS(
 SELECT 1 FROM sd_exact_matches m WHERE m.telemetry_id=t.id)
 UNION ALL
 SELECT 'sd:'||s.id,'sd',NULL,s.id,NULL,s.timestamp_ms,
 CASE WHEN s.time_source='network' THEN s.timestamp_ms ELSE NULL END,
 NULL,s.data_type,NULL,s.fields_json,s.identity_key,
 (SELECT COUNT(*)>1 FROM sd_samples c WHERE c.identity_key=s.identity_key)
 FROM sd_samples s;
'''


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False,
                      allow_nan=False)


def _file(path, max_bytes):
    path = Path(path)
    # Refuse symlinks, special files, and directories. O_NOFOLLOW closes the leaf
    # lstat/open race; read at most the configured bound even if a file grows.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        import stat
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_size > max_bytes:
            raise ValueError('Expected a regular CSV file within the byte limit')
        with os.fdopen(fd, 'rb', closefd=False) as handle:
            data = handle.read(max_bytes + 1)
        if len(data) > max_bytes:
            raise ValueError('CSV exceeds byte limit')
        return data
    finally:
        os.close(fd)


def create_derived_recording(source, destination):
    """SQLite snapshot, including WAL, to a new destination. Source is read-only."""
    source = Path(source)
    if source.is_symlink() or not source.is_file():
        raise ValueError('Source must be a regular recording')
    source = source.resolve(strict=True)
    destination = Path(destination)
    fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    os.close(fd)
    try:
        with closing(sqlite3.connect(source.as_uri() + '?mode=ro', uri=True)) as src:
            with closing(sqlite3.connect(destination)) as dst:
                src.backup(dst)
                dst.execute('SELECT id FROM telemetry LIMIT 1')
    except Exception:
        destination.unlink()
        raise


def _manifest(manifest):
    # Require frozen config even when empty; no fallback to current station config.
    for key in ('board_id', 'boot_id', 'stream_id'):
        value = manifest.get(key)
        if not isinstance(value, str) or not value.strip() or len(value) > 256:
            raise ValueError('Explicit bounded board_id, boot_id and stream_id required')
    if not isinstance(manifest.get('config'), dict):
        raise ValueError('Recorded config snapshot required (empty object means unavailable)')
    mapping = manifest.get('sensor_types', {})
    if not isinstance(mapping, dict) or any(not isinstance(k, str) or not isinstance(v, str)
            or not k or not v or len(k)>256 or len(v)>256 for k,v in mapping.items()):
        raise ValueError('sensor_types must map explicit sensor labels to GS types')
    if manifest.get('launch_session') is not None and not isinstance(manifest['launch_session'], str):
        raise ValueError('launch_session must be a string or null')
    encoded = canonical(manifest)
    if len(encoded.encode()) > 1024*1024:
        raise ValueError('Manifest exceeds 1 MiB')
    return mapping, encoded


def identity_key(manifest, fields):
    return canonical([manifest['board_id'], manifest['boot_id'], manifest['stream_id'],
                      fields['sensor'], fields['uptime_ticks'], fields['record_sequence']])


def _uint(text, label, maximum=2**63-1):
    if not text or not text.isascii() or not text.isdigit():
        raise ValueError(label + ' must be an unsigned integer')
    value = int(text)
    if value > maximum:
        raise ValueError(label + ' out of range')
    return value


def import_csv(db, source, manifest, *, max_bytes=MAX_FILE_BYTES, max_rows=MAX_ROWS, max_database_bytes=MAX_DATABASE_BYTES):
    """Atomic import to derived recording. db must not have an open transaction.

    Rejects mixed boots (regressing uptime) rather than silently joining them.
    uint64 ticks/sequence are TEXT in SQLite to preserve their full unsigned range.
    CSV values remain strings to preserve raw codes, vector nan, and exact formatting.
    """
    mapping, encoded_manifest = _manifest(manifest)
    if db.in_transaction:
        raise ValueError('Import requires its own transaction')
    source_fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    import stat
    source_info=os.fstat(source_fd)
    if not stat.S_ISREG(source_info.st_mode) or source_info.st_size>max_bytes:
        os.close(source_fd);raise ValueError('Expected a regular CSV within configured disk/file limit')
    binary=os.fdopen(source_fd,'rb')
    file_hash=hashlib.sha256();hashed_bytes=0
    try:
        for block in iter(lambda:binary.read(65536),b''):
            hashed_bytes+=len(block)
            if hashed_bytes>max_bytes:raise ValueError('CSV exceeds configured byte limit')
            file_hash.update(block)
        binary.seek(0)
    except Exception:
        binary.close();raise
    digest=file_hash.hexdigest()
    # SQL schema changes have no effect on the original recording and never touch
    # telemetry; execute before BEGIN because executescript commits implicitly.
    db.execute('PRAGMA foreign_keys=ON')
    db.executescript(SCHEMA)
    csv.field_size_limit(MAX_FIELD_BYTES)
    text=io.TextIOWrapper(binary,encoding='utf-8-sig',newline='')
    reader = csv.reader(bounded_csv_lines(text), strict=True)
    metadata = []
    try:
        if next(reader, None) != FIELDS:
            raise ValueError('Unsupported SD CSV schema: exact current 13-column header required')
        db.execute('BEGIN IMMEDIATE')
        prior = db.execute('SELECT id,manifest_json,metadata_json FROM sd_imports WHERE file_sha256=? '
                           'AND board_id=? AND boot_id=? AND stream_id=?',
                           (digest,manifest['board_id'],manifest['boot_id'],manifest['stream_id'])).fetchone()
        if prior:
            if prior[1] != encoded_manifest:
                raise ValueError('Reimport manifest conflicts with retained provenance')
            db.rollback()
            return {'import_id':prior[0], 'file_sha256':digest, 'already_imported':True,
                    'sd_format':sd_format_summary(format_block_from_metadata(json.loads(prior[2])),manifest)}
        cur = db.execute('INSERT INTO sd_imports(file_sha256,filename,board_id,boot_id,stream_id,'
            'launch_session,schema_version,config_json,metadata_json,manifest_json) VALUES(?,?,?,?,?,?,?,?,?,?)',
            (digest,Path(source).name,manifest['board_id'],manifest['boot_id'],manifest['stream_id'],
             manifest.get('launch_session'),'seds-sd-csv-13-v1',canonical(manifest['config']),'[]',encoded_manifest))
        import_id, count, previous_ticks = cur.lastrowid, 0, {}
        if not db.execute('SELECT 1 FROM sd_source_chunks WHERE file_sha256=? LIMIT 1',(digest,)).fetchone():
            raw_fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
            with os.fdopen(raw_fd,'rb') as raw:
                offset=0;raw_hash=hashlib.sha256()
                for block in iter(lambda:raw.read(65536),b''):
                    if offset+len(block)>max_bytes:raise ValueError('CSV grew beyond configured byte limit')
                    db.execute('INSERT INTO sd_source_chunks VALUES(?,?,?)',(digest,offset,block))
                    offset+=len(block);raw_hash.update(block)
                    if offset%(4*1024*1024)==0:check_disk_limit(db,max_database_bytes)
                if raw_hash.hexdigest()!=digest:raise ValueError('CSV changed while retaining original bytes')
        for index,row in enumerate(reader, 1):
            if row and row[0].startswith('#'):
                metadata.append({'record_index':index, 'fields':row})
                if len(metadata)>1024 or len(canonical(metadata).encode())>1024*1024:
                    raise ValueError('Too much SD metadata')
                continue
            if not row:
                raise ValueError('Empty CSV record')
            if len(row) != len(FIELDS):
                raise ValueError('Expected 13 columns')
            if any(len(cell.encode())>MAX_FIELD_BYTES for cell in row):
                raise ValueError('CSV field exceeds byte limit')
            fields = dict(zip(FIELDS,row))
            stamp = _uint(fields['timestamp_ms'],'timestamp_ms')
            mono = _uint(fields['monotonic_ms'],'monotonic_ms')
            ticks = _uint(fields['uptime_ticks'],'uptime_ticks',2**64-1)
            freq = _uint(fields['uptime_frequency_hz'],'uptime_frequency_hz',2**32-1)
            _uint(fields['record_sequence'],'record_sequence',2**64-1)
            if not freq or fields['time_source'] not in ('local','network'):
                raise ValueError('Invalid clock provenance')
            sensor = fields['sensor']
            if sensor in previous_ticks and ticks < previous_ticks[sensor]:
                raise ValueError('Regressing sensor uptime: separate boot/stream or corrupt ordering')
            if sensor not in previous_ticks and len(previous_ticks)>=128:raise ValueError('Too many source sensors in one SD file')
            previous_ticks[sensor] = ticks
            if not fields['sensor'] or len(fields['sensor'])>256:
                raise ValueError('Invalid sensor label')
            fields_json = canonical(fields)
            content = hashlib.sha256(fields_json.encode()).hexdigest()
            identity = identity_key(manifest,fields)
            data_type = mapping.get(fields['sensor'])
            existing = db.execute('SELECT id,data_type FROM sd_samples WHERE identity_key=? '
                                  'AND content_sha256=?',(identity,content)).fetchone()
            if existing:
                if existing[1] != data_type:
                    raise ValueError('Identical sample has conflicting type mapping')
                sample_id=existing[0]
            else:
                sample_id=db.execute('INSERT INTO sd_samples(identity_key,content_sha256,sensor,'
                    'data_type,timestamp_ms,monotonic_ms,time_source,uptime_ticks,uptime_frequency_hz,'
                    'record_sequence,fields_json) VALUES(?,?,?,?,?,?,?,?,?,?,?)',
                    (identity,content,fields['sensor'],data_type,stamp,mono,fields['time_source'],
                     fields['uptime_ticks'],freq,fields['record_sequence'],fields_json)).lastrowid
            db.execute('INSERT INTO sd_sample_origins VALUES(?,?,?)',(import_id,index,sample_id))
            count += 1
            if count%4096==0:check_disk_limit(db,max_database_bytes)
            if count>max_rows:
                raise ValueError('Import exceeds row limit; no rows committed')
        if not count:
            raise ValueError('CSV contains no samples')
        sd_format=sd_format_summary(format_block_from_metadata(metadata),manifest)
        check_disk_limit(db,max_database_bytes)
        after=os.fstat(binary.fileno())
        if (after.st_size,after.st_mtime_ns)!=(source_info.st_size,source_info.st_mtime_ns):
            raise ValueError('CSV changed while importing; retry a closed immutable file')
        db.execute('UPDATE sd_imports SET metadata_json=? WHERE id=?',(canonical(metadata),import_id))
        db.commit()
        return {'import_id':import_id,'file_sha256':digest,'samples':count,'already_imported':False,'sd_format':sd_format}
    except Exception as error:
        db.rollback()
        raise ValueError(f'SD import rejected near physical line {reader.line_num}: {error}') from error
    finally:
        text.close()


def iter_sd_rows(db, import_ids=None):
    """Bounded report iterator. Exact raw occurrences remain in origins/blob tables.

    `record_indices` previews at most64 indices; record_occurrences/first/last
    identify larger duplicate groups without allocating an unbounded JSON array.
    """
    selected=list(import_ids) if import_ids is not None else None
    if selected is not None and (len(selected)>1024 or any(type(n)is not int or n<=0 for n in selected)):
        raise ValueError('Invalid SD import selection')
    imports='SELECT id,board_id,boot_id,stream_id,launch_session,config_json,metadata_json,file_sha256,filename FROM sd_imports'
    args=[]
    if selected is not None:
        imports+=' WHERE id IN ('+','.join('?' for _ in selected)+')';args=selected
    for imported in db.execute(imports+' ORDER BY id',args):
        import_id,board,boot,stream,session,config,metadata,sha,filename=imported
        config=json.loads(config);metadata=json.loads(metadata)
        query="""SELECT s.id,s.sensor,s.data_type,s.time_source,s.timestamp_ms,s.fields_json,s.identity_key,
          (SELECT COUNT(*)>1 FROM sd_samples c WHERE c.identity_key=s.identity_key),
          o.first_index,o.last_index,o.occurrences FROM
          (SELECT sample_id,MIN(record_index) first_index,MAX(record_index) last_index,COUNT(*) occurrences
           FROM sd_sample_origins WHERE import_id=? GROUP BY sample_id) o
          JOIN sd_samples s ON s.id=o.sample_id ORDER BY o.first_index"""
        for row in db.execute(query,(import_id,)):
            sample_id,sensor,kind,clock,stamp,fields,identity,ambiguous,first,last,count=row
            indices=[r[0] for r in db.execute('SELECT record_index FROM sd_sample_origins WHERE import_id=? AND sample_id=? ORDER BY record_index LIMIT 64',(import_id,sample_id))]
            yield {'sample_id':sample_id,'import_id':import_id,'board_id':board,'boot_id':boot,
                'stream_id':stream,'launch_session':session,'sensor':sensor,'data_type':kind,
                'utc_timestamp_ms':stamp if clock=='network' else None,'fields':json.loads(fields),
                'config':config,'calibration_metadata':metadata,'identity_key':identity,
                'ambiguous':bool(ambiguous),'file_sha256':sha,'filename':filename,
                'record_indices':indices,'record_occurrences':count,'record_index_first':first,
                'record_index_last':last,'record_indices_preview':count>64}


def main():
    import argparse
    parser=argparse.ArgumentParser(description='Import firmware SD CSV into a NEW derived recording; preserve original network DB')
    parser.add_argument('--recording',required=True)
    parser.add_argument('--csv',required=True)
    parser.add_argument('--manifest',required=True)
    parser.add_argument('--output',required=True)
    args=parser.parse_args()
    manifest=json.loads(_file(args.manifest,1024*1024).decode('utf-8'))
    # Validate manifest and input before creating the derived output.
    _manifest(manifest)
    create_derived_recording(args.recording,args.output)
    with closing(sqlite3.connect(args.output)) as db:
        result=import_csv(db,args.csv,manifest)
    print(canonical({'output':args.output,**result}))


if __name__=='__main__':
    main()
