#!/usr/bin/env python3
"""Exercise two real backends against a three-radio PTY broadcast model.

Requires the built HITL backend and the Python sedsnet 4.x wheel. No hardware,
operator commands, or existing credentials/data are used. This models serial
broadcast delivery, not RF propagation or modem collision/retry timing.
"""
import argparse, json, os, pathlib, pty, selectors, signal, socket, struct
import subprocess, tempfile, time, tty, urllib.request, zlib, sys
import sedsnet as seds


def frame(command, payload):
    return (b'\xa6\x5b' if command else b'\xa5\x5a')+struct.pack('<H',len(payload))+payload


def take(buf):
    result=[]
    while len(buf)>=4:
        if buf[:2] not in (b'\xa5\x5a',b'\xa6\x5b',b'\xa7\x7a'):
            del buf[0];continue
        n=int.from_bytes(buf[2:4],'little')
        if n<1 or n>4096:del buf[0];continue
        if len(buf)<4+n:break
        result.append((buf[0]==0xa6,bytes(buf[4:4+n])))
        del buf[:4+n]
    return result


def free_port():
    with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=pathlib.Path,required=True)
    args=parser.parse_args();binary=args.binary.resolve()
    ep=seds.endpoint_identity('GROUND_STATION');ty=seds.data_type_identity('GPS_DATA')
    if not seds.endpoint_exists(ep):seds.register_endpoint(ep,'GROUND_STATION')
    if not seds.data_type_exists(ty):seds.register_data_type(ty,'GPS_DATA',True,3,1,0,[ep],priority=255)
    peers={};procs=[];logs=[];selector=selectors.DefaultSelector();buffers={};
    with tempfile.TemporaryDirectory(prefix='rrls-serial-mesh-') as directory:
        base=pathlib.Path(directory)
        try:
            for role in ('primary','rrls'):
                work=base/role;work.mkdir();public=work/'frontend/dist/public';public.mkdir(parents=True)
                (public/'index.html').write_text('<html>RRLS serial smoke fixture</html>')
                master,slave=pty.openpty();tty.setraw(slave);port=free_port()
                config={'version':1,'av_bay':{'interface':'serial','port':os.ttyname(slave),'baud_rate':0 if sys.platform=='darwin' else 115200,'protocol':'raw_uart'},'fill_box':{'interface':'serial','port':str(work/'absent-fill-uart'),'protocol':'raw_uart'}}
                cfg=work/'comms.json';cfg.write_text(json.dumps(config));log=(work/'backend.log').open('w');logs.append(log)
                env=os.environ.copy();env.update(GS_HTTPS_BIND_ADDRESS='off',GS_RRLS_ROLE=role,GS_BIND_ADDRESS=f'127.0.0.1:{port}',GS_COMMS_LINK_CONFIG=str(cfg),GS_USERS_FILE=str(work/'users.json'),GS_VIDEO_MANAGED='0',GROUNDSTATION_TIMESYNC='0')
                if role=='primary':
                    # Match systemd's ordinary launch: no simulator-only role
                    # override may conceal disabled primary recovery startup.
                    env.pop('GS_RRLS_ROLE',None)
                process=subprocess.Popen([str(binary)],cwd=work,env=env,stdout=log,stderr=subprocess.STDOUT);procs.append(process)
                os.set_blocking(master,False);selector.register(master,selectors.EVENT_READ,role);buffers[role]=bytearray();peers[role]=(master,slave,port)
            def status(role):
                with urllib.request.urlopen(f'http://127.0.0.1:{peers[role][2]}/api/rrls/status',timeout=1) as response:return json.load(response)
            # Pump startup traffic while both HTTP servers initialize.
            started=time.monotonic();next_gps=started;next_window=started;ready=False
            checkpoint={};uplinks=0;rrls_wire_frames=0
            while time.monotonic()-started<23:
                now=time.monotonic();elapsed=now-started
                direct=elapsed<5 or elapsed>=12
                relay_connected=elapsed<19
                if any(p.poll() is not None for p in procs):raise AssertionError('backend exited early')
                if now>=next_window:
                    packet=frame(True,b'RS\x01\x01\x01\x08\x00\x00\x00')
                    if relay_connected:os.write(peers['rrls'][0],packet)
                    if direct:os.write(peers['primary'][0],packet)
                    next_window=now+.1
                if now>=next_gps:
                    pkt=seds.make_packet(ty,'RF',[ep],int(time.time()*1000),struct.pack('<fff',43.,-78.,123.))
                    packet=frame(False,pkt.pack())
                    if relay_connected:os.write(peers['rrls'][0],packet)
                    if direct:os.write(peers['primary'][0],packet)
                    next_gps=now+.2
                for key,_ in selector.select(.01):
                    role=key.data
                    try:data=os.read(key.fd,65536)
                    except (BlockingIOError,OSError):continue
                    buffers[role].extend(data)
                    for command,payload in take(buffers[role]):
                        if role=='rrls':
                            # Check the actual serial bytes, not the worker's
                            # accepted-packet log (observer drops return Ok).
                            # RRLS may emit mesh control/relay envelopes only;
                            # it must never originate an unwrapped SDK packet.
                            assert command and payload[:3]==b'RR\x01', ('RRLS originated SDK traffic',payload[:32].hex())
                            rrls_wire_frames+=1
                        wire=frame(command,payload)
                        other='rrls' if role=='primary' else 'primary'
                        if relay_connected:os.write(peers[other][0],wire)
                        if payload[:3]==b'RR\x01' and len(payload)>=8:
                            assert zlib.crc32(payload[:-4])==int.from_bytes(payload[-4:],'little')
                            if role=='rrls' and payload[3] in (5,6):uplinks+=1
                if not ready:
                    try:
                        first=status('rrls');primary=status('primary')
                    except OSError:pass
                    else:
                        assert first['role']=='rrls' and primary['role']=='primary', {'rrls':first,'primary':primary}
                        with urllib.request.urlopen(f'http://127.0.0.1:{peers["rrls"][2]}/',timeout=1) as response:assert response.status==200
                        ready=True
                for at,name in [(4,'standby'),(11,'relaying'),(17,'restored'),(18,'quiet'),(21,'rrls_disconnected')]:
                    if ready and elapsed>=at and name not in checkpoint:
                        checkpoint[name]={'primary':status('primary'),'rrls':status('rrls')}
            assert ready and len(checkpoint)==5,checkpoint
            assert rrls_wire_frames>0,'RRLS wire assertion had no observations'
            assert uplinks>0,"no recovery uplink frames reached the rocket model"
            assert not checkpoint['standby']['rrls']['forwarding'],checkpoint
            assert not checkpoint['rrls_disconnected']['primary']['forwarding'],checkpoint
            assert checkpoint['rrls_disconnected']['primary']['direct_age_ms']<1000,checkpoint
            assert checkpoint['standby']['rrls']['forwarded_frames']==0,checkpoint
            assert checkpoint['relaying']['primary']['forwarding'] and checkpoint['relaying']['rrls']['forwarding'],checkpoint
            assert checkpoint['relaying']['rrls']['forwarded_frames']>5,checkpoint
            assert not checkpoint['restored']['primary']['forwarding'] and not checkpoint['restored']['rrls']['forwarding'],checkpoint
            assert checkpoint['quiet']['rrls']['forwarded_frames']==checkpoint['restored']['rrls']['forwarded_frames'],checkpoint
            print(json.dumps({'passed':True,'checkpoints':checkpoint,'recovery_uplink_frames':uplinks,
                             'rrls_wire_frames_checked':rrls_wire_frames,
                             'scope':'serial relay failover and RRLS wire ownership; excludes UI/control parity, sensor acquisition and physical RF'},indent=2))
        except Exception:
            for log in logs:log.flush();print(pathlib.Path(log.name).read_text()[-12000:])
            raise
        finally:
            for process in procs:
                if process.poll() is None:process.send_signal(signal.SIGINT)
            for process in procs:
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:process.kill();process.wait()
            for master,slave,_ in peers.values():os.close(master);os.close(slave)
            for log in logs:log.close()
            selector.close()

if __name__=='__main__':main()
