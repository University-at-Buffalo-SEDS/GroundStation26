"""Two real local backend processes; synthetic PTY radio and disposable settings only."""
from pathlib import Path
import os,pty,tty,select,socket,subprocess,json,time,tempfile,threading,urllib.request,urllib.error,signal,shutil,argparse
parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--binary',type=Path);parser.add_argument('--output-json',type=Path);args=parser.parse_args()
root=Path(__file__).resolve().parents[1];base=Path(tempfile.mkdtemp(prefix='gs-paired-runtime-acceptance-'));binary=(args.binary or root/'target/debug/groundstation_backend').resolve()
if not binary.is_file():raise SystemExit('Build the testing backend first: cargo build -p groundstation_backend --features testing')
state={'phase':'prepare','scope':'Two actual testing-feature backend processes on loopback; synthetic PTY radio only. GS_SIM_MODE=0. No hardware, real credentials, external services or deployment.','directory':str(base),'checks':[]}
progress=args.output_json or base/'progress.json'
print('Acceptance evidence:',progress,flush=True)
def save():progress.write_text(json.dumps(state,indent=2))
def phase(name):state['phase']=name;save()
def passed(name):state['checks'].append(name);save()
def write(p,value):p.write_text(json.dumps(value))
def request(station,path,body=None,expected=200):
 data=None if body is None else json.dumps(body).encode()
 req=urllib.request.Request(f'http://127.0.0.1:{station["port"]}'+path,data=data,headers={'Content-Type':'application/json'})
 try:
  with urllib.request.urlopen(req,timeout=3) as r:code=r.status;raw=r.read()
 except urllib.error.HTTPError as e:code=e.code;raw=e.read()
 assert code==expected,(station['id'],path,code,raw[:200])
 return json.loads(raw) if code==200 else raw
stopped=threading.Event();linked=threading.Event();linked.set();stations=[];handles=[];slaves=[]
def pump():
 while not stopped.is_set():
  ready,_,_=select.select([s['master'] for s in stations],[],[],.05)
  for fd in ready:
   try:data=os.read(fd,65536)
   except OSError:continue
   if data and linked.is_set():
    peer=next(s['master'] for s in stations if s['master']!=fd)
    try:os.write(peer,data)
    except OSError:pass

def start(s):
 f=(s['dir']/f'backend-{time.time_ns()}.log').open('w');handles.append(f)
 s['process']=subprocess.Popen([str(binary)],cwd=s['dir'],env=s['env'],stdout=f,stderr=subprocess.STDOUT)
def stop(s):
 p=s.get('process')
 if p is not None and p.poll() is None:
  p.send_signal(signal.SIGTERM)
  try:p.wait(timeout=10)
  except subprocess.TimeoutExpired:p.kill();p.wait()
def wait_for(predicate,seconds=65):
 deadline=time.monotonic()+seconds;last=None
 while time.monotonic()<deadline:
  for s in stations:
   p=s.get('process')
   if p is not None and p.poll() is not None:raise RuntimeError(f'{s["id"]} exited: {p.returncode}')
  try:
   if predicate():return
  except (OSError,AssertionError) as e:last=str(e)
  time.sleep(.25)
 raise TimeoutError(last or 'condition not met')
def calibrate(s,slope):
 cfg=request(s,'/api/calibration')
 cfg.setdefault('extra_channels',{})['ch1']={'linear':{'m':slope,'b':0},'points':[]}
 request(s,'/api/calibration',{'channel':'ch1','calibration':cfg})
def slope(s):return request(s,'/api/calibration')['extra_channels']['ch1']['linear']['m']
try:
 seed=os.urandom(32)
 for identity,role,initial in [('gs','primary',1),('rrls','rrls',None)]:
  d=base/identity;d.mkdir();(d/'data').mkdir();(d/'seed').write_bytes(seed);os.chmod(d/'seed',0o600)
  m,slave=pty.openpty();tty.setraw(slave);slaves.append(slave)
  with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
  peer='rrls' if identity=='gs' else 'gs'
  write(d/'pairing.json',{'version':1,'installation_id':'local-acceptance','station_id':identity,'signing_seed_file':'seed','peers':[{'id':peer,'role':'rrls' if peer=='rrls' else 'primary'}],'scheme':'http','port':port})
  write(d/'users.json',{'version':1,'anonymous':{'view_data':True},'anonymous_calibration_access':{'view':True,'edit':True},'users':[]})
  cfg={'full_mass_kg':17 if initial else None}
  if initial:cfg['extra_channels']={'ch1':{'linear':{'m':initial,'b':0},'points':[]}}
  write(d/'calibration.json',cfg)
  radio={'interface':'serial','port':os.ttyname(slave),'baud_rate':115200,'protocol':'raw_uart'}
  links={'version':1,'av_bay':radio}
  if role=='primary':links['fill_box']={'interface':'serial','port':str(d/'absent-fill-fixture'),'baud_rate':115200,'protocol':'raw_uart'}
  write(d/'comms.json',links)
  env={**os.environ,'GS_RRLS_ROLE':role,'GS_SIM_MODE':'0','GS_SIMULATED_SERIAL_PTY':'1','GS_ETHERNET_AUTOCONFIG':'off','GS_BIND_ADDRESS':f'127.0.0.1:{port}','GS_HTTPS_BIND_ADDRESS':'off','GS_VIDEO_MANAGED':'0','GS_VIDEO_DISCOVERY':'0','GS_STATION_DISCOVERY_BIND':'127.0.0.1:0','GS_STATION_PAIRING_FILE':str(d/'pairing.json'),'GS_USERS_FILE':str(d/'users.json'),'GS_LOADCELL_CALIBRATION_PATH':str(d/'calibration.json'),'GS_MISSION_SETTINGS_PATH':str(d/'history.json'),'GS_GSE_CONFIG':str(d/'gse.json'),'GS_GSE_QUALIFICATION':str(d/'qualification.json'),'GS_CHECKLISTS_FILE':str(d/'checklists.json'),'GS_FILL_TARGETS_CONFIG':str(d/'targets.json'),'GS_FLIGHT_SETUP_CONFIG':str(d/'flight.json'),'GS_CALIBRATION_CONFIG_PATH':str(d/'calibration-layout.json'),'GS_COMMS_LINK_CONFIG':str(d/'comms.json'),'GS_RRLS_COMMS_LINK_CONFIG':str(d/'comms.json'),'GS_LOG_DIR':str(d/'logs'),'GS_DUMMY_RADIO_LOG':'0'}
  stations.append({'id':identity,'dir':d,'port':port,'master':m,'env':env})
 threading.Thread(target=pump,daemon=True).start()
 phase('boot')
 for s in stations:start(s)
 wait_for(lambda:all(request(s,'/api/mission-settings')['enabled'] for s in stations),30)
 for fd in slaves:os.close(fd)
 slaves.clear();passed('both real processes boot and expose enabled settings API')
 phase('initial_sync')
 wait_for(lambda:slope(stations[1])==1 and request(stations[1],'/api/calibration')['full_mass_kg']==17)
 passed('RRLS imports primary calibration over actual raw-UART/mesh/actor path')
 phase('offline_concurrent_edits');linked.clear();time.sleep(1)
 calibrate(stations[0],2);calibrate(stations[1],3);time.sleep(1.5)
 assert slope(stations[0])==2 and slope(stations[1])==3
 passed('API edits on disconnected peers remain local and durable')
 phase('conflict_reconnect');linked.set()
 wait_for(lambda:all(request(s,'/api/mission-settings')['conflicts']>=1 for s in stations))
 conflict=request(stations[0],'/api/mission-settings/conflicts')['calibration/ch1']
 selected=next(h['value'] for h in conflict['heads'] if h['value']['calibration']['linear']['m']==3)
 body={'key':'calibration/ch1','expected':conflict,'value':selected}
 assert request(stations[0],'/api/mission-settings/resolve',body)=={'resolved':True}
 request(stations[0],'/api/mission-settings/resolve',body,409)
 passed('actual HTTP conflict resolution checks exact reviewed heads and rejects stale replay')
 phase('resolved_sync');wait_for(lambda:all(slope(s)==3 and request(s,'/api/mission-settings')['conflicts']==0 for s in stations))
 passed('both actors apply the selected calibration after reconnection')
 phase('rrls_restart');stop(stations[1]);start(stations[1])
 wait_for(lambda:request(stations[1],'/api/mission-settings')['enabled'] and slope(stations[1])==3,30)
 passed('RRLS process restart preserves selected calibration and causal history')
 state['phase']='passed';save();print('PASS two actual backend processes:',', '.join(state['checks']))
except Exception as error:
 state.update(phase='failed',error=str(error));save();raise
finally:
 stopped.set()
 for s in stations:stop(s)
 for fd in slaves:
  try:os.close(fd)
  except OSError:pass
 for s in stations:os.close(s['master'])
 for f in handles:f.close()
 # Ephemeral application trust is never copied into source or evidence archives.
 for s in stations:(s['dir']/'seed').unlink(missing_ok=True)
