"""Synthetic loopback/PTY service integration; never opens physical devices."""
import argparse, base64, hashlib, hmac, json, os, pty, selectors, signal, socket, subprocess, tempfile, threading, time, tty, urllib.request
from pathlib import Path
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, required=True, help='Native backend executable built without hitl_mode')
parser.add_argument('--artifacts-dir', type=Path, help='New directory for disposable state and progress; defaults to a temporary directory')
args = parser.parse_args()
SOURCE = Path(__file__).resolve().parents[1]
BINARY = args.binary.resolve()
if not BINARY.is_file():
    raise SystemExit('Build the native backend first: cargo build --locked -p groundstation_backend')
OUT = args.artifacts_dir.resolve() if args.artifacts_dir else Path(tempfile.mkdtemp(prefix='gs-paired-mission-'))
if args.artifacts_dir:
    OUT.mkdir()
os.chmod(OUT, 0o700)
print('Acceptance artifacts:', OUT, flush=True)
key = hashlib.pbkdf2_hmac('sha256', b'fixture-only-password', b'isolated-gse-model', 120000)
users = {'version': 1, 'anonymous': {'view_data': False, 'send_commands': False}, 'users': [{'username': 'sequence-fixture', 'roles': ['admin'], 'password': {'algorithm': 'pbkdf2_sha256', 'iterations': 120000, 'salt_b64': base64.b64encode(b'isolated-gse-model').decode(), 'hash_b64': base64.b64encode(key).decode()}, 'permissions': {'view_data': True, 'send_commands': True}, 'disabled': False}]}
reserved = []

def port():
    s = socket.socket()
    s.bind(('127.0.0.1', 0))
    reserved.append(s)
    return s.getsockname()[1]
ports = {name: port() for name in ['primary', 'rrls']}
fills = {name: port() for name in ports}
pairs = {}
processes = {}
tokens = {}
stopped = threading.Event()
connected = threading.Event()
connected.set()
stats = {'bytes': {}, 'partition_drops': 0}
progress = {'phase': 'preparing', 'ports': ports, 'stages': []}

def save():
    (OUT / 'progress.json').write_text(json.dumps(progress, indent=2) + '\n')

def request(name, path, body=None):
    headers = {'Content-Type': 'application/json'}
    if name in tokens:
        headers['Authorization'] = 'Bearer ' + tokens[name]
    req = urllib.request.Request(f'http://127.0.0.1:{ports[name]}' + path, None if body is None else json.dumps(body, separators=(',', ':')).encode(), headers)
    with urllib.request.urlopen(req, timeout=3) as response:
        data = response.read(1024 * 1024 + 1)
        assert len(data) <= 1024 * 1024
        return json.loads(data) if data else None

def login(name):
    c = request(name, '/api/auth/challenge', {'username': 'sequence-fixture'})
    m = {'username': 'sequence-fixture', 'challenge_id': c['challenge_id'], 'server_nonce_b64': c['server_nonce_b64'], 'client_nonce_b64': base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip('='), 'remember_me': False}
    m['proof_b64'] = base64.urlsafe_b64encode(hmac.new(key, json.dumps(m, separators=(',', ':')).encode(), hashlib.sha256).digest()).decode().rstrip('=')
    tokens[name] = request(name, '/api/auth/login', m)['token']

def wait(label, predicate, seconds=300):
    deadline = time.monotonic() + seconds
    error = None
    reported = 0
    while time.monotonic() < deadline:
        for n, p in processes.items():
            assert p.poll() is None, (n, p.returncode)
        try:
            value = predicate()
            if value:
                progress['stages'].append({'name': label, 'passed': True, 'elapsed_s': round(seconds - (deadline - time.monotonic()), 3)})
                save()
                return value
        except (urllib.error.URLError, ConnectionError, TimeoutError, KeyError) as e:
            error = str(e)
        if time.monotonic() - reported >= 10:
            reported = time.monotonic()
            snapshot = {'stage': label, 'elapsed_s': round(seconds - (deadline - time.monotonic()), 1), 'bytes': dict(stats['bytes']), 'stations': {}}
            for n in processes:
                path = OUT / n / 'mission.json'
                if path.exists():
                    snapshot['stations'][n] = list(json.loads(path.read_text())['store']['documents'])
            with (OUT / 'observations.jsonl').open('a') as f:
                f.write(json.dumps(snapshot) + '\n')
            print(json.dumps(snapshot), flush=True)
        time.sleep(0.2)
    raise RuntimeError(f'{label} timed out; last={error}')
for name in ports:
    directory = OUT / name
    directory.mkdir()
    os.chmod(directory, 0o700)
    master, slave = pty.openpty()
    tty.setraw(slave)
    os.set_blocking(master, False)
    pairs[name] = (master, slave)
    (directory / 'users.json').write_text(json.dumps(users))
    (directory / 'signing.seed').write_bytes(bytes([113]) * 32)
    assert (directory / 'signing.seed').stat().st_size == 32
    peer = 'rrls' if name == 'primary' else 'primary'
    (directory / 'pairing.json').write_text(json.dumps({'version': 1, 'installation_id': 'paired-fixture-v1', 'station_id': name, 'signing_seed_file': 'signing.seed', 'peers': [{'id': peer, 'role': peer, 'urls': [f'http://127.0.0.1:{ports[peer]}']}], 'scheme': 'http', 'port': ports[name]}))
    (directory / 'comms.json').write_text(json.dumps({'version': 1, 'av_bay': {'interface': 'serial', 'port': os.ttyname(slave), 'baud_rate': 115200, 'protocol': 'raw_uart'}, 'fill_box': {'interface': 'pico_fi_tcp', 'mode': 'server', 'address': f'127.0.0.1:{fills[name]}', 'allowed_peer': '127.0.0.1', 'discovery': False}}))
    (directory / 'fill.json').write_text(json.dumps({'version': 1, 'fill_source': 'kg50', 'nitrogen': {'target_mass_kg': 11.0 if name == 'primary' else 99.0, 'target_pressure_psi': 120.0}, 'nitrous': {'target_mass_kg': 10.0, 'target_pressure_psi': 745.0}}))
    (directory / 'auto-zero.json').write_text(json.dumps({'enabled': False}))
    (directory / 'checklists.json').write_text(json.dumps({'revision': 1, 'enforce_before_launch': True, 'checklists': [{'id': 'setup', 'title': 'Setup', 'required_before_launch': True, 'items': [{'id': 'manual', 'text': 'Inspect fixture sensor', 'checked': False}]}]}))

def bridge():
    selector = selectors.DefaultSelector()
    for name, (fd, _) in pairs.items():
        selector.register(fd, selectors.EVENT_READ, name)
    while not stopped.is_set():
        for event, _ in selector.select(0.1):
            try:
                data = os.read(event.fd, 65536)
            except BlockingIOError:
                continue
            if not data:
                continue
            stats['bytes'][event.data] = stats['bytes'].get(event.data, 0) + len(data)
            if not connected.is_set():
                stats['partition_drops'] += len(data)
                continue
            other = 'rrls' if event.data == 'primary' else 'primary'
            written = 0
            while written < len(data) and (not stopped.is_set()):
                try:
                    written += os.write(pairs[other][0], data[written:])
                except BlockingIOError:
                    time.sleep(0.001)
    selector.close()
thread = threading.Thread(target=bridge, daemon=True)
thread.start()

def start(name):
    directory = OUT / name
    env = os.environ.copy()
    env.update(
        GS_SIM_MODE='0',
        GS_ETHERNET_AUTOCONFIG='off',
        GS_LOG_DIR=str(directory / 'logs'),
        GS_SIMULATED_SERIAL_PTY='1',
        GS_RRLS_ROLE=name,
        GS_COMMS_LINK_CONFIG=str(directory / 'comms.json'),
        GS_BIND_ADDRESS=f'127.0.0.1:{ports[name]}',
        GS_HTTPS_BIND_ADDRESS='off',
        GS_VIDEO_MANAGED='0',
        GS_VIDEO_DISCOVERY='0',
        GS_STATION_DISCOVERY_BIND='127.0.0.1:0',
        GS_USERS_FILE=str(directory / 'users.json'),
        GS_LAYOUT_PATH=str(SOURCE / 'backend/layout/layout_test_fire.json'),
        GS_FLIGHT_STATE_DEFAULT='0',
        GS_STATION_PAIRING_FILE=str(directory / 'pairing.json'),
        GS_MISSION_SETTINGS_PATH=str(directory / 'mission.json'),
        GS_CHECKLISTS_FILE=str(directory / 'checklists.json'),
        GS_FILL_TARGETS_CONFIG=str(directory / 'fill.json'),
        GS_GSE_CONFIG=str(directory / 'gse.json'),
        GS_GSE_QUALIFICATION=str(directory / 'qualification.json'),
        GS_AUTO_ZERO_CONFIG=str(directory / 'auto-zero.json'),
        GS_FLIGHT_SETUP_CONFIG=str(directory / 'flight.json'),
        GS_LOADCELL_CALIBRATION_PATH=str(directory / 'calibration.json'),
        GS_CALIBRATION_CONFIG_PATH=str(directory / 'calibration-config.json'),
        GS_STATION_POSITIONS_PATH=str(directory / 'positions.json'),
        GS_NETWORK_VARIABLE_CACHE=str(directory / 'network-variables.json'),
        GS_STATION_MODE_FILE=str(directory / 'mode.json'),
        GS_DEBUG_PRINTS='0',
        RUST_LOG='warn',
    )
    log = (directory / f'backend-{time.time_ns()}.log').open('w')
    processes[name] = subprocess.Popen([str(BINARY)], cwd=directory, env=env, stdout=log, stderr=subprocess.STDOUT)
    log.close()
    progress.setdefault('processes', []).append({'station': name, 'pid': processes[name].pid})
    save()

try:
    for s in reserved:
        s.close()
    for name in ports:
        start(name)
    progress['phase'] = 'running'
    save()
    for name in ports:
        wait(name + ' API ready', lambda n=name: request(n, '/api/auth/challenge', {'username': 'sequence-fixture'}), 20)
        login(name)
    wait('both paired settings enabled', lambda: all((request(n, '/api/mission-settings')['enabled'] for n in ports)), 20)
    wait('secondary receives primary saved fill target', lambda: request('rrls', '/api/fill_targets')['nitrogen']['target_mass_kg'] == 11.0)
    wait('both checklist journals ready', lambda: all((request(n, '/api/checklists/sync')['ready'] for n in ports)))

    def histories():
        return {n: json.loads((OUT / n / 'mission.json').read_text())['store']['documents'] for n in ports}

    def converged():
        d = histories()
        return len(d['primary']) == 9 and d['primary'] == d['rrls']
    wait('all nine saved documents converge', converged, 300)
    request('primary', '/api/checklists/check', {'checklist_id': 'setup', 'item_id': 'manual', 'checked': True})

    def checked(name):
        data = request(name, '/api/checklists')
        return data['checklists'][0]['items'][0]['checked']
    wait('primary checklist progress reaches RRLS', lambda: checked('rrls'), 180)
    assert not request('rrls', '/api/auto_zero')['enabled']
    connected.clear()
    body = request('primary', '/api/fill_targets')
    body['nitrogen']['target_mass_kg'] = 12.0
    request('primary', '/api/fill_targets', body)
    request('rrls', '/api/auto_zero', {'enabled': True})
    wait('both offline edits saved', lambda: histories()['primary']['mission/fill_targets']['heads'][0]['value']['nitrogen']['target_mass_kg'] == 12.0 and histories()['rrls']['mission/auto_zero_preference']['heads'][0]['value']['enabled'], 20)
    connected.set()
    wait('independent offline edits merge', lambda: converged() and request('rrls', '/api/fill_targets')['nitrogen']['target_mass_kg'] == 12.0 and request('primary', '/api/auto_zero')['enabled'], 300)
    connected.clear()
    for name, value in [('primary', 13.0), ('rrls', 14.0)]:
        body = request(name, '/api/fill_targets')
        body['nitrogen']['target_mass_kg'] = value
        request(name, '/api/fill_targets', body)
    wait('concurrent local edits saved', lambda: all((histories()[n]['mission/fill_targets']['heads'][0]['value']['nitrogen']['target_mass_kg'] == v for n, v in [('primary', 13.0), ('rrls', 14.0)])), 20)
    connected.set()
    wait('both retain same-setting conflict', lambda: all((len(request(n, '/api/mission-settings/conflicts').get('mission/fill_targets', {}).get('heads', [])) == 2 for n in ports)), 300)
    reviewed = request('primary', '/api/mission-settings/conflicts')['mission/fill_targets']
    progress['reviewed_conflict'] = reviewed
    save()
    for name in ports:
        processes[name].send_signal(signal.SIGINT)
        processes[name].wait(timeout=10)
        tokens.pop(name, None)
        start(name)
    for name in ports:
        wait(name + ' restarted API', lambda n=name: request(n, '/api/auth/challenge', {'username': 'sequence-fixture'}), 20)
        login(name)
    wait('conflict survives both service restarts', lambda: all((request(n, '/api/mission-settings/conflicts').get('mission/fill_targets') == reviewed for n in ports)), 30)
    request('primary', '/api/mission-settings/resolve', {'key': 'mission/fill_targets', 'expected': reviewed, 'value': reviewed['heads'][0]['value']})
    wait('reviewed resolution converges', lambda: converged() and all((request(n, '/api/mission-settings')['conflicts'] == 0 for n in ports)), 300)
    try:
        request('primary', '/api/mission-settings/resolve', {'key': 'mission/fill_targets', 'expected': reviewed, 'value': reviewed['heads'][1]['value']})
    except urllib.error.HTTPError as error:
        assert error.code == 409, error.code
    else:
        raise AssertionError('stale conflict resolution accepted')
    progress['stages'].append({'name': 'stale reviewed heads rejected', 'passed': True})
    save()
    progress['snapshots'] = {n: {'mission': request(n, '/api/mission-settings'), 'checklists': request(n, '/api/checklists/sync')} for n in ports}
    progress['phase'] = 'passed'
    save()
except BaseException as error:
    progress['phase'] = 'failed'
    progress['error'] = str(error)
    save()
    raise
finally:
    stopped.set()
    for p in processes.values():
        if p.poll() is None:
            p.send_signal(signal.SIGINT)
    for p in processes.values():
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
    thread.join(2)
    for fd, slave in pairs.values():
        os.close(fd)
        os.close(slave)
    for name in ports:
        (OUT / name / 'signing.seed').unlink(missing_ok=True)
        (OUT / name / 'users.json').unlink(missing_ok=True)
    progress['bridge'] = stats
    progress['stopped'] = all((p.poll() is not None for p in processes.values()))
    save()
