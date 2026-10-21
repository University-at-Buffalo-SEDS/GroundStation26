#!/usr/bin/env python3
"""Provision persistent application identity for a trusted GroundStation/RRLS pair."""
import argparse,json,os,uuid
from pathlib import Path
from urllib.parse import urlsplit

def origin(value):
    p=urlsplit(value)
    if p.scheme not in ('http','https') or not p.hostname or p.username or p.password or p.path not in ('','/') or p.query or p.fragment:
        raise argparse.ArgumentTypeError('use an HTTP(S) origin, without credentials, path or query')
    port=p.port
    host=p.hostname.lower()
    host=f'[{host}]' if ':' in host else host.encode('idna').decode('ascii')
    suffix=f':{port}' if port and port != (443 if p.scheme=='https' else 80) else ''
    return f'{p.scheme}://{host}{suffix}'

def provision(output,primary_url,rrls_url):
    # Never overwrite an installation identity: regenerating it invalidates logins.
    output=Path(output)
    if output.exists(): raise ValueError('output directory already exists; use a new private directory')
    output.mkdir(mode=0o700,parents=True)
    seed=os.urandom(32);installation=str(uuid.uuid4())
    for station,url,peer,peer_url in [('primary',primary_url,'rrls',rrls_url),('rrls',rrls_url,'primary',primary_url)]:
        directory=output/station;directory.mkdir(mode=0o700)
        fd=os.open(directory/'identity.seed',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'wb') as file:file.write(seed)
        p=urlsplit(url)
        config={'version':1,'installation_id':installation,'station_id':station,'signing_seed_file':'identity.seed','scheme':p.scheme,'port':p.port or (443 if p.scheme=='https' else 80),'peers':[{'id':peer,'role':peer,'urls':[peer_url]}]}
        fd=os.open(directory/'pairing.json',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'w') as file:json.dump(config,file,indent=2);file.write('\n')
    return output

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output',type=Path,required=True,help='new private directory for the two installation bundles')
    p.add_argument('--primary-url',type=origin,required=True,help='primary station origin visible to native apps, including proxy port')
    p.add_argument('--rrls-url',type=origin,required=True,help='RRLS origin visible to native apps, including proxy port')
    a=p.parse_args()
    try:provision(a.output,a.primary_url,a.rrls_url)
    except (ValueError,OSError) as e:p.error(str(e))
    print('Created primary/ and rrls/ pairing bundles. Install each only on its corresponding trusted station.')
    print('Set GS_STATION_PAIRING_FILE to the installed pairing.json path and restart each service.')
    print('Provision the same username on both stations; local permissions remain authoritative. Sign in again once to issue a paired session.')
if __name__=='__main__':main()
