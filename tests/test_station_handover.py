#!/usr/bin/env python3
"""Exercise paired HTTP sessions on isolated loopback backends; no output commands."""
from pathlib import Path
import argparse,base64,hashlib,hmac,importlib.util,json,os,socket,subprocess,tempfile,time,urllib.request,urllib.error,urllib.parse

def port():
 with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
def request(origin,path,body=None,token=None):
 headers={'Content-Type':'application/json'}
 if token:headers['Authorization']='Bearer '+token
 req=urllib.request.Request(origin+path,data=None if body is None else json.dumps(body).encode(),headers=headers)
 try:
  with urllib.request.urlopen(req,timeout=3) as r:
   raw=r.read();return r.status,json.loads(raw) if raw else None
 except urllib.error.HTTPError as e:return e.code,None

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary',type=Path,required=True);p.add_argument('--pair-script',type=Path,default=Path(__file__).parents[1]/'backend/tools/pair_stations.py');a=p.parse_args();binary=a.binary.resolve()
 spec=importlib.util.spec_from_file_location('pair',a.pair_script);pair=importlib.util.module_from_spec(spec);spec.loader.exec_module(pair)
 with tempfile.TemporaryDirectory(prefix='paired-station-test-') as temporary:
  root=Path(temporary);origins={s:f'http://127.0.0.1:{port()}' for s in ['primary','rrls']};pair.provision(root/'pair',origins['primary'],origins['rrls'])
  salt=os.urandom(16);verifier=hashlib.pbkdf2_hmac('sha256',b'test-only-password',salt,120000)
  user={'username':'operator','password':{'algorithm':'pbkdf2_sha256','iterations':120000,'salt_b64':base64.b64encode(salt).decode(),'hash_b64':base64.b64encode(verifier).decode()},'roles':[],'permissions':{'view_data':True,'send_commands':True},'command_access':{'allowed_commands':['EnableIMU']},'calibration_access':{'view':True,'edit':False}}
  processes=[];logs=[];contexts=[]
  try:
   for station in origins:
    directory=root/station;directory.mkdir();(directory/'frontend/dist/public').mkdir(parents=True);(directory/'frontend/dist/public/index.html').write_text('<html>paired fixture</html>')
    config={'version':1,'av_bay':{'interface':'serial','port':'/missing-pair-test-radio','baud_rate':115200,'protocol':'raw_uart'}}
    if station=='primary':config['fill_box']=config['av_bay'].copy()
    (directory/'comms.json').write_text(json.dumps(config));(directory/'users.json').write_text(json.dumps({'version':1,'users':[user]}))
    env={k:v for k,v in os.environ.items() if not k.startswith('GS_')};env.update(GS_RRLS_ROLE=station,GS_COMMS_LINK_CONFIG=str(directory/'comms.json'),GS_RRLS_COMMS_LINK_CONFIG=str(directory/'comms.json'),GS_USERS_FILE=str(directory/'users.json'),GS_BIND_ADDRESS=origins[station].removeprefix('http://'),GS_HTTPS_BIND_ADDRESS='off',GS_VIDEO_MANAGED='0',GS_VIDEO_DISCOVERY='0',GS_STATION_PAIRING_FILE=str(root/'pair'/station/'pairing.json'),GS_STATION_DISCOVERY_BIND='127.0.0.1:0')
    log=(directory/'backend.log').open('w');logs.append(log);proc=subprocess.Popen([str(binary)],cwd=directory,env=env,stdout=log,stderr=subprocess.STDOUT);processes.append(proc);contexts.append((directory,env,log))
    deadline=time.monotonic()+20
    while True:
     if proc.poll() is not None:raise AssertionError((directory/'backend.log').read_text())
     try:
      code,status=request(origins[station],'/api/auth/session')
      if code==200:break
     except OSError:pass
     if time.monotonic()>deadline:raise AssertionError('startup timeout')
     time.sleep(.1)
    assert status['station_role']==station
   code,c=request(origins['primary'],'/api/auth/challenge',{'username':'operator'});assert code==200
   client_nonce=base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip('=')
   message={'username':'operator','challenge_id':c['challenge_id'],'server_nonce_b64':c['server_nonce_b64'],'client_nonce_b64':client_nonce,'remember_me':True}
   proof=base64.urlsafe_b64encode(hmac.new(verifier,json.dumps(message,separators=(',',':')).encode(),hashlib.sha256).digest()).decode().rstrip('=')
   code,login=request(origins['primary'],'/api/auth/login',{'username':'operator','challenge_id':c['challenge_id'],'client_nonce_b64':client_nonce,'remember_me':True,'proof_b64':proof});assert code==200
   token=login['token'];assert token.startswith('paired1.')
   for station in origins:
    code,status=request(origins[station],'/api/auth/session',token=token);assert code==200 and status['authenticated'] and status['username']=='operator';assert status['allowed_commands']==['EnableIMU'];assert not status['can_edit_calibration']
   def restart(index):
    processes[index].terminate();processes[index].wait(timeout=10)
    directory,env,log=contexts[index]
    processes[index]=subprocess.Popen([str(binary)],cwd=directory,env=env,stdout=log,stderr=subprocess.STDOUT)
    deadline=time.monotonic()+20
    while True:
     try:
      result=request(origins[['primary','rrls'][index]],'/api/auth/session',token=token)
      if result[0]==200:return result[1]
     except OSError:pass
     if time.monotonic()>deadline:raise AssertionError('restart timeout')
     time.sleep(.1)
   assert restart(0)['authenticated'] and restart(1)['authenticated'],'paired login must survive service restarts'
   target=root/'rrls/users.json';account=json.loads(target.read_text());account['users'][0]['roles']=['admin'];target.write_text(json.dumps(account))
   code,status=request(origins['rrls'],'/api/auth/session',token=token);assert code==200;assert 'admin' not in status['roles'];assert status['allowed_commands']==['EnableIMU'];assert not status['can_edit_calibration']
   account['users'][0]['roles']=[];account['users'][0]['command_access']={'allowed_commands':['OtherCommand']};target.write_text(json.dumps(account))
   code,status=request(origins['rrls'],'/api/auth/session',token=token);assert code==200 and status['allowed_commands']==['__no_paired_commands__'],'disjoint grants must not become unrestricted'
   account['users'][0]['disabled']=True;target.write_text(json.dumps(account));assert not request(origins['rrls'],'/api/auth/session',token=token)[1]['authenticated']
   account['users'][0]['disabled']=False;target.write_text(json.dumps(account))
   assert request(origins['rrls'],'/api/auth/logout',{},token)[0] in (200,204)
   assert not restart(1)['authenticated'],'logout must survive a service restart'
   # Query-string tokens use the same canonical form as Authorization headers.
   # A whitespace variant must not bypass the paired-token revocation path.
   account['users'][0]['roles']=['admin'];target.write_text(json.dumps(account))
   parsed=urllib.parse.urlsplit(origins['rrls'])
   path='/ws?token='+urllib.parse.quote(' '+token+' ')
   key=base64.b64encode(os.urandom(16)).decode()
   with socket.create_connection((parsed.hostname,parsed.port),timeout=3) as stream:
    stream.sendall(f'GET {path} HTTP/1.1\r\nHost: {parsed.netloc}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n'.encode())
    code=int(stream.recv(512).split(b' ',2)[1])
   assert code in (401,403),f'revoked token spelling must not reopen a WebSocket (status {code})'

   assert request(origins['primary'],'/api/auth/logout',{},token)[0] in (200,204)
   assert not request(origins['primary'],'/api/auth/session',token=token)[1]['authenticated']
   print('PASS: independent primary/RRLS HTTP identities, one login across both, local permission ceiling, no admin escalation, disabled account rejection, and persistent logout tombstones; no output commands sent')
  finally:
   for proc in processes:proc.terminate()
   for proc in processes:
    try:proc.wait(timeout=10)
    except subprocess.TimeoutExpired:proc.kill();proc.wait()
   for log in logs:log.close()
if __name__=='__main__':main()
