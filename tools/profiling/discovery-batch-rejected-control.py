import pathlib,json,subprocess,shutil,os,hashlib
R=pathlib.Path('/scratch/keyed-discovery-cpu');binary=R/'cv-hybrid';assert hashlib.sha256(binary.read_bytes()).digest()!=hashlib.sha256((R/'cv-after').read_bytes()).digest()
shutil.copyfile(binary,R/'runtime/cv');(R/'runtime/cv').chmod(0o755)
env=dict(os.environ,LC_ALL='C',TZ='UTC',RAYON_NUM_THREADS='1',CLUSTERVISION_MAX_STALE_SECS='3600')
for case in ['changed','warm']:
 root=R/case;command=[str(R/'runtime/cv'),'ls','--json','--harness','claude','--limit','100000']
 if case=='warm':command.append('--fresh')
 args=['python3',str(R/'after/tools/profiling/cachegrind.py'),'--input',str(root/'home'),'--harness-home',str(root/'home'),'--seed-catalog',str(root/'seed'),'--catalog-home',str(root/'catalog'),'--output','bytes',str(root/'hybrid'),'--',*command]
 with (root/'hybrid-run.log').open('w') as f:subprocess.run(args,env=env,stdout=f,stderr=subprocess.STDOUT,check=True)
 shutil.rmtree(root/'catalog');s=root/'hybrid/summary.json';d=json.loads(s.read_text());d['build']=json.loads((R/'build.json').read_text());s.write_text(json.dumps(d,indent=2)+'\n')
 for label in ['before','after']:
  with (root/f'{label}-vs-hybrid.json').open('w') as f:subprocess.run(['python3',str(R/'after/tools/profiling/compare.py'),str(root/label/'summary.json'),str(s)],stdout=f,check=True)
 print(case,d['Ir'],flush=True)
