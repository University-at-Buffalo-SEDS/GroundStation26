#!/usr/bin/env python3
"""Build inert host bridges from the actual sibling AB/VB recovery C fixtures.

No hardware, firmware build, downloads, or source edits. This opt-in test needs
both board repositories and their already configured SDK/LaunchCore sources.
"""
import importlib.util, os, pathlib, re, shutil, subprocess, sys
workspace=pathlib.Path(__file__).resolve().parents[3]
out=pathlib.Path(sys.argv[1]);out.mkdir(parents=True,exist_ok=True)
for short,folder in [('ab','ActuatorBoard26'),('vb','ValveBoard26')]:
 root=workspace/folder
 sys.path.insert(0,str(root/'tests'))
 for name in ['native_sdk','test_no_telemetry']:sys.modules.pop(name,None)
 spec=importlib.util.spec_from_file_location('actual_'+short,root/'tests/test_recovery_launch_exclusion.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
 from native_sdk import configured_dependency_source
 cache=(root/'build/Release_Script/CMakeCache.txt').read_text()
 sdk=pathlib.Path(re.search(r'^SEDSNET_DIR:PATH=(.+)$',cache,re.M).group(1))
 os.environ.pop('SDK_FREE_LAUNCHCORE_SOURCE',None)
 lc=configured_dependency_source(root,'sedslaunchcore','SDK_FREE_LAUNCHCORE_SOURCE')
 os.environ['SEDSNET_TEST_SOURCE']=str(sdk);os.environ['SDK_FREE_LAUNCHCORE_SOURCE']=str(lc);os.environ['RECOVERY_LAUNCH_TEST_SOURCE']=str(root)
 harness=out/(short+'_bridge.c')
 harness.write_text(('#define ACTUAL_AB_BRIDGE 1\n' if short=='ab' else '')+'#define main fixture_original_main\n#include "'+str(root/'tests/fixtures/recovery_launch_exclusion/harness.c')+'"\n#undef main\n'+r'''
static uint8_t bridge_prepare_outputs(uint8_t desired){assert(state_mutex->locked&&desired<=1);return 1;}
int main(void){
 scenario="sdk_bridge";image_path="unused";memset(image.nvm,255,NVM_SIZE);
 launchcore_storage_set_driver(&launchcore_board_storage_driver);seed_control("seed_healthy");fixture=false;
 board_abort_latch_init();(void)persistent_store_boot_begin();(void)board_recovery_boot_generation();board_bootstrap_storage_probe_boot();
 assert(persistent_store_runtime_start()==0);thread_present=true;TX_BYTE_POOL pool={0};assert(thread_comm_init(&pool)==0);
 board_recovery_bind_owner(read_outputs,bridge_prepare_outputs,finish_outputs);assert(board_bootstrap_storage_bind_owner(stop_owner));
 int cookie=0;SedsRouter*r=(SedsRouter*)&cookie;assert(flight_state_cache_init(r)==SEDS_OK);board_recovery_transport_set_router(r);
 assert(thread_comm_set_flight_state(1)==TX_SUCCESS);assert(audit_accept_launch()==TX_SUCCESS);audit_launch_phase("completed");
 puts("READY");fflush(stdout);
 unsigned long long timestamp,network_now;char hex[65];
 while(scanf("%llu %llu %64s",&network_now,&timestamp,hex)==3){
  uint8_t body[32];assert(strlen(hex)==64);for(unsigned i=0;i<32;i++){unsigned byte;assert(sscanf(hex+i*2,"%2x",&byte)==1);body[i]=(uint8_t)byte;}
  now=network_now;responses=0;
  if(body[0]==255){
#ifdef ACTUAL_AB_BRIDGE
   thread_comm_msg_t command={.cmd=body[1],.timestamp_ms=timestamp};
#else
   thread_comm_msg_t command=body[1];
#endif
   memset(response,0,40);response[0]=(uint8_t)thread_comm_admit_network(command,timestamp);
  }else{request(r,body,(uint64_t)timestamp);main_service();assert(responses);}
  for(unsigned i=0;i<40;i++)printf("%02x",response[i]);putchar('\n');fflush(stdout);
 }
 return 0;
}
''')
 real_run=subprocess.run
 def compile_bridge(args,*a,**kw):
  if isinstance(args,list) and args[0]=='cc':
   args=args.copy();old=str(root/'tests/fixtures/recovery_launch_exclusion/harness.c');args[args.index(old)]=str(harness);args[1:1]=['-Wno-attributes','-Wno-unused-command-line-argument']
  return real_run(args,*a,**kw)
 subprocess.run=compile_bridge
 cls=module.RecoveryLaunchExclusionTests
 try:
  cls.setUpClass();shutil.copy2(cls.exe,out/short)
 finally:
  subprocess.run=real_run
  if hasattr(cls,'tmp'):cls.tearDownClass()
 print(short, out/short)
