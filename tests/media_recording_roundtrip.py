"""Current synthetic loopback camera archive acceptance. No hardware or voice mix."""
from pathlib import Path
import array, hashlib, json, math, os, signal, socket, subprocess, time, urllib.error, urllib.request

import argparse
parser = argparse.ArgumentParser(description=__doc__)
backend_input = parser.add_mutually_exclusive_group(required=True)
backend_input.add_argument('--backend-build-json', type=Path)
backend_input.add_argument('--backend-proof', type=Path)
helper_input = parser.add_mutually_exclusive_group(required=True)
helper_input.add_argument('--helper-build-json', type=Path)
helper_input.add_argument('--helper-proof', type=Path)
parser.add_argument('--output', type=Path, required=True, help='fresh absolute owned evidence directory; retained after testing')
parser.add_argument('--check-only', action='store_true', help='validate trusted test inputs without starting services')
options = parser.parse_args()
REPO = Path(__file__).resolve().parents[1]
OUT = options.output
assert OUT.is_absolute() and OUT.parent.is_dir() and not OUT.exists(), 'fresh absolute output required'
ROOT = OUT.parent

def source_snapshot(target):
    if target == 'groundstation_backend':
        folders = ['backend/src', 'backend/config', 'backend/layout', 'shared']
        names = ['Cargo.toml', 'Cargo.lock', 'backend/Cargo.toml', 'backend/build.rs', '.cargo/config.toml']
    else:
        folders = ['video_transcoder/src']
        names = ['video_transcoder/Cargo.toml', 'video_transcoder/Cargo.lock']
    paths = {REPO / p for p in names}
    for folder in folders:
        paths.update(p for p in (REPO / folder).rglob('*') if p.is_file() and (p.suffix in ('.rs', '.json') or p.name == 'Cargo.toml'))
    return {str(p.relative_to(REPO)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(paths)}

def test_artifact(proof_path, build_json, target):
    snapshot = source_snapshot(target)
    if proof_path is not None:
        proof = json.loads(proof_path.read_text())
        assert proof['source_snapshot'] == snapshot, 'proof must bind ALL current source inputs'
        assert proof['test_target'] == target, 'explicit TEST artifact required'
    else:
        artifacts = [json.loads(line) for line in build_json.read_text().splitlines() if line.startswith('{')]
        matches = [a for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('executable')
                   and a['profile']['test'] and a['target']['name'] == target]
        assert len(matches) == 1, 'one Cargo TEST executable required; never substitute a production binary'
        item = matches[0]
        expected_manifest = REPO / ('backend/Cargo.toml' if target == 'groundstation_backend' else 'video_transcoder/Cargo.toml')
        assert Path(item['manifest_path']).resolve() == expected_manifest.resolve(), 'build must come from this source checkout'
        executable = Path(item['executable']).resolve()
        proof = {'path': str(executable), 'sha256': hashlib.sha256(executable.read_bytes()).hexdigest(),
                 'test_target': target, 'source_snapshot': snapshot}
    executable = Path(proof['path'])
    assert executable.is_absolute() and executable.is_file()
    assert hashlib.sha256(executable.read_bytes()).hexdigest() == proof['sha256'], 'artifact hash mismatch'
    return proof

backend = test_artifact(options.backend_proof, options.backend_build_json, 'groundstation_backend')
backend['entrypoint'] = 'media::tests::serve_synthetic_media'
worker = test_artifact(options.helper_proof, options.helper_build_json, 'gs-video-transcoder')
if options.check_only:
    print('PASS trusted test executable hashes/input structure; current snapshot recorded, not historical compilation proof; no services started')
    raise SystemExit(0)
OUT.mkdir(exist_ok=False)
RECORDINGS = OUT / 'recordings'
RECORDINGS.mkdir()

def port(udp=False):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM) as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

rtsp, api, hls, webrtc, http = [port() for _ in range(5)]
udp = port(True)
container = 'seds-private-camera-recording-' + str(os.getpid())
config = f'''logLevel: warn
api: true
apiAddress: 127.0.0.1:{api}
metrics: false
pprof: false
playback: false
rtsp: true
rtspAddress: 127.0.0.1:{rtsp}
rtspTransports: [tcp]
rtmp: false
hls: true
hlsAddress: 127.0.0.1:{hls}
hlsAlwaysRemux: true
hlsVariant: fmp4
hlsSegmentDuration: 1s
hlsSegmentCount: 90
hlsSegmentMaxSize: 8M
webrtc: true
webrtcAddress: 127.0.0.1:{webrtc}
webrtcLocalUDPAddress: 127.0.0.1:{udp}
webrtcLocalTCPAddress: ''
webrtcAdditionalHosts: [127.0.0.1]
srt: false
moq: false
authInternalUsers:
  - user: camera
    pass: fixture-only-media
    permissions: [{{action: publish}}]
  - user: groundstation
    pass: fixture-only-media
    permissions: [{{action: read}}, {{action: api}}]
pathDefaults:
  source: publisher
  overridePublisher: false
  maxReaders: 16
  record: true
  recordPath: /evidence/recordings/%path/%Y-%m-%d_%H-%M-%S-%f
  recordFormat: fmp4
  recordPartDuration: 1s
  recordSegmentDuration: 1m
  recordDeleteAfter: 0s
paths:
  "~^[A-Za-z0-9_-]{{1,64}}$": {{}}
'''
(OUT / 'mediamtx.yml').write_text(config)
env = {k: v for k, v in os.environ.items() if not k.startswith('GS_')}
env.update(GS_MEDIA_TEST_DIR=str(OUT), GS_MEDIA_TEST_BIND=f'127.0.0.1:{http}',
           GS_MEDIA_TEST_ORIGIN=f'http://127.0.0.1:{http}', GS_STAGE_MODELS_DIR=str(OUT / 'models'),
           GS_VIDEO_RECORDINGS_DIR=str(RECORDINGS), GS_VIDEO_PASSWORD='fixture-only-media',
           GS_VIDEO_API_URL=f'http://127.0.0.1:{api}', GS_VIDEO_HLS_URL=f'http://127.0.0.1:{hls}',
           GS_VIDEO_WEBRTC_URL=f'http://127.0.0.1:{webrtc}', GS_VIDEO_RTSP_URL=f'rtsp://127.0.0.1:{rtsp}')
for key, name in [('GS_LOADCELL_CALIBRATION_PATH', 'loadcell.json'), ('GS_CHECKLISTS_FILE', 'checklists.json'),
                  ('GS_AUTO_ZERO_CONFIG', 'auto_zero.json'), ('GS_GSE_QUALIFICATION', 'qualification.json'),
                  ('GS_ZERO_CAPTURE_DIR', 'zero-captures'), ('GS_NETWORK_VARIABLE_CACHE', 'variables.json')]:
    env[key] = str(OUT / name)
for key, name in [('GS_FLIGHT_SETUP_CONFIG', 'flight_setup.json'), ('GS_FILL_TARGETS_CONFIG', 'fill_targets.json'),
                  ('GS_GSE_CONFIG', 'gse_sequence.json'), ('GS_CALIBRATION_CONFIG_PATH', 'calibration_config.json'),
                  ('GS_STATION_POSITIONS_PATH', 'station_positions.json')]:
    source = REPO / 'backend/config' / name
    if source.exists():
        (OUT / name).write_bytes(source.read_bytes())
    env[key] = str(OUT / name)
children, logs = [], []
started = time.monotonic()
result = {'status': 'running', 'scope': 'actual original/processed camera MP4, not voice/composite recording',
          'backend': backend, 'worker': worker, 'record_directory': str(RECORDINGS), 'checks': {}}

def resources():
    assert time.monotonic() - started < 900, 'whole fixture 900s bound'
    assert os.statvfs(ROOT).f_bavail * os.statvfs(ROOT).f_frsize > 6 * 1024**3, 'free floor'
    files = [p for p in OUT.rglob('*') if p.is_file()]
    assert sum(p.stat().st_size for p in files) < 512 * 1024**2, 'evidence512MiB cap'
    assert all(p.stat().st_size < 64 * 1024**2 for p in files if p.suffix == '.log'), 'log64MiB cap'

def start(name, command, extra=None):
    log = (OUT / (name + '.log')).open('xb'); logs.append(log)
    child = subprocess.Popen(command, cwd=REPO, env=env | (extra or {}), stdout=log,
                             stderr=subprocess.STDOUT, start_new_session=True)
    children.append(child)
    return child

def wait_port(value):
    end = time.monotonic() + 30
    while time.monotonic() < end:
        try:
            with socket.create_connection(('127.0.0.1', value), timeout=.2):
                return
        except OSError:
            time.sleep(.1)
    raise AssertionError('fixture listener not ready')

def request(path, user='producer', headers=None, limit=1024**2):
    assert path.startswith('/') and not path.startswith('//')
    values = {} if user is None else {'Authorization': 'Bearer synthetic-' + user}
    values.update(headers or {})
    req = urllib.request.Request(env['GS_MEDIA_TEST_ORIGIN'] + path, headers=values)
    try:
        response = urllib.request.urlopen(req, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        body = response.read(limit + 1)
        assert len(body) <= limit
        return response.status, {key.lower(): value for key, value in response.headers.items()}, body

def catalog(user='producer'):
    status, _, data = request('/api/video/recordings?limit=100', user)
    assert status == 200, (status, data[:300])
    return json.loads(data)['recordings']

def inspect_receiver(script):
    return subprocess.check_output(['docker', 'run', '--rm', '--network', 'none', '--pid', 'container:' + container,
        '--cap-drop', 'ALL', '--cap-add', 'SYS_PTRACE', '--read-only', '--entrypoint', '/bin/sh',
        'debian:bookworm-slim', '-c', script], text=True, stderr=subprocess.STDOUT, timeout=15)

def recording_fds():
    script = 'for f in /proc/1/fd/*; do p="$(readlink "$f")"; case "$p" in /evidence/recordings/*) printf "%s\\n" "$p";; esac; done'
    return set(inspect_receiver(script).splitlines())

def container_file(item):
    assert item['stream'] in ('front', 'side', 'transcoded-front', 'transcoded-side')
    assert '/' not in item['file'] and item['file'].endswith('.mp4')
    return '/evidence/recordings/' + item['stream'] + '/' + item['file']

def actual_active(stream):
    end = time.monotonic() + 25
    while time.monotonic() < end:
        resources()
        for item in catalog():
            if item['stream'] != stream or not item['recently_updated']:
                continue
            local = RECORDINGS / stream / item['file']; before = local.stat()
            opened = recording_fds()
            if container_file(item) not in opened:
                continue
            time.sleep(2)
            after = local.stat()
            if after.st_size <= before.st_size or after.st_mtime_ns <= before.st_mtime_ns:
                continue
            status, _, _ = request(item['url'], None)
            assert status == 409, 'REAL currently open/growing archive must refuse download'
            return {'stream': stream, 'file': item['file'], 'before_bytes': before.st_size,
                    'after_bytes': after.st_size, 'mtime_advanced_naturally': True,
                    'actual_receiver_FD_open': True, 'HTTP_status': status}
        time.sleep(.2)
    raise AssertionError('no actual open/growing recording for ' + stream)

def finalized_pair():
    end = time.monotonic() + 90
    while time.monotonic() < end:
        resources(); rows = catalog(); opened = recording_fds(); chosen = {}
        for stream in ('front', 'transcoded-front'):
            candidates = sorted([i for i in rows if i['stream'] == stream], key=lambda i: i['file'])
            for item in candidates:
                own = container_file(item)
                # Natural rollover is established by OLD fd CLOSED and NEW same-stream fd OPEN.
                next_open = sorted(p for p in opened if p.startswith('/evidence/recordings/' + stream + '/') and p != own)
                if not item['recently_updated'] and item['size_bytes'] > 32768 and own not in opened and next_open:
                    chosen[stream] = {'item': item, 'old_fd_closed': True, 'new_same_stream_fd_open': next_open}
                    break
        if len(chosen) == 2:
            return chosen
        time.sleep(1)
    raise AssertionError('missing natural1min rollover/finalized original+processed pair')

def download_and_decode(proof):
    item = proof['item']; assert item['size_bytes'] < 64 * 1024**2
    status, headers, prefix = request(item['url'], None, {'Range': 'bytes=0-4095'}, 4096)
    assert status == 206 and len(prefix) == 4096
    assert headers.get('content-range', '').startswith('bytes 0-4095/')
    assert request(item['url'].split('?')[0], None)[0] == 403, 'no capability/no manager'
    destination = OUT / ('downloaded-' + item['stream'] + '.mp4')
    req = urllib.request.Request(env['GS_MEDIA_TEST_ORIGIN'] + item['url'])
    count = 0
    with urllib.request.urlopen(req, timeout=15) as response, destination.open('xb') as output:
        assert response.status == 200
        while chunk := response.read(65536):
            count += len(chunk); assert count <= item['size_bytes']
            output.write(chunk); resources()
    assert count == item['size_bytes']
    local = RECORDINGS / item['stream'] / item['file']
    digest = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
    assert digest(local) == digest(destination), 'download must equal actual closed receiver file'
    probe = subprocess.run(['ffprobe', '-v', 'error', '-show_streams', '-show_format', '-of', 'json', str(destination)],
                           capture_output=True, check=True, timeout=20)
    metadata = json.loads(probe.stdout); streams = metadata['streams']
    video = next(s for s in streams if s['codec_type'] == 'video')
    audio = next(s for s in streams if s['codec_type'] == 'audio')
    assert video['codec_name'] == 'h264' and audio['codec_name'] == 'aac'
    pcm = subprocess.run(['ffmpeg', '-v', 'error', '-i', str(destination), '-t', '2', '-map', '0:a:0',
                          '-ac', '1', '-ar', '16000', '-f', 's16le', 'pipe:1'], capture_output=True, check=True, timeout=20)
    assert 32000 <= len(pcm.stdout) <= 128000
    samples = array.array('h', pcm.stdout)
    import sys
    if sys.byteorder != 'little': samples.byteswap()
    rms = math.sqrt(sum((v / 32768)**2 for v in samples) / len(samples))
    assert rms > .001, 'actual recorded camera AAC must decode to non-silent PCM'
    decode = subprocess.run(['ffmpeg', '-v', 'error', '-i', str(destination), '-map', '0:v:0', '-an',
                             '-f', 'null', '-'], capture_output=True, timeout=45)
    assert decode.returncode == 0 and len(decode.stderr) < 65536, 'FULL recorded video decode'
    return {'stream': item['stream'], 'file': item['file'], 'bytes': count, 'sha256': digest(destination),
            'range_status': status, 'range_bytes': len(prefix), 'no_auth_status': 403,
            'video': {k: video.get(k) for k in ('codec_name', 'profile', 'width', 'height', 'r_frame_rate')},
            'audio': {k: audio.get(k) for k in ('codec_name', 'sample_rate', 'channels')},
            'duration_seconds': metadata['format'].get('duration'), 'decoded_camera_audio_RMS': rms,
            'full_video_decode_exit': decode.returncode, 'natural_rollover': {k: v for k, v in proof.items() if k != 'item'}}

original_users = None
try:
    image = subprocess.check_output(['docker', 'image', 'inspect', 'bluenviron/mediamtx:1.21.0', '--format', '{{.Id}}'], text=True).strip()
    assert image == 'sha256:19fddade8d6110a3d718ac0045681fbeba344ae563a066205fe5929a87f7582f'
    subprocess.run(['docker', 'run', '--detach', '--name', container, '--network', 'host',
                    '--label', 'seds-test=private-camera-recording', '-v', str(OUT / 'mediamtx.yml') + ':/mediamtx.yml:ro',
                    '-v', str(RECORDINGS) + ':/evidence/recordings:rw', 'bluenviron/mediamtx:1.21.0'],
                   check=True, stdout=subprocess.DEVNULL, timeout=20)
    wait_port(rtsp)
    raw = inspect_receiver('sockets="$(for f in /proc/1/fd/*; do readlink "$f"; done)"; awk -v owned="$sockets" \'FNR>1 && index(owned,"socket:[" $10 "]")>0 {print FILENAME,$2,$4,$10}\' /proc/1/net/tcp /proc/1/net/tcp6 /proc/1/net/udp /proc/1/net/udp6')
    listeners = [line.split() for line in raw.splitlines() if line.startswith('/proc/1/net/')]
    actual = set()
    for protocol, address, state, inode in listeners:
        assert protocol in ('/proc/1/net/tcp', '/proc/1/net/udp') and address.split(':')[0] == '0100007F'
        actual.add((protocol.rsplit('/', 1)[-1], int(address.split(':')[1], 16)))
    assert len(listeners) == 5 and actual == {('tcp', rtsp), ('tcp', api), ('tcp', hls), ('tcp', webrtc), ('udp', udp)}
    result['receiver_listener_identity'] = listeners
    start('backend', [backend['path'], '--exact', backend['entrypoint'], '--ignored', '--nocapture'])
    wait_port(http)
    for user in ('viewer', 'publisher', 'operator'):
        assert request('/api/video/recordings', user)[0] == 403
    for name, pattern, gop in [('front', 'testsrc2', 60), ('side', 'smptebars', 45)]:
        start(name, ['ffmpeg', '-hide_banner', '-loglevel', 'error', '-re', '-f', 'lavfi', '-i', pattern + '=size=640x360:rate=15',
                     '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000', '-c:v', 'libx264', '-preset', 'ultrafast',
                     '-tune', 'zerolatency', '-profile:v', 'baseline', '-pix_fmt', 'yuv420p', '-g', str(gop), '-bf', '0',
                     '-c:a', 'aac', '-b:a', '96k', '-f', 'rtsp', '-rtsp_transport', 'tcp',
                     f'rtsp://camera:fixture-only-media@127.0.0.1:{rtsp}/{name}'])
    result['checks']['active_original'] = actual_active('front')
    encoder = start('native-helper', [worker['path'], '--exact', 'engine::tests::native_worker_roundtrip',
                    '--ignored', '--nocapture', '--test-threads=1'], {'GS_TRANSCODE_TEST_HOLD_SECONDS': '100'})
    result['checks']['active_processed'] = actual_active('transcoded-front')
    pair = finalized_pair()
    assert encoder.poll() is None, 'actual encoder must remain live during natural rollover'
    result['checks']['finalized'] = [download_and_decode(proof) for proof in pair.values()]
    # A previously minted narrow media ticket must NOT outlive current account grants.
    original_users = (OUT / 'users.json').read_bytes(); users = json.loads(original_users)
    next(u for u in users['users'] if u['username'] == 'producer')['roles'] = []
    temp = OUT / 'users-revoked.next'; temp.write_text(json.dumps(users)); os.replace(temp, OUT / 'users.json')
    assert request('/api/video/recordings')[0] == 403
    for proof in pair.values():
        assert request(proof['item']['url'], None)[0] == 403
    result['checks']['old_ticket_current_revocation_denied'] = True
    restore = OUT / 'users-restored.next'; restore.write_bytes(original_users); os.replace(restore, OUT / 'users.json'); original_users = None
    # Permission revocation may already have intentionally stopped current jobs; don't label interrupted worker a successful100s run.
    result['native_helper_terminal_before_cleanup'] = encoder.poll()
    resources(); result['status'] = 'PASS'
    print('PASS actual natural rollover raw+processed MP4/H264/AAC/fullvideo+PCM/range/writing409/currentrevocation', flush=True)
except Exception as error:
    result.update(status='FAIL', error=str(error)); raise
finally:
    if original_users is not None:
        restore = OUT / 'users-restored.next'; restore.write_bytes(original_users); os.replace(restore, OUT / 'users.json')
    for child in reversed(children):
        if child.poll() is None:
            try: os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError: pass
            try: child.wait(timeout=5)
            except subprocess.TimeoutExpired: os.killpg(child.pid, signal.SIGKILL); child.wait(timeout=5)
    with (OUT / 'receiver.log').open('xb') as log:
        subprocess.run(['docker', 'logs', container], stdout=log, stderr=subprocess.STDOUT, timeout=10)
    subprocess.run(['docker', 'rm', '--force', container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
    for log in logs: log.close()
    result['seconds'] = time.monotonic() - started
    result['teardown_all_direct_children_exited'] = all(c.poll() is not None for c in children)
    (OUT / 'RESULT.json').write_text(json.dumps(result, indent=2) + '\n')
